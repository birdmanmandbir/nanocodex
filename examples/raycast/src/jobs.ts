import { randomUUID } from "node:crypto";
import {
  mkdir,
  link,
  open,
  readFile,
  readdir,
  rename,
  stat,
  unlink,
} from "node:fs/promises";
import { join } from "node:path";
import {
  initialTerminalState,
  queuePrompt,
  type TerminalState,
} from "nanocodex-tui";

import type { SavedConversation } from "./sessions";

const JOB_VERSION = 1;
const JOB_FILE = /^job-([0-9a-f-]{36})\.json$/i;
const JOB_ID =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export type BackgroundJobStatus =
  | "queued"
  | "starting"
  | "running"
  | "completed"
  | "failed"
  | "cancelled";

export type BackgroundConversation = Omit<
  SavedConversation,
  "createdAt" | "updatedAt"
> & {
  createdAt: string;
  updatedAt: string;
};

export type BackgroundJob = {
  version: typeof JOB_VERSION;
  id: string;
  revision: number;
  prompt: string;
  parentJobId?: string;
  workspace: string;
  conversation?: BackgroundConversation;
  status: BackgroundJobStatus;
  statusDetail: string;
  terminal: TerminalState;
  truncatedHistory: boolean;
  payment?: {
    channelId?: string;
    cumulativePayment: string;
  };
  error?: string;
  createdAt: string;
  updatedAt: string;
  startedAt?: string;
  completedAt?: string;
};

export type CreateBackgroundJob = {
  prompt: string;
  workspace: string;
  conversation?: BackgroundConversation;
  parentJobId?: string;
};

export type BackgroundJobSubmission = CreateBackgroundJob & {
  id: string;
  createdAt: string;
};

export class BackgroundJobStore {
  readonly directory: string;

  constructor(directory: string) {
    this.directory = directory;
  }

  async create(options: CreateBackgroundJob): Promise<BackgroundJob> {
    return this.enqueue(newBackgroundJobSubmission(options));
  }

  async enqueue(submission: BackgroundJobSubmission): Promise<BackgroundJob> {
    const job = draftBackgroundJob(submission);
    await mkdir(this.directory, { recursive: true, mode: 0o700 });
    const saved: BackgroundJob = {
      ...job,
      revision: 1,
      updatedAt: new Date().toISOString(),
    };
    try {
      await atomicCreate(this.jobPath(job.id), JSON.stringify(saved));
      return saved;
    } catch (cause) {
      if (!isNodeError(cause, "EEXIST")) throw cause;
      const existing = await this.read(job.id);
      if (
        !existing ||
        existing.prompt !== job.prompt ||
        existing.workspace !== job.workspace
      ) {
        throw new Error(`background job ${job.id} already has different input`);
      }
      return existing;
    }
  }

  async read(id: string): Promise<BackgroundJob | undefined> {
    assertJobId(id);
    let encoded: string;
    try {
      encoded = await readFile(this.jobPath(id), "utf8");
    } catch (cause) {
      if (isNodeError(cause, "ENOENT")) return undefined;
      throw cause;
    }
    return decodeJob(encoded, this.jobPath(id));
  }

  async list(limit = 100): Promise<BackgroundJob[]> {
    if (limit <= 0) return [];
    let entries: string[];
    try {
      entries = await readdir(this.directory);
    } catch (cause) {
      if (isNodeError(cause, "ENOENT")) return [];
      throw cause;
    }
    const jobs = await Promise.all(
      entries.flatMap((name) => {
        const id = JOB_FILE.exec(name)?.[1];
        if (!id) return [];
        return [this.read(id).catch(() => undefined)];
      }),
    );
    return jobs
      .filter((job) => job !== undefined)
      .sort((left, right) => right.createdAt.localeCompare(left.createdAt))
      .slice(0, limit);
  }

  async save(job: BackgroundJob): Promise<BackgroundJob> {
    assertJobId(job.id);
    await mkdir(this.directory, { recursive: true, mode: 0o700 });
    const saved: BackgroundJob = {
      ...job,
      version: JOB_VERSION,
      revision: job.revision + 1,
      updatedAt: new Date().toISOString(),
    };
    await atomicWrite(this.jobPath(job.id), JSON.stringify(saved));
    return saved;
  }

  async requestCancellation(id: string): Promise<void> {
    assertJobId(id);
    await mkdir(this.directory, { recursive: true, mode: 0o700 });
    const file = await open(this.cancelPath(id), "a", 0o600);
    await file.close();
  }

  async cancellationRequested(id: string): Promise<boolean> {
    assertJobId(id);
    try {
      await stat(this.cancelPath(id));
      return true;
    } catch (cause) {
      if (isNodeError(cause, "ENOENT")) return false;
      throw cause;
    }
  }

  async clearCancellation(id: string): Promise<void> {
    assertJobId(id);
    try {
      await unlink(this.cancelPath(id));
    } catch (cause) {
      if (!isNodeError(cause, "ENOENT")) throw cause;
    }
  }

  private jobPath(id: string): string {
    return join(this.directory, `job-${id}.json`);
  }

  private cancelPath(id: string): string {
    return join(this.directory, `cancel-${id}`);
  }
}

export function newBackgroundJobSubmission(
  options: CreateBackgroundJob,
): BackgroundJobSubmission {
  return {
    ...options,
    id: randomUUID(),
    createdAt: new Date().toISOString(),
  };
}

export function draftBackgroundJob(
  submission: BackgroundJobSubmission,
): BackgroundJob {
  const prompt = submission.prompt.trim();
  if (!prompt) throw new Error("a background job requires a prompt");
  assertJobId(submission.id);
  if (submission.parentJobId) assertJobId(submission.parentJobId);
  if (!Number.isFinite(Date.parse(submission.createdAt))) {
    throw new Error("a background job requires a valid creation timestamp");
  }
  return {
    version: JOB_VERSION,
    id: submission.id,
    revision: 0,
    prompt,
    ...(submission.parentJobId ? { parentJobId: submission.parentJobId } : {}),
    workspace: submission.workspace,
    ...(submission.conversation
      ? { conversation: submission.conversation }
      : {}),
    status: "queued",
    statusDetail: "Queued in background",
    terminal: queuePrompt(
      initialTerminalState("Queued in background"),
      1,
      prompt,
    ),
    truncatedHistory: false,
    createdAt: submission.createdAt,
    updatedAt: submission.createdAt,
  };
}

export function serializeConversation(
  conversation: SavedConversation,
): BackgroundConversation {
  return {
    ...conversation,
    createdAt: conversation.createdAt.toISOString(),
    updatedAt: conversation.updatedAt.toISOString(),
  };
}

export function deserializeConversation(
  conversation: BackgroundConversation,
): SavedConversation {
  return {
    ...conversation,
    createdAt: new Date(conversation.createdAt),
    updatedAt: new Date(conversation.updatedAt),
  };
}

export function isTerminalJob(job: BackgroundJob): boolean {
  return (
    job.status === "completed" ||
    job.status === "failed" ||
    job.status === "cancelled"
  );
}

async function atomicWrite(path: string, encoded: string): Promise<void> {
  const temporary = `${path}.${process.pid}.${randomUUID()}.tmp`;
  const file = await open(temporary, "wx", 0o600);
  try {
    await file.writeFile(encoded, "utf8");
    await file.sync();
    await file.close();
    await rename(temporary, path);
  } catch (cause) {
    await file.close().catch(() => undefined);
    await unlink(temporary).catch(() => undefined);
    throw cause;
  }
}

async function atomicCreate(path: string, encoded: string): Promise<void> {
  const temporary = `${path}.${process.pid}.${randomUUID()}.tmp`;
  const file = await open(temporary, "wx", 0o600);
  try {
    await file.writeFile(encoded, "utf8");
    await file.sync();
    await file.close();
    await link(temporary, path);
    await unlink(temporary);
  } catch (cause) {
    await file.close().catch(() => undefined);
    await unlink(temporary).catch(() => undefined);
    throw cause;
  }
}

function decodeJob(encoded: string, path: string): BackgroundJob {
  let value: unknown;
  try {
    value = JSON.parse(encoded);
  } catch (cause) {
    throw new Error(`failed to decode ${path}: ${errorMessage(cause)}`);
  }
  if (
    !isObject(value) ||
    value.version !== JOB_VERSION ||
    typeof value.id !== "string" ||
    typeof value.prompt !== "string" ||
    typeof value.workspace !== "string" ||
    typeof value.status !== "string" ||
    typeof value.statusDetail !== "string" ||
    !isObject(value.terminal) ||
    typeof value.createdAt !== "string" ||
    typeof value.updatedAt !== "string"
  ) {
    throw new Error(`failed to decode ${path}: invalid background job`);
  }
  assertJobId(value.id);
  return value as BackgroundJob;
}

function assertJobId(id: string): void {
  if (!JOB_ID.test(id)) throw new Error(`invalid background job id: ${id}`);
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
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
