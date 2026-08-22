import { Agent as WorkerAgent, Transport as WorkerTransport } from "nanocodex/browser";
import { Agent as InlineAgent, Transport as InlineTransport } from "nanocodex/host";
import type {
  AgentEvent,
  DefaultAgent,
  EventWatcher,
  Turn,
  TurnResult,
} from "nanocodex/host";

type RuntimeKind = "inline" | "worker";

type BenchmarkOptions = {
  endpoint: string;
  samples: number;
  declaredSourceCommit: string;
};

type ServerTiming = {
  id: string;
  requestReceivedEpochUs: number;
  responseDeltaSentEpochUs: number;
};

type RawSample = {
  id: string;
  ordinal: number;
  connection: "initial" | "reused";
  client_submit_epoch_us: number;
  server_request_received_epoch_us: number;
  server_delta_sent_epoch_us: number;
  client_raw_api_event_received_epoch_us: number;
  client_typed_delta_received_epoch_us: number;
  client_turn_result_received_epoch_us: number;
  submit_to_raw_api_event_ns: number;
  raw_api_event_to_typed_event_ns: number;
  server_request_to_delta_send_ns: number;
  submit_to_typed_event_ns: number;
  submit_to_turn_result_ns: number;
};

type Distribution = {
  n: number;
  min_ns: number;
  p50_ns: number;
  p95_ns: number;
  max_ns: number;
  mean_ns: number;
};

type RuntimeReport = {
  raw_samples: RawSample[];
  distributions: {
    submit_to_raw_api_event: Distribution;
    raw_api_event_to_typed_event: Distribution;
    server_request_to_delta_send: Distribution;
    submit_to_typed_event: Distribution;
    submit_to_turn_result: Distribution;
  };
};

export type BrowserModelLatencyReport = {
  schema_version: 1;
  implementation: "nanocodex-browser-model-latency";
  declared_source_commit: string;
  browser: { user_agent: string };
  clock: {
    timestamp_unit: "microseconds";
    duration_unit: "nanoseconds";
    browser: string;
    server: string;
    cross_realm_timestamps: string;
    boundaries: Record<string, string>;
  };
  sample_count_per_runtime: number;
  runtimes: Record<RuntimeKind, RuntimeReport>;
  correctness_gates: {
    exact_final_output: true;
    typed_event_order: true;
    incremental_follow_on: true;
    historical_fork_boundary: true;
    cancellation: true;
    graceful_shutdown: true;
    loopback_only: true;
  };
};

declare global {
  interface Window {
    runNanocodexBrowserModelLatency(
      options: BenchmarkOptions,
    ): Promise<BrowserModelLatencyReport>;
  }
}

window.runNanocodexBrowserModelLatency = runBenchmark;

async function runBenchmark(options: BenchmarkOptions): Promise<BrowserModelLatencyReport> {
  validateOptions(options);
  const inline = await runRuntime("inline", options);
  const worker = await runRuntime("worker", options);
  return {
    schema_version: 1,
    implementation: "nanocodex-browser-model-latency",
    declared_source_commit: options.declaredSourceCommit,
    browser: { user_agent: navigator.userAgent },
    clock: {
      timestamp_unit: "microseconds",
      duration_unit: "nanoseconds",
      browser: "Math.round((performance.timeOrigin + performance.now()) * 1000)",
      server: "Math.round((performance.timeOrigin + performance.now()) * 1000) in Node.js",
      cross_realm_timestamps: "retained for audit only and never subtracted; every reported duration uses one realm's clock",
      boundaries: {
        client_submit: "immediately before agent.turn.prompt",
        server_request_received: "entry to the ws message callback with the complete request frame",
        server_delta_sent: "immediately before timestamp insertion and WebSocket.send",
        client_raw_api_event_received: "entry to the AgentEvent api.event listener for the delta frame",
        client_typed_delta_received: "entry to the AgentEvent assistant.delta listener",
        client_turn_result_received: "first promise continuation after turn.result resolves",
      },
    },
    sample_count_per_runtime: options.samples,
    runtimes: { inline, worker },
    correctness_gates: {
      exact_final_output: true,
      typed_event_order: true,
      incremental_follow_on: true,
      historical_fork_boundary: true,
      cancellation: true,
      graceful_shutdown: true,
      loopback_only: true,
    },
  };
}

async function runRuntime(
  runtime: RuntimeKind,
  options: BenchmarkOptions,
): Promise<RuntimeReport> {
  const turns = new Set<Turn>();
  const results = new Set<TurnResult>();
  const agents = new Set<DefaultAgent>();
  const rawSamples: RawSample[] = [];
  let tracker: TypedEventTracker | undefined;
  let firstResult: TurnResult | undefined;

  try {
    const agent = await createAgent(runtime, options.endpoint);
    agents.add(agent);
    tracker = new TypedEventTracker(agent);
    for (let ordinal = 0; ordinal < options.samples; ordinal += 1) {
      const id = `${runtime}-sample-${ordinal}`;
      const finalText = `${runtime.toUpperCase()}_SAMPLE_${ordinal}_OK`;
      const eventStart = tracker.length;
      const delta = tracker.expectDelta(id, finalText);
      const submittedUs = epochMicroseconds();
      const turn = agent.turn.prompt({
        input: `BENCH_${runtime.toUpperCase()}_SAMPLE_${ordinal}`,
      });
      turns.add(turn);
      const resultPromise = turn.result().then((result) => ({
        receivedUs: epochMicroseconds(),
        result,
      }));
      const [typedDelta, completed] = await Promise.all([delta, resultPromise]);
      invariant(completed.result.finalMessage === finalText, `${id} returned the wrong final text`);
      results.add(completed.result);
      await tracker.assertCompletedTurn(eventStart, id, finalText);
      rawSamples.push(sample(
        id,
        ordinal,
        submittedUs,
        typedDelta.rawEventReceivedUs,
        typedDelta.receivedUs,
        completed.receivedUs,
        typedDelta.server,
      ));
      turn.dispose();
      turns.delete(turn);
      if (ordinal === 0) {
        firstResult = completed.result;
      } else {
        completed.result.dispose();
        results.delete(completed.result);
      }
    }

    invariant(firstResult !== undefined, `${runtime} did not retain its first result`);
    const branch = await agent.session.fork({ at: firstResult });
    agents.add(branch);
    await checkedTurn(
      branch,
      `BENCH_${runtime.toUpperCase()}_FORK`,
      `${runtime.toUpperCase()}_FORK_OK`,
      `${runtime}-fork`,
      tracker,
      turns,
      results,
    );
    await shutdown(branch);
    agents.delete(branch);

    const cancelledEventStart = tracker.length;
    const cancelledId = `${runtime}-cancel`;
    const partialText = `${runtime.toUpperCase()}_CANCEL_PARTIAL`;
    const partial = tracker.expectDelta(cancelledId, partialText);
    const cancelled = agent.turn.prompt({
      input: `BENCH_${runtime.toUpperCase()}_CANCEL`,
    });
    turns.add(cancelled);
    await partial;
    await cancelled.cancel();
    let cancelledResultRejected = false;
    try {
      await cancelled.result();
    } catch {
      cancelledResultRejected = true;
    }
    invariant(cancelledResultRejected, `${runtime} cancellation resolved a TurnResult`);
    await tracker.assertCancelledTurn(cancelledEventStart, cancelledId, partialText);
    cancelled.dispose();
    turns.delete(cancelled);

    await checkedTurn(
      agent,
      `BENCH_${runtime.toUpperCase()}_RECOVER`,
      `${runtime.toUpperCase()}_RECOVERED_OK`,
      `${runtime}-recover`,
      tracker,
      turns,
      results,
    );

    firstResult.dispose();
    results.delete(firstResult);
    tracker.off();
    await shutdown(agent);
    agents.delete(agent);
    return runtimeReport(rawSamples);
  } finally {
    await cleanupRuntime(runtime, tracker, results, turns, agents);
  }
}

async function cleanupRuntime(
  runtime: RuntimeKind,
  tracker: TypedEventTracker | undefined,
  results: Set<TurnResult>,
  turns: Set<Turn>,
  agents: Set<DefaultAgent>,
): Promise<void> {
  const failures: unknown[] = [];
  try {
    tracker?.off();
  } catch (error) {
    failures.push(error);
  }
  for (const result of results) {
    try {
      result.dispose();
    } catch (error) {
      failures.push(error);
    }
  }
  for (const turn of turns) {
    try {
      await turn.cancel();
    } catch (error) {
      failures.push(error);
    }
    try {
      turn.dispose();
    } catch (error) {
      failures.push(error);
    }
  }
  const shutdowns = await Promise.allSettled([...agents].map(shutdown));
  for (const outcome of shutdowns) {
    if (outcome.status === "rejected") failures.push(outcome.reason);
  }
  if (failures.length > 0) {
    throw new AggregateError(failures, `${runtime} agent cleanup failed`);
  }
}

async function createAgent(runtime: RuntimeKind, endpoint: string): Promise<DefaultAgent> {
  const connection = {
    apiKey: "loopback-benchmark",
    websocketPreconnect: false,
    websocketUrl: endpoint,
    websocketWarmup: false,
  };
  const common = {
    instructions: "Return only the exact scripted benchmark token.",
    mcp: false as const,
    thinking: "none" as const,
  };
  if (runtime === "inline") {
    return InlineAgent.create({
      ...common,
      transport: InlineTransport.openAi(connection),
    });
  }
  return WorkerAgent.create({
    ...common,
    harness: false,
    transport: WorkerTransport.openAi(connection),
  });
}

async function checkedTurn(
  agent: DefaultAgent,
  prompt: string,
  finalText: string,
  benchmarkId: string,
  tracker: TypedEventTracker,
  turns: Set<Turn>,
  results: Set<TurnResult>,
): Promise<void> {
  const eventStart = tracker.length;
  const turn = agent.turn.prompt({ input: prompt });
  turns.add(turn);
  const result = await turn.result();
  results.add(result);
  invariant(result.finalMessage === finalText, `${prompt} returned the wrong final text`);
  await tracker.assertCompletedTurn(eventStart, benchmarkId, finalText);
  result.dispose();
  results.delete(result);
  turn.dispose();
  turns.delete(turn);
}

async function shutdown(agent: DefaultAgent): Promise<void> {
  const failures: unknown[] = [];
  try {
    await agent.session.shutdown();
  } catch (error) {
    failures.push(error);
  }
  try {
    agent.dispose();
  } catch (error) {
    failures.push(error);
  }
  if (failures.length > 0) {
    throw new AggregateError(failures, "browser agent shutdown failed");
  }
}

class TypedEventTracker {
  readonly events: Array<{
    benchmarkId?: string;
    event: AgentEvent;
    observedUs: number;
    text?: string;
  }> = [];
  readonly metadata = new Map<string, {
    rawEventReceivedUs: number;
    server: ServerTiming;
  }>();
  readonly waiters = new Map<string, {
    id: string;
    reject(error: Error): void;
    resolve(value: {
      rawEventReceivedUs: number;
      receivedUs: number;
      server: ServerTiming;
    }): void;
  }>();
  readonly watch: EventWatcher;

  constructor(agent: DefaultAgent) {
    this.watch = agent.events.watch({ includeAllSessions: true });
    this.watch.onEvent((event) => this.observe(event));
  }

  get length(): number {
    return this.events.length;
  }

  expectDelta(
    id: string,
    text: string,
  ): Promise<{
    rawEventReceivedUs: number;
    receivedUs: number;
    server: ServerTiming;
  }> {
    invariant(!this.waiters.has(text), `duplicate delta waiter for ${text}`);
    return new Promise((resolve, reject) => {
      this.waiters.set(text, { id, reject, resolve });
    });
  }

  async assertCompletedTurn(start: number, id: string, text: string): Promise<void> {
    await eventually(() => this.events.slice(start).some(({ event }) => event.type === "run.completed"));
    await settleEventDelivery();
    const events = this.events.slice(start);
    assertOneRequest(events, id);
    assertContiguousSequence(events, id);
    const raw = onlyIndex(events, (entry) => entry.benchmarkId === id, `${id} raw api.event`);
    const delta = onlyIndex(
      events,
      (entry) => entry.event.type === "assistant.delta",
      `${id} assistant.delta`,
    );
    const message = onlyIndex(
      events,
      (entry) => entry.event.type === "assistant.message",
      `${id} assistant.message`,
    );
    const completed = onlyIndex(
      events,
      (entry) => entry.event.type === "run.completed" || entry.event.type === "run.failed",
      `${id} terminal event`,
    );
    invariant(events[delta].text === text, `${id} assistant.delta text differed`);
    invariant(events[message].text === text, `${id} assistant.message text differed`);
    assertModelCall(events[delta], id);
    assertModelCall(events[message], id);
    invariant(events[completed].event.type === "run.completed", `${id} did not complete successfully`);
    invariant(completed === events.length - 1, `${id} emitted an event after its terminal`);
    invariant(
      !events.some(({ event }) => event.type === "tool.call" || event.type === "tool.result"),
      `${id} emitted a tool event`,
    );
    invariant(raw >= 0 && raw < delta, `${id} did not preserve api.event -> assistant.delta order`);
    invariant(delta < message && message < completed, `${id} did not preserve typed completion order`);
  }

  async assertCancelledTurn(start: number, id: string, partialText: string): Promise<void> {
    await eventually(() => this.events.slice(start).some(({ event }) => event.type === "run.failed"));
    await settleEventDelivery();
    const events = this.events.slice(start);
    assertOneRequest(events, id);
    assertContiguousSequence(events, id);
    const raw = onlyIndex(events, (entry) => entry.benchmarkId === id, `${id} raw api.event`);
    const delta = onlyIndex(
      events,
      (entry) => entry.event.type === "assistant.delta",
      `${id} assistant.delta`,
    );
    const failed = onlyIndex(
      events,
      (entry) => entry.event.type === "run.completed" || entry.event.type === "run.failed",
      `${id} terminal event`,
    );
    invariant(events[delta].text === partialText, `${id} partial assistant.delta text differed`);
    assertModelCall(events[delta], id);
    invariant(raw < delta, `${id} did not preserve api.event -> assistant.delta order`);
    invariant(delta >= 0 && delta < failed, "cancelled run.failed did not follow its typed partial delta");
    invariant(events[failed].event.payload.status === "cancelled", "run.failed did not report cancellation");
    invariant(events[failed].event.type === "run.failed", `${id} emitted the wrong terminal kind`);
    invariant(failed === events.length - 1, `${id} emitted an event after its terminal`);
    invariant(
      !events.some(({ event }) => event.type === "assistant.message"),
      `${id} emitted a completed assistant message`,
    );
  }

  off(): void {
    this.watch.off();
  }

  private observe(event: AgentEvent): void {
    const observedUs = epochMicroseconds();
    const timing = event.type === "api.event" ? serverTiming(event.payload.event) : undefined;
    if (timing) {
      this.metadata.set(timing.id, {
        rawEventReceivedUs: observedUs,
        server: timing,
      });
    }
    const text = typeof event.payload.text === "string" ? event.payload.text : undefined;
    this.events.push({
      ...(timing ? { benchmarkId: timing.id } : {}),
      event,
      observedUs,
      ...(text === undefined ? {} : { text }),
    });
    if (event.type !== "assistant.delta" || text === undefined) return;
    const waiter = this.waiters.get(text);
    if (!waiter) return;
    this.waiters.delete(text);
    const metadata = this.metadata.get(waiter.id);
    if (!metadata) {
      waiter.reject(new Error(`${waiter.id} assistant.delta arrived before its raw api.event`));
      return;
    }
    waiter.resolve({
      rawEventReceivedUs: metadata.rawEventReceivedUs,
      receivedUs: observedUs,
      server: metadata.server,
    });
  }
}

function serverTiming(value: unknown): ServerTiming | undefined {
  const event = typeof value === "string" ? parseJson(value) : value;
  if (!event || typeof event !== "object") return undefined;
  const benchmark = (event as Record<string, unknown>).benchmark;
  if (!benchmark || typeof benchmark !== "object") return undefined;
  const fields = benchmark as Record<string, unknown>;
  if (
    typeof fields.id !== "string"
    || typeof fields.request_received_epoch_us !== "number"
    || typeof fields.response_delta_sent_epoch_us !== "number"
  ) return undefined;
  return {
    id: fields.id,
    requestReceivedEpochUs: fields.request_received_epoch_us,
    responseDeltaSentEpochUs: fields.response_delta_sent_epoch_us,
  };
}

function parseJson(value: string): unknown {
  try {
    return JSON.parse(value);
  } catch {
    return undefined;
  }
}

function sample(
  id: string,
  ordinal: number,
  submittedUs: number,
  rawEventReceivedUs: number,
  typedDeltaUs: number,
  resultUs: number,
  server: ServerTiming,
): RawSample {
  invariant(server.id === id, `${id} received timing for ${server.id}`);
  invariant(rawEventReceivedUs >= submittedUs, `${id} raw api.event preceded prompt submission`);
  invariant(typedDeltaUs >= rawEventReceivedUs, `${id} typed delta preceded its raw api.event`);
  invariant(resultUs >= typedDeltaUs, `${id} TurnResult preceded its typed delta`);
  invariant(
    server.responseDeltaSentEpochUs >= server.requestReceivedEpochUs,
    `${id} server send preceded request receipt`,
  );
  return {
    id,
    ordinal,
    connection: ordinal === 0 ? "initial" : "reused",
    client_submit_epoch_us: submittedUs,
    server_request_received_epoch_us: server.requestReceivedEpochUs,
    server_delta_sent_epoch_us: server.responseDeltaSentEpochUs,
    client_raw_api_event_received_epoch_us: rawEventReceivedUs,
    client_typed_delta_received_epoch_us: typedDeltaUs,
    client_turn_result_received_epoch_us: resultUs,
    submit_to_raw_api_event_ns: toNanoseconds(rawEventReceivedUs - submittedUs),
    raw_api_event_to_typed_event_ns: toNanoseconds(typedDeltaUs - rawEventReceivedUs),
    server_request_to_delta_send_ns: toNanoseconds(
      server.responseDeltaSentEpochUs - server.requestReceivedEpochUs,
    ),
    submit_to_typed_event_ns: toNanoseconds(typedDeltaUs - submittedUs),
    submit_to_turn_result_ns: toNanoseconds(resultUs - submittedUs),
  };
}

function runtimeReport(rawSamples: RawSample[]): RuntimeReport {
  return {
    raw_samples: rawSamples,
    distributions: {
      submit_to_raw_api_event: distribution(rawSamples.map((entry) => entry.submit_to_raw_api_event_ns)),
      raw_api_event_to_typed_event: distribution(
        rawSamples.map((entry) => entry.raw_api_event_to_typed_event_ns),
      ),
      server_request_to_delta_send: distribution(
        rawSamples.map((entry) => entry.server_request_to_delta_send_ns),
      ),
      submit_to_typed_event: distribution(rawSamples.map((entry) => entry.submit_to_typed_event_ns)),
      submit_to_turn_result: distribution(rawSamples.map((entry) => entry.submit_to_turn_result_ns)),
    },
  };
}

function distribution(values: number[]): Distribution {
  invariant(values.length > 0, "latency distribution requires samples");
  const sorted = [...values].sort((left, right) => left - right);
  return {
    n: sorted.length,
    min_ns: sorted[0],
    p50_ns: percentile(sorted, 0.5),
    p95_ns: percentile(sorted, 0.95),
    max_ns: sorted[sorted.length - 1],
    mean_ns: Math.round(sorted.reduce((total, value) => total + value, 0) / sorted.length),
  };
}

function percentile(sorted: number[], quantile: number): number {
  return sorted[Math.max(0, Math.ceil(sorted.length * quantile) - 1)];
}

function assertOneRequest(
  entries: Array<{ event: AgentEvent }>,
  id: string,
): void {
  const requestIds = new Set(entries.map((entry) => entry.event.request_id));
  invariant(requestIds.size === 1, `${id} mixed events from more than one session`);
}

function assertContiguousSequence(entries: Array<{ event: AgentEvent }>, id: string): void {
  const sequence = entries.map((entry) => entry.event.seq);
  invariant(
    sequence.every((value, index) => index === 0 || value === sequence[index - 1] + 1),
    `${id} event sequence is not contiguous`,
  );
}

function onlyIndex<T>(entries: T[], predicate: (entry: T) => boolean, label: string): number {
  const matches = entries.flatMap((entry, index) => predicate(entry) ? [index] : []);
  invariant(matches.length === 1, `${label} count was ${matches.length}, expected one`);
  return matches[0];
}

function assertModelCall(entry: { event: AgentEvent }, id: string): void {
  invariant(entry.event.payload.model_call_index === 1, `${id} used the wrong model call index`);
}

async function settleEventDelivery(): Promise<void> {
  await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
}

async function eventually(predicate: () => boolean): Promise<void> {
  const deadline = performance.now() + 5_000;
  while (!predicate()) {
    if (performance.now() >= deadline) throw new Error("timed out waiting for typed event delivery");
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
}

function validateOptions(options: BenchmarkOptions): void {
  const endpoint = new URL(options.endpoint);
  invariant(endpoint.protocol === "ws:", "browser model benchmark requires ws loopback");
  invariant(
    endpoint.hostname === "127.0.0.1" || endpoint.hostname === "localhost" || endpoint.hostname === "[::1]",
    "browser model benchmark refuses a non-loopback endpoint",
  );
  invariant(Number.isSafeInteger(options.samples) && options.samples >= 2, "samples must be an integer >= 2");
  invariant(
    /^[0-9a-f]{40}$/.test(options.declaredSourceCommit),
    "declaredSourceCommit must be a full Git commit",
  );
}

function epochMicroseconds(): number {
  return Math.round((performance.timeOrigin + performance.now()) * 1_000);
}

function toNanoseconds(microseconds: number): number {
  return Math.round(microseconds * 1_000);
}

function invariant(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}
