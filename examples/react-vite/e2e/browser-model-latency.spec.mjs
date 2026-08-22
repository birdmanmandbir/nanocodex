import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";

import { test } from "@playwright/test";
import { WebSocketServer } from "ws";

const SAMPLE_COUNT = integerEnvironment("NANOCODEX_BROWSER_SAMPLES", 8, 2);
const REPOSITORY_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const SOURCE_COMMIT = git("rev-parse", "HEAD");
const SOURCE_TREE = git("rev-parse", "HEAD^{tree}");
const SOURCE_DIRTY = git("status", "--porcelain=v1", "--untracked-files=all").length > 0;

test("real Chromium reports inline and package-Worker model latency", async ({ page }) => {
  const server = await ScriptedResponsesServer.start(SAMPLE_COUNT);
  const external = [];
  const servedAssetPromises = [];
  page.on("request", (request) => {
    if (!isLoopback(request.url())) external.push(request.url());
  });
  page.on("websocket", (socket) => {
    if (!isLoopback(socket.url())) external.push(socket.url());
  });
  page.on("response", (response) => {
    const contentType = response.headers()["content-type"] ?? "";
    if (
      isLoopback(response.url())
      && (contentType.includes("javascript") || contentType.includes("application/wasm"))
    ) {
      servedAssetPromises.push(response.body().then((body) => ({
        path: new URL(response.url()).pathname,
        content_type: contentType,
        sha256: createHash("sha256").update(body).digest("hex"),
      })));
    }
  });

  try {
    await page.goto("/browser-model-latency.html");
    await page.waitForFunction(() => typeof window.runNanocodexBrowserModelLatency === "function");
    const report = await page.evaluate(
      ({ endpoint, samples, declaredSourceCommit }) => window.runNanocodexBrowserModelLatency({
        endpoint,
        samples,
        declaredSourceCommit,
      }),
      {
        endpoint: server.endpoint,
        samples: SAMPLE_COUNT,
        declaredSourceCommit: SOURCE_COMMIT,
      },
    );
    const servedAssets = deduplicateAssets(await Promise.all(servedAssetPromises));
    assert.ok(
      servedAssets.some((asset) => asset.content_type.includes("javascript")),
      "benchmark did not retain any served JavaScript asset hash",
    );
    assert.ok(
      servedAssets.some((asset) => asset.content_type.includes("application/wasm")),
      "benchmark did not retain its served WebAssembly asset hash",
    );
    report.provenance = {
      source_commit: SOURCE_COMMIT,
      source_tree: SOURCE_TREE,
      source_dirty: SOURCE_DIRTY,
      served_assets: servedAssets,
    };

    await server.waitForCleanup();
    server.assertComplete(report);
    assert.deepEqual(external, [], `benchmark attempted non-loopback requests: ${external.join(", ")}`);
    assert.equal(report.schema_version, 1);
    assert.equal(report.implementation, "nanocodex-browser-model-latency");
    assert.equal(report.declared_source_commit, SOURCE_COMMIT);

    await publishAfterServerClose(server, async () => {
      const encoded = `${JSON.stringify(report, null, 2)}\n`;
      const output = process.env.NANOCODEX_BROWSER_REPORT;
      if (output) {
        const path = resolve(output);
        await mkdir(dirname(path), { recursive: true });
        await writeFile(path, encoded, "utf8");
      }
      console.log(JSON.stringify(report));
    });
  } catch (error) {
    server.rethrowFailure();
    throw error;
  } finally {
    await server.close();
  }
});

test("listener close failure prevents benchmark report publication", async () => {
  const closeFailure = new Error("injected listener close failure");
  let verified = false;
  let published = false;
  const server = {
    close: async () => {
      throw closeFailure;
    },
    assertClosed: () => {
      verified = true;
    },
  };

  await assert.rejects(
    publishAfterServerClose(server, async () => {
      published = true;
    }),
    (error) => error === closeFailure,
  );
  assert.equal(verified, false);
  assert.equal(published, false);
});

class ScriptedResponsesServer {
  static async start(sampleCount) {
    const instance = new ScriptedResponsesServer(sampleCount);
    await new Promise((resolveListening, reject) => {
      instance.server.once("listening", resolveListening);
      instance.server.once("error", reject);
    });
    return instance;
  }

  constructor(sampleCount) {
    this.sampleCount = sampleCount;
    this.server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
    this.closePromise = undefined;
    this.failure = undefined;
    this.records = new Map();
    this.state = Object.fromEntries(["inline", "worker"].map((runtime) => [runtime, {
      cancelSocket: undefined,
      cancelSocketClosed: false,
      cancellationConnectionReplaced: false,
      cancelValidated: false,
      followOnValidated: false,
      forkValidated: false,
      recoveryValidated: false,
      samples: 0,
    }]));
    this.server.on("connection", (socket) => {
      socket.on("message", (data) => this.handle(socket, data));
      socket.on("close", () => {
        if (socket.cancelRuntime) this.state[socket.cancelRuntime].cancelSocketClosed = true;
      });
      socket.on("error", (error) => this.fail(error));
    });
    this.server.on("error", (error) => this.fail(error));
  }

  get endpoint() {
    const address = this.server.address();
    assert.equal(typeof address, "object");
    return `ws://127.0.0.1:${address.port}`;
  }

  handle(socket, data) {
    const requestReceivedUs = epochMicroseconds();
    try {
      const request = JSON.parse(data.toString("utf8"));
      const encoded = JSON.stringify(request.input);
      const runtime = encoded.includes("BENCH_INLINE_") ? "inline"
        : encoded.includes("BENCH_WORKER_") ? "worker"
          : undefined;
      assert.ok(runtime, `unrecognized benchmark request: ${encoded}`);
      if (encoded.includes(`BENCH_${runtime.toUpperCase()}_RECOVER`)) {
        this.handleRecovery(socket, runtime, request, encoded, requestReceivedUs);
      } else if (encoded.includes(`BENCH_${runtime.toUpperCase()}_CANCEL`)) {
        this.handleCancellation(socket, runtime, request, requestReceivedUs);
      } else if (encoded.includes(`BENCH_${runtime.toUpperCase()}_FORK`)) {
        this.handleFork(socket, runtime, request, encoded, requestReceivedUs);
      } else {
        this.handleSample(socket, runtime, request, encoded, requestReceivedUs);
      }
    } catch (error) {
      this.fail(error);
    }
  }

  handleSample(socket, runtime, request, encoded, requestReceivedUs) {
    const matches = [...encoded.matchAll(new RegExp(`BENCH_${runtime.toUpperCase()}_SAMPLE_(\\d+)`, "g"))];
    assert.equal(matches.length, 1, `${runtime} request did not contain one current sample marker`);
    const ordinal = Number(matches[0][1]);
    assert.equal(ordinal, this.state[runtime].samples, `${runtime} samples arrived out of order`);
    if (ordinal === 0) {
      assert.equal(request.previous_response_id, undefined);
    } else {
      assert.equal(request.previous_response_id, `${runtime}-response-${ordinal - 1}`);
      assert.equal(request.input.length, 1, `${runtime} follow-on did not send only its delta`);
      assert.ok(!encoded.includes(`BENCH_${runtime.toUpperCase()}_SAMPLE_${ordinal - 1}`));
      this.state[runtime].followOnValidated = true;
    }
    const id = `${runtime}-sample-${ordinal}`;
    const finalText = `${runtime.toUpperCase()}_SAMPLE_${ordinal}_OK`;
    this.sendCompleted(socket, {
      eventId: id,
      finalText,
      requestReceivedUs,
      responseId: `${runtime}-response-${ordinal}`,
    });
    this.state[runtime].samples += 1;
  }

  handleFork(socket, runtime, request, encoded, requestReceivedUs) {
    assert.equal(request.previous_response_id, undefined, `${runtime} historical fork leaked a response id`);
    assert.ok(encoded.includes(`BENCH_${runtime.toUpperCase()}_SAMPLE_0`));
    assert.ok(encoded.includes(`${runtime.toUpperCase()}_SAMPLE_0_OK`));
    assert.ok(!encoded.includes(`BENCH_${runtime.toUpperCase()}_SAMPLE_1`));
    assert.ok(!encoded.includes(`${runtime.toUpperCase()}_SAMPLE_1_OK`));
    this.state[runtime].forkValidated = true;
    this.sendCompleted(socket, {
      eventId: `${runtime}-fork`,
      finalText: `${runtime.toUpperCase()}_FORK_OK`,
      requestReceivedUs,
      responseId: `${runtime}-fork-response`,
    });
  }

  handleCancellation(socket, runtime, request, requestReceivedUs) {
    assert.equal(
      request.previous_response_id,
      `${runtime}-response-${this.sampleCount - 1}`,
      `${runtime} cancellation did not continue the retained response chain`,
    );
    assert.equal(this.state[runtime].cancelSocket, undefined, `${runtime} opened more than one cancellation socket`);
    this.state[runtime].cancelSocket = socket;
    socket.cancelRuntime = runtime;
    this.state[runtime].cancelValidated = true;
    this.sendDelta(socket, {
      eventId: `${runtime}-cancel`,
      finalText: `${runtime.toUpperCase()}_CANCEL_PARTIAL`,
      requestReceivedUs,
    });
  }

  handleRecovery(socket, runtime, request, encoded, requestReceivedUs) {
    assert.equal(
      this.state[runtime].cancelSocketClosed,
      true,
      `${runtime} recovery began before cancellation closed its in-flight socket`,
    );
    assert.notEqual(
      socket,
      this.state[runtime].cancelSocket,
      `${runtime} recovery reused the cancelled in-flight socket`,
    );
    this.state[runtime].cancellationConnectionReplaced = true;
    assert.equal(request.previous_response_id, undefined, `${runtime} recovery did not replay after cancellation`);
    assert.ok(encoded.includes(`BENCH_${runtime.toUpperCase()}_CANCEL`));
    assert.ok(encoded.includes(`BENCH_${runtime.toUpperCase()}_RECOVER`));
    assert.ok(encoded.includes("<turn_aborted>"));
    assert.ok(!encoded.includes(`${runtime.toUpperCase()}_CANCEL_PARTIAL`));
    this.state[runtime].recoveryValidated = true;
    this.sendCompleted(socket, {
      eventId: `${runtime}-recover`,
      finalText: `${runtime.toUpperCase()}_RECOVERED_OK`,
      requestReceivedUs,
      responseId: `${runtime}-recovered-response`,
    });
  }

  sendDelta(socket, { eventId, finalText, requestReceivedUs }) {
    const template = JSON.stringify({
      type: "response.output_text.delta",
      output_index: 0,
      content_index: 0,
      delta: finalText,
      benchmark: {
        id: eventId,
        request_received_epoch_us: requestReceivedUs,
        response_delta_sent_epoch_us: "__NANOCODEX_SEND_EPOCH_US__",
      },
    });
    const sentUs = epochMicroseconds();
    const payload = template.replace('"__NANOCODEX_SEND_EPOCH_US__"', String(sentUs));
    this.records.set(eventId, { requestReceivedUs, sentUs });
    socket.send(payload);
  }

  sendCompleted(socket, { eventId, finalText, requestReceivedUs, responseId }) {
    this.sendDelta(socket, { eventId, finalText, requestReceivedUs });
    socket.send(JSON.stringify({
      type: "response.output_item.done",
      output_index: 0,
      item: {
        id: `${responseId}-message`,
        type: "message",
        role: "assistant",
        status: "completed",
        content: [{ type: "output_text", text: finalText }],
      },
    }));
    socket.send(JSON.stringify({
      type: "response.completed",
      response: {
        id: responseId,
        status: "completed",
        output: [],
        usage: null,
      },
    }));
  }

  async waitForCleanup() {
    const deadline = performance.now() + 5_000;
    while (this.server.clients.size > 0) {
      this.rethrowFailure();
      if (performance.now() >= deadline) {
        assert.fail(`${this.server.clients.size} benchmark WebSocket(s) remained after shutdown`);
      }
      await new Promise((resolveWait) => setTimeout(resolveWait, 10));
    }
  }

  assertComplete(report) {
    this.rethrowFailure();
    for (const runtime of ["inline", "worker"]) {
      const state = this.state[runtime];
      assert.equal(state.samples, this.sampleCount);
      assert.equal(state.followOnValidated, true);
      assert.equal(state.forkValidated, true);
      assert.equal(state.cancelValidated, true);
      assert.equal(state.cancelSocketClosed, true);
      assert.equal(state.cancellationConnectionReplaced, true);
      assert.equal(state.recoveryValidated, true);
      assert.equal(report.runtimes[runtime].raw_samples.length, this.sampleCount);
      for (const sample of report.runtimes[runtime].raw_samples) {
        const recorded = this.records.get(sample.id);
        assert.ok(recorded, `server did not record ${sample.id}`);
        assert.equal(sample.server_request_received_epoch_us, recorded.requestReceivedUs);
        assert.equal(sample.server_delta_sent_epoch_us, recorded.sentUs);
      }
      for (const distribution of Object.values(report.runtimes[runtime].distributions)) {
        assert.equal(distribution.n, this.sampleCount);
      }
    }
  }

  fail(error) {
    this.failure ??= error instanceof Error ? error : new Error(String(error));
    for (const socket of this.server.clients) socket.terminate();
  }

  rethrowFailure() {
    if (this.failure) throw this.failure;
  }

  assertClosed() {
    this.rethrowFailure();
    assert.equal(this.server.address(), null, "benchmark WebSocket listener remained open after close");
    assert.equal(this.server.clients.size, 0, "benchmark WebSocket clients remained after close");
  }

  close() {
    if (this.closePromise === undefined) {
      this.closePromise = new Promise((resolveClose, reject) => {
        for (const socket of this.server.clients) socket.terminate();
        this.server.close((error) => error ? reject(error) : resolveClose());
      });
    }
    return this.closePromise;
  }
}

async function publishAfterServerClose(server, publish) {
  await server.close();
  server.assertClosed();
  await publish();
}

function epochMicroseconds() {
  return Math.round((performance.timeOrigin + performance.now()) * 1_000);
}

function isLoopback(value) {
  const hostname = new URL(value).hostname;
  return hostname === "127.0.0.1" || hostname === "localhost" || hostname === "[::1]";
}

function integerEnvironment(name, fallback, minimum) {
  const raw = process.env[name];
  if (raw === undefined) return fallback;
  const value = Number(raw);
  assert.ok(Number.isSafeInteger(value) && value >= minimum, `${name} must be an integer >= ${minimum}`);
  return value;
}

function git(...args) {
  return execFileSync("git", ["-C", REPOSITORY_ROOT, ...args], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

function deduplicateAssets(assets) {
  const byIdentity = new Map();
  for (const asset of assets) {
    byIdentity.set(`${asset.path}\0${asset.sha256}`, asset);
  }
  return [...byIdentity.values()].sort((left, right) => (
    left.path.localeCompare(right.path) || left.sha256.localeCompare(right.sha256)
  ));
}
