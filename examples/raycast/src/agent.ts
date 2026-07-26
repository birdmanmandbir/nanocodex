import { environment } from "@raycast/api";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { Agent, type AgentEvent, type Turn } from "nanocodex/node";
import type { TerminalEntry, TuiMessage, TuiTarget } from "nanocodex-tui";
import { formatUnits } from "viem";

import { createMppSession, type MppSetup } from "./mpp";
import {
  createSavedConversation,
  loadSavedConversation,
  type CodexRolloutJournal,
  type SavedConversation,
} from "./sessions";

const MAIN_TARGET = {
  pane: "main",
  branchId: 0,
} as const satisfies TuiTarget;
const EVENT_BATCH_MS = 80;
const RAYCAST_INSTRUCTIONS =
  "You are Codex, an agent based on GPT-5. Work with the user until their request is genuinely handled. Be concise, show useful reasoning summaries, and use Markdown when it improves clarity.";

let nanocodexModule: Promise<WebAssembly.Module> | undefined;

type BaseTuiMessage = Exclude<
  TuiMessage,
  { type: "event" } | { type: "ready" }
>;
type DefaultAgent = Awaited<ReturnType<typeof Agent.create>>;

export type NanocodexSessionMessage =
  | BaseTuiMessage
  | { type: "events"; events: AgentEvent[] }
  | { type: "setupStatus"; status: string }
  | {
      type: "ready";
      sessionId: string;
      path: string;
      entries: TerminalEntry[];
      truncated: boolean;
      workspace: string;
    }
  | {
      type: "payment";
      payment: { channelId?: string; cumulativePayment: string };
    }
  | { type: "turnRejected"; id: number; error: string }
  | { type: "persistenceFailed"; error: string };

export type NanocodexTurnResult = {
  message?: string;
  error?: string;
  persisted: boolean;
  persistenceError?: string;
};

type TurnRecord = {
  id: number;
  prompt: string;
  turn: Turn;
  startedAt: number;
  settled: boolean;
  compacted: boolean;
};

export class NanocodexSession {
  private readonly turns = new Map<number, TurnRecord>();
  private readonly eventQueue: AgentEvent[] = [];
  private eventTimer: ReturnType<typeof setTimeout> | undefined;
  private disposed = false;

  private constructor(
    private readonly agent: DefaultAgent,
    private readonly eventWatch: ReturnType<DefaultAgent["events"]["watch"]>,
    private readonly mpp: MppSetup,
    private readonly journal: CodexRolloutJournal,
    private emit: (message: NanocodexSessionMessage) => void,
  ) {}

  static async create(
    options: {
      saved?: SavedConversation;
      workspace: string;
    },
    emit: (message: NanocodexSessionMessage) => void,
  ): Promise<NanocodexSession> {
    emit({ type: "setupStatus", status: "Prewarming JS/WASM session..." });
    const [module, mpp, stored] = await Promise.all([
      loadNanocodexModule(),
      createMppSession((status) => emit({ type: "setupStatus", status })),
      options.saved
        ? loadSavedConversation(options.saved, options.workspace)
        : createSavedConversation({
            workspace: options.workspace,
            instructions: RAYCAST_INSTRUCTIONS,
          }),
    ]);
    const instructions =
      "instructions" in stored && stored.instructions
        ? stored.instructions
        : RAYCAST_INSTRUCTIONS;
    const sessionId =
      "snapshot" in stored ? stored.snapshot.lineage_id : stored.sessionId;
    const workspace =
      "snapshot" in stored ? stored.snapshot.workspace : stored.workspace;

    emit({
      type: "setupStatus",
      status: "Starting Nanocodex WASM agent...",
    });
    const agent = await Agent.create({
      mpp: mpp.manager,
      module,
      thinking: "low",
      reasoningMode: "standard",
      fastMode: true,
      instructions,
      sessionId,
      ...("snapshot" in stored ? { resume: stored.snapshot } : {}),
    });
    const eventWatch = agent.events.watch();
    const session = new NanocodexSession(
      agent,
      eventWatch,
      mpp,
      stored.journal,
      emit,
    );
    eventWatch.onEvent((event) => session.onEvent(event));
    emit({
      type: "ready",
      sessionId: agent.sessionId,
      path: stored.journal.path,
      entries: "transcript" in stored ? stored.transcript.entries : [],
      truncated: "transcript" in stored ? stored.transcript.truncated : false,
      workspace,
    });
    session.postPaymentStatus();
    return session;
  }

  setEmitter(emit: (message: NanocodexSessionMessage) => void): void {
    this.emit = emit;
  }

  prompt(
    id: number,
    prompt: string,
    persistenceId?: string,
  ): Promise<NanocodexTurnResult> {
    if (this.disposed) {
      return Promise.resolve({
        error: "the Nanocodex session was disposed",
        persisted: false,
      });
    }
    let turn: Turn;
    try {
      turn = this.agent.turn.prompt({ input: prompt });
    } catch (cause) {
      const error = errorMessage(cause);
      this.post({
        type: "turnRejected",
        id,
        error,
      });
      return Promise.resolve({ error, persisted: false });
    }
    const record: TurnRecord = {
      id,
      prompt,
      turn,
      startedAt: Date.now(),
      settled: false,
      compacted: false,
    };
    this.turns.set(id, record);
    return turn
      .result()
      .then(
        async (message): Promise<NanocodexTurnResult> => {
          record.settled = true;
          const snapshot = turn.snapshot();
          let persistenceError: string | undefined;
          try {
            await this.journal.appendTurn({
              turnId: persistenceId,
              prompt,
              finalMessage: message,
              snapshot,
              compacted: record.compacted,
              startedAt: record.startedAt,
            });
          } catch (cause) {
            persistenceError = errorMessage(cause);
            this.post({
              type: "persistenceFailed",
              error: persistenceError,
            });
          }
          this.post({
            type: "turnFinished",
            target: MAIN_TARGET,
            id,
            message,
          });
          this.postPaymentStatus();
          return {
            message,
            persisted: persistenceError === undefined,
            ...(persistenceError ? { persistenceError } : {}),
          };
        },
        (cause): NanocodexTurnResult => {
          record.settled = true;
          const error = errorMessage(cause);
          this.post({
            type: "turnFinished",
            target: MAIN_TARGET,
            id,
            error,
          });
          return { error, persisted: false };
        },
      )
      .finally(() => {
        this.turns.delete(id);
        turn.dispose();
      });
  }

  async cancel(): Promise<void> {
    const active = this.firstUnsettled();
    if (!active) {
      this.post({
        type: "cancelFailed",
        target: MAIN_TARGET,
        error: "No active or queued turn",
      });
      return;
    }
    try {
      await active.turn.cancel();
      this.post({ type: "cancelAccepted", target: MAIN_TARGET });
    } catch (cause) {
      this.post({
        type: "cancelFailed",
        target: MAIN_TARGET,
        error: errorMessage(cause),
      });
    }
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this.eventTimer !== undefined) clearTimeout(this.eventTimer);
    this.eventTimer = undefined;
    this.eventQueue.length = 0;
    this.eventWatch.off();
    for (const record of this.turns.values()) {
      if (!record.settled) void record.turn.cancel().catch(() => undefined);
      record.turn.dispose();
    }
    this.turns.clear();
    this.agent.dispose();
  }

  private firstUnsettled(): TurnRecord | undefined {
    return [...this.turns.values()].find((record) => !record.settled);
  }

  private onEvent(event: AgentEvent): void {
    if (event.type === "model.compaction.completed") {
      const active = this.firstUnsettled();
      if (active) active.compacted = true;
    }
    this.eventQueue.push(event);
    if (event.type === "run.completed" || event.type === "run.failed") {
      this.flushEvents();
    } else {
      this.eventTimer ??= setTimeout(() => this.flushEvents(), EVENT_BATCH_MS);
    }
  }

  private flushEvents(): void {
    if (this.eventTimer !== undefined) clearTimeout(this.eventTimer);
    this.eventTimer = undefined;
    if (!this.eventQueue.length) return;
    const events = this.eventQueue.splice(0);
    this.post({ type: "events", events });
  }

  private postPaymentStatus(): void {
    this.post({
      type: "payment",
      payment: {
        channelId: this.mpp.manager.channelId,
        cumulativePayment: formatUnits(this.mpp.manager.cumulative, 6),
      },
    });
  }

  private post(message: NanocodexSessionMessage): void {
    if (!this.disposed) this.emit(message);
  }
}

function loadNanocodexModule(): Promise<WebAssembly.Module> {
  nanocodexModule ??= readFile(
    join(environment.assetsPath, "nanocodex_bg.wasm"),
  ).then((bytes) => WebAssembly.compile(bytes));
  return nanocodexModule;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
