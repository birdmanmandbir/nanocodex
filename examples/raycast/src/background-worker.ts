import { randomUUID } from "node:crypto";
import {
  mkdir,
  readFile,
  rmdir,
  stat,
  unlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import {
  appendError,
  applyAgentEvents,
  initialTerminalState,
  queuePrompt,
  turnFinished,
  turnRejected,
  type TerminalState,
} from "nanocodex-tui";

import {
  NanocodexSession,
  type NanocodexSessionMessage,
  type NanocodexTurnResult,
} from "./agent";
import {
  type BackgroundConversation,
  type BackgroundJob,
  BackgroundJobStore,
  deserializeConversation,
  isTerminalJob,
} from "./jobs";
import { findCompletedTurnMessage } from "./sessions";

const JOB_WRITE_DELAY_MS = 100;
const CANCEL_POLL_MS = 250;
const WORKER_HEARTBEAT_MS = 3_000;
const STALE_WORKER_MS = 30_000;
const WORKER_HANDOFF_MS = 3_000;
const QUEUE_QUIET_MS = 1_000;

export async function runBackgroundJobs(
  store: BackgroundJobStore,
  requestedJobId?: string,
): Promise<void> {
  const lease = await acquireWorkerLease(store, requestedJobId);
  if (!lease) return;

  let activePresenter: JobPresenter | undefined;
  let session: NanocodexSession | undefined;
  let sessionThreadId: string | undefined;
  const heartbeat = setInterval(() => {
    void lease.heartbeat();
    activePresenter?.heartbeat();
  }, WORKER_HEARTBEAT_MS);

  try {
    await recoverInterruptedJobs(store);
    while (lease.owned) {
      const queued = (await store.list(1_000))
        .filter((job) => job.status === "queued")
        .reverse();
      const resolved = await firstRunnableJob(store, queued);
      if (!resolved) {
        await delay(QUEUE_QUIET_MS);
        const remaining = (await store.list(1_000)).some(
          (job) => job.status === "queued",
        );
        if (!remaining) break;
        continue;
      }
      if ("failed" in resolved) {
        await failQueuedJob(store, resolved.job, resolved.failed);
        continue;
      }

      let job = resolved.job;
      if (await store.cancellationRequested(job.id)) {
        await cancelQueuedJob(store, job);
        continue;
      }

      const presenter = new JobPresenter(store, {
        ...job,
        status: "starting",
        statusDetail: "Starting background JS/WASM session",
        terminal: {
          ...job.terminal,
          status: "Starting background JS/WASM session",
        },
        startedAt: job.startedAt ?? new Date().toISOString(),
      });
      activePresenter = presenter;
      await presenter.flush();
      job = presenter.current;

      try {
        const targetThreadId = job.conversation?.id;
        if (!session || sessionThreadId !== targetThreadId) {
          session?.dispose();
          session = undefined;
          sessionThreadId = undefined;
          session = await NanocodexSession.create(
            {
              ...(job.conversation
                ? { saved: deserializeConversation(job.conversation) }
                : {}),
              workspace: job.workspace,
            },
            (message) => presenter.handle(message),
          );
          sessionThreadId = presenter.current.conversation?.id;
        } else {
          session.setEmitter((message) => presenter.handle(message));
        }

        if (await store.cancellationRequested(job.id)) {
          await session.cancel();
        }
        const cancelPoll = setInterval(() => {
          void store.cancellationRequested(job.id).then((requested) => {
            if (requested) void session?.cancel();
          });
        }, CANCEL_POLL_MS);
        try {
          const outcome = await session.prompt(1, job.prompt, job.id);
          await presenter.finish(outcome);
        } finally {
          clearInterval(cancelPoll);
        }
      } catch (cause) {
        session?.dispose();
        session = undefined;
        sessionThreadId = undefined;
        await presenter.fail(errorMessage(cause));
      } finally {
        activePresenter = undefined;
        await store.clearCancellation(job.id);
      }
    }
  } finally {
    clearInterval(heartbeat);
    session?.dispose();
    await lease.release();
  }
}

class JobPresenter {
  private job: BackgroundJob;
  private saveTimer: ReturnType<typeof setTimeout> | undefined;
  private writes = Promise.resolve();
  private persistedRevision: number;

  constructor(
    private readonly store: BackgroundJobStore,
    job: BackgroundJob,
  ) {
    this.job = job;
    this.persistedRevision = job.revision;
  }

  get current(): BackgroundJob {
    return this.job;
  }

  handle(message: NanocodexSessionMessage): void {
    switch (message.type) {
      case "setupStatus":
        this.job = {
          ...this.job,
          status: "starting",
          statusDetail: message.status,
          terminal: { ...this.job.terminal, status: message.status },
        };
        break;
      case "ready":
        this.job = {
          ...this.job,
          conversation: {
            ...(this.job.conversation ??
              newConversationReference(
                this.job,
                message.sessionId,
                message.path,
              )),
            id: message.sessionId,
            path: message.path,
            cwd: message.workspace,
            updatedAt: new Date().toISOString(),
          },
          workspace: message.workspace,
          truncatedHistory: message.truncated,
          statusDetail: "Starting turn",
          terminal: { ...this.job.terminal, status: "Starting" },
        };
        break;
      case "events": {
        const terminal = applyAgentEvents(this.job.terminal, message.events);
        this.job = {
          ...this.job,
          status: terminal.running ? "running" : this.job.status,
          statusDetail: terminal.status,
          terminal,
        };
        break;
      }
      case "payment":
        this.job = { ...this.job, payment: message.payment };
        break;
      case "turnFinished":
        this.job = {
          ...this.job,
          statusDetail: message.error ? "Turn failed" : "Finalizing",
          terminal: turnFinished(this.job.terminal, message.error),
        };
        break;
      case "turnRejected":
        this.job = {
          ...this.job,
          statusDetail: "Turn rejected",
          terminal: turnRejected(this.job.terminal, message.error),
        };
        break;
      case "persistenceFailed":
        this.job = {
          ...this.job,
          error: `Could not save turn: ${message.error}`,
          terminal: appendError(
            this.job.terminal,
            `Could not save turn: ${message.error}`,
          ),
        };
        break;
      case "cancelAccepted":
        this.job = {
          ...this.job,
          statusDetail: "Cancellation accepted",
          terminal: {
            ...this.job.terminal,
            status: "Cancellation accepted",
          },
        };
        break;
      case "cancelFailed":
        this.job = {
          ...this.job,
          terminal: appendError(this.job.terminal, message.error),
        };
        break;
      case "fatal":
        this.job = {
          ...this.job,
          error: message.message,
          terminal: appendError(
            { ...this.job.terminal, running: false, status: "Session failed" },
            message.message,
          ),
        };
        break;
      default:
        break;
    }
    this.scheduleSave();
  }

  heartbeat(): void {
    this.scheduleSave();
  }

  async finish(outcome: NanocodexTurnResult): Promise<void> {
    const completedAt = new Date().toISOString();
    if (outcome.persisted) {
      this.job = {
        ...this.job,
        status: "completed",
        statusDetail: "Completed",
        terminal: {
          ...ensureAssistant(this.job.terminal, outcome.message),
          running: false,
          status: "Ready",
        },
        error: undefined,
        completedAt,
      };
    } else if (outcome.error === "the turn was cancelled") {
      this.job = {
        ...this.job,
        status: "cancelled",
        statusDetail: "Cancelled",
        terminal: {
          ...this.job.terminal,
          running: false,
          status: "Cancelled",
        },
        error: undefined,
        completedAt,
      };
    } else {
      const error =
        outcome.persistenceError ??
        outcome.error ??
        "the turn did not complete";
      this.job = {
        ...this.job,
        status: "failed",
        statusDetail: "Failed",
        terminal: appendError(
          { ...this.job.terminal, running: false, status: "Turn failed" },
          error,
        ),
        error,
        completedAt,
      };
    }
    await this.refreshConversationMetadata();
    await this.flush();
  }

  async fail(error: string): Promise<void> {
    this.job = {
      ...this.job,
      status: "failed",
      statusDetail: "Background worker failed",
      terminal: appendError(
        { ...this.job.terminal, running: false, status: "Session failed" },
        error,
      ),
      error,
      completedAt: new Date().toISOString(),
    };
    await this.flush();
  }

  async flush(): Promise<void> {
    if (this.saveTimer !== undefined) clearTimeout(this.saveTimer);
    this.saveTimer = undefined;
    this.enqueueSave();
    await this.writes;
  }

  private scheduleSave(): void {
    this.saveTimer ??= setTimeout(() => {
      this.saveTimer = undefined;
      this.enqueueSave();
    }, JOB_WRITE_DELAY_MS);
  }

  private enqueueSave(): void {
    const snapshot = structuredClone(this.job);
    this.writes = this.writes.then(async () => {
      const saved = await this.store.save({
        ...snapshot,
        revision: this.persistedRevision,
      });
      this.persistedRevision = saved.revision;
      if (this.job.updatedAt <= snapshot.updatedAt) {
        this.job = { ...this.job, updatedAt: saved.updatedAt };
      }
    });
  }

  private async refreshConversationMetadata(): Promise<void> {
    const conversation = this.job.conversation;
    if (!conversation) return;
    const metadata = await stat(conversation.path);
    this.job = {
      ...this.job,
      conversation: {
        ...conversation,
        size: metadata.size,
        updatedAt: metadata.mtime.toISOString(),
      },
    };
  }
}

async function recoverInterruptedJobs(
  store: BackgroundJobStore,
): Promise<void> {
  const jobs = await store.list(1_000);
  for (const job of jobs) {
    if (job.status !== "starting" && job.status !== "running") {
      continue;
    }
    const completedMessage = job.conversation
      ? await findCompletedTurnMessage(job.conversation.path, job.id)
      : undefined;
    if (completedMessage !== undefined) {
      await store.save({
        ...job,
        status: "completed",
        statusDetail: "Completed before the previous worker exited",
        terminal: {
          ...ensureAssistant(turnFinished(job.terminal), completedMessage),
          running: false,
          status: "Ready",
        },
        error: undefined,
        completedAt: job.completedAt ?? new Date().toISOString(),
      });
      continue;
    }
    await store.save({
      ...job,
      status: "queued",
      statusDetail: "Retrying from the last completed conversation state",
      terminal: queuePrompt(
        initialTerminalState(
          "Retrying from the last completed conversation state",
        ),
        1,
        job.prompt,
      ),
      error: undefined,
      startedAt: undefined,
      completedAt: undefined,
    });
  }
}

async function firstRunnableJob(
  store: BackgroundJobStore,
  jobs: BackgroundJob[],
): Promise<
  { job: BackgroundJob } | { job: BackgroundJob; failed: string } | undefined
> {
  for (let job of jobs) {
    if (!job.parentJobId) return { job };
    const parent = await store.read(job.parentJobId);
    if (!parent) {
      return {
        job,
        failed: `Parent background job ${job.parentJobId} no longer exists`,
      };
    }
    if (!isTerminalJob(parent)) continue;
    if (parent.status !== "completed" || !parent.conversation) {
      return {
        job,
        failed: `The previous prompt ${parent.status}; this follow-up was not run`,
      };
    }
    if (!job.conversation) {
      job = await store.save({
        ...job,
        conversation: parent.conversation,
        workspace: parent.workspace,
      });
    }
    return { job };
  }
  return undefined;
}

async function failQueuedJob(
  store: BackgroundJobStore,
  job: BackgroundJob,
  error: string,
): Promise<void> {
  await store.save({
    ...job,
    status: "failed",
    statusDetail: "Follow-up could not start",
    terminal: appendError(
      { ...job.terminal, running: false, status: "Turn failed" },
      error,
    ),
    error,
    completedAt: new Date().toISOString(),
  });
}

async function cancelQueuedJob(
  store: BackgroundJobStore,
  job: BackgroundJob,
): Promise<void> {
  await store.save({
    ...job,
    status: "cancelled",
    statusDetail: "Cancelled before starting",
    terminal: {
      ...turnFinished(job.terminal),
      running: false,
      status: "Cancelled",
    },
    completedAt: new Date().toISOString(),
  });
  await store.clearCancellation(job.id);
}

function newConversationReference(
  job: BackgroundJob,
  id: string,
  path: string,
): BackgroundConversation {
  return {
    id,
    path,
    title: preview(job.prompt),
    cwd: job.workspace,
    source: "cli",
    createdAt: job.createdAt,
    updatedAt: new Date().toISOString(),
    archived: false,
    size: 0,
  };
}

function ensureAssistant(
  state: TerminalState,
  message?: string,
): TerminalState {
  if (!message) return state;
  const tail = state.entries.at(-1);
  if (tail?.kind === "assistant" && tail.text === message) return state;
  const syntheticId = state.syntheticId + 1;
  return {
    ...state,
    syntheticId,
    entries: [
      ...state.entries,
      {
        id: `assistant-${syntheticId}`,
        kind: "assistant",
        text: message,
        streaming: false,
      },
    ],
  };
}

class WorkerLease {
  owned = true;

  constructor(
    private readonly lockPath: string,
    private readonly owner: string,
  ) {}

  async heartbeat(): Promise<void> {
    if (!this.owned) return;
    try {
      const now = new Date();
      await utimes(this.lockPath, now, now);
    } catch {
      this.owned = false;
    }
  }

  async release(): Promise<void> {
    if (!this.owned) return;
    this.owned = false;
    const ownerPath = join(this.lockPath, "owner");
    try {
      if ((await readFile(ownerPath, "utf8")) !== this.owner) return;
      await unlink(ownerPath);
      await rmdir(this.lockPath);
    } catch (cause) {
      if (!isNodeError(cause, "ENOENT")) throw cause;
    }
  }
}

async function acquireWorkerLease(
  store: BackgroundJobStore,
  requestedJobId?: string,
): Promise<WorkerLease | undefined> {
  const deadline = Date.now() + WORKER_HANDOFF_MS;
  while (true) {
    const lease = await tryAcquireWorkerLease(store);
    if (lease) return lease;
    if (requestedJobId) {
      const requested = await store.read(requestedJobId);
      if (!requested || requested.status !== "queued") return undefined;
    }
    if (Date.now() >= deadline) return undefined;
    await delay(200);
  }
}

async function tryAcquireWorkerLease(
  store: BackgroundJobStore,
): Promise<WorkerLease | undefined> {
  await mkdir(store.directory, { recursive: true, mode: 0o700 });
  const lockPath = join(store.directory, "worker.lock");
  const owner = `${process.pid}:${randomUUID()}`;
  try {
    await mkdir(lockPath, { mode: 0o700 });
  } catch (cause) {
    if (!isNodeError(cause, "EEXIST")) throw cause;
    const metadata = await stat(lockPath);
    if (Date.now() - metadata.mtimeMs <= STALE_WORKER_MS) return undefined;
    let removed = false;
    try {
      await unlink(join(lockPath, "owner"));
      await rmdir(lockPath);
      removed = true;
    } catch (cleanupCause) {
      if (
        !isNodeError(cleanupCause, "ENOENT") &&
        !isNodeError(cleanupCause, "ENOTEMPTY")
      ) {
        throw cleanupCause;
      }
    }
    return removed ? tryAcquireWorkerLease(store) : undefined;
  }
  try {
    await writeFile(join(lockPath, "owner"), owner, {
      encoding: "utf8",
      flag: "wx",
      mode: 0o600,
    });
  } catch (cause) {
    await rmdir(lockPath).catch(() => undefined);
    throw cause;
  }
  return new WorkerLease(lockPath, owner);
}

function preview(value: string, limit = 88): string {
  const normalized = value.split(/\s+/).filter(Boolean).join(" ");
  const characters = [...normalized];
  return characters.length <= limit
    ? normalized
    : `${characters.slice(0, limit).join("")}...`;
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function isNodeError(cause: unknown, code: string): boolean {
  return (
    cause instanceof Error &&
    "code" in cause &&
    (cause as NodeJS.ErrnoException).code === code
  );
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
