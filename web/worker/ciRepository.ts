import {
  isCiSourcePublication,
  sourceArchiveKey,
  sourceTreeKey,
  type CiSourcePublication,
  type NanocodexCiParams,
} from "./ciSource.ts";

const HEAD_KEY = "head";
const PUBLICATION_PREFIX = "publication:";
const RUN_PREFIX = "run:";
const OUTBOX_PREFIX = "outbox:";
const MAX_DISPATCH_DELAY_MS = 5 * 60 * 1_000;
const MAX_RETAINED_RUNS = 100;
const TERMINAL_WORKFLOW_STATES = new Set(["complete", "errored", "terminated"]);

export type CiRunRecord = {
  version: 1;
  head: string;
  beforeHead: string | null;
  workflowId: string;
  state: "pending" | "dispatched";
  attempts: number;
  publishedAt: string;
  dispatchedAt?: string;
  lastDispatchError?: string;
  nextDispatchAt?: string;
};

type CiOutboxRecord = {
  version: 1;
  run: CiRunRecord;
  params: NanocodexCiParams;
};

type CiRepositoryEnv = {
  CI_WORKFLOW: Workflow<NanocodexCiParams>;
  CI_SOURCE: R2Bucket;
};

export class CiRepository {
  readonly #state: DurableObjectState;
  readonly #env: CiRepositoryEnv;

  constructor(state: DurableObjectState, env: CiRepositoryEnv) {
    this.#state = state;
    this.#env = env;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/state" && request.method === "GET") {
      const head = await this.#state.storage.get<string>(HEAD_KEY);
      if (!head) return Response.json({ error: "not_published" }, { status: 404 });
      const [publication, run] = await Promise.all([
        this.#state.storage.get<CiSourcePublication>(`${PUBLICATION_PREFIX}${head}`),
        this.#state.storage.get<CiRunRecord>(`${RUN_PREFIX}${head}`),
      ]);
      return publication && run
        ? Response.json({ publication, run })
        : Response.json({ error: "repository_state_invalid" }, { status: 503 });
    }
    const publicationMatch = url.pathname.match(/^\/publications\/([a-f0-9]{40})$/);
    if (publicationMatch && request.method === "GET") {
      const publication = await this.#state.storage.get<CiSourcePublication>(
        `${PUBLICATION_PREFIX}${publicationMatch[1]}`,
      );
      return publication
        ? Response.json(publication)
        : Response.json({ error: "not_found" }, { status: 404 });
    }
    const runMatch = url.pathname.match(/^\/runs\/([a-f0-9]{40})$/);
    if (runMatch && request.method === "GET") {
      const run = await this.#state.storage.get<CiRunRecord>(`${RUN_PREFIX}${runMatch[1]}`);
      return run ? Response.json(run) : Response.json({ error: "not_found" }, { status: 404 });
    }
    if (url.pathname === "/runs" && request.method === "GET") {
      const records = await this.#state.storage.list<CiRunRecord>({ prefix: RUN_PREFIX });
      const runs = [...records.values()]
        .sort((left, right) => right.publishedAt.localeCompare(left.publishedAt))
        .slice(0, 50);
      return Response.json({ runs });
    }
    if (url.pathname === "/publications" && request.method === "PUT") {
      return this.#publish(request);
    }
    return Response.json({ error: "not_found" }, { status: 404 });
  }

  async alarm(): Promise<void> {
    const outbox = await this.#state.storage.list<CiOutboxRecord>({ prefix: OUTBOX_PREFIX });
    const now = Date.now();
    const pending = [...outbox.entries()]
      .filter(([, record]) => dispatchTime(record) <= now)
      .sort(([, left], [, right]) =>
        dispatchTime(left) - dispatchTime(right) ||
        left.run.publishedAt.localeCompare(right.run.publishedAt) ||
        left.run.head.localeCompare(right.run.head)
      )[0];
    if (!pending) {
      await this.#scheduleNextDispatch(outbox);
      return;
    }
    const [key, record] = pending;
    try {
      await this.#env.CI_WORKFLOW.createBatch([
        {
          id: record.run.workflowId,
          params: record.params,
          retention: { successRetention: "30 days", errorRetention: "30 days" },
        },
      ]);
      const dispatched: CiRunRecord = {
        ...record.run,
        state: "dispatched",
        attempts: record.run.attempts + 1,
        dispatchedAt: new Date().toISOString(),
      };
      await this.#state.storage.transaction(async (transaction) => {
        await transaction.put(`${RUN_PREFIX}${record.run.head}`, dispatched);
        await transaction.delete(key);
      });
      await this.#trimHistory().catch((cause) => {
        console.error("Failed to trim retained CI repository history", cause);
      });
    } catch (cause) {
      const attempts = record.run.attempts + 1;
      if (await this.#workflowExists(record.run.workflowId)) {
        const dispatched: CiRunRecord = {
          ...record.run,
          state: "dispatched",
          attempts,
          dispatchedAt: new Date().toISOString(),
        };
        delete dispatched.lastDispatchError;
        delete dispatched.nextDispatchAt;
        await this.#state.storage.transaction(async (transaction) => {
          await transaction.put(`${RUN_PREFIX}${record.run.head}`, dispatched);
          await transaction.delete(key);
        });
        await this.#scheduleNextDispatch();
        return;
      }
      const retryAt = Date.now() + Math.min(
        1_000 * 2 ** Math.min(attempts, 8),
        MAX_DISPATCH_DELAY_MS,
      );
      const failed: CiOutboxRecord = {
        ...record,
        run: {
          ...record.run,
          attempts,
          lastDispatchError: boundedError(cause),
          nextDispatchAt: new Date(retryAt).toISOString(),
        },
      };
      await this.#state.storage.transaction(async (transaction) => {
        await transaction.put(key, failed);
        await transaction.put(`${RUN_PREFIX}${record.run.head}`, failed.run);
      });
      await this.#scheduleNextDispatch();
      return;
    }
    await this.#scheduleNextDispatch();
  }

  async #workflowExists(workflowId: string): Promise<boolean> {
    try {
      const status = await (await this.#env.CI_WORKFLOW.get(workflowId)).status();
      return status.status !== "unknown";
    } catch {
      return false;
    }
  }

  async #scheduleNextDispatch(
    known?: Map<string, CiOutboxRecord>,
  ): Promise<void> {
    const outbox = known ?? await this.#state.storage.list<CiOutboxRecord>({
      prefix: OUTBOX_PREFIX,
    });
    if (outbox.size === 0) return;
    const next = Math.min(...[...outbox.values()].map(dispatchTime));
    await this.#state.storage.setAlarm(Math.max(Date.now(), next));
  }

  async #publish(request: Request): Promise<Response> {
    const value = await request.json().catch(() => undefined) as {
      expectedHead?: unknown;
      publication?: unknown;
    } | undefined;
    if (
      value == null ||
      !(value.expectedHead === null ||
        (typeof value.expectedHead === "string" && /^[a-f0-9]{40}$/.test(value.expectedHead))) ||
      !isCiSourcePublication(value.publication)
    ) return Response.json({ error: "invalid_publication" }, { status: 400 });

    const publication = value.publication;
    const existing = await this.#state.storage.get<CiSourcePublication>(
      `${PUBLICATION_PREFIX}${publication.head}`,
    );
    if (existing) {
      const run = await this.#state.storage.get<CiRunRecord>(`${RUN_PREFIX}${publication.head}`);
      return stableJson(existing) === stableJson(publication) && run
        ? Response.json({ publication: existing, run })
        : Response.json({ error: "publication_conflict" }, { status: 409 });
    }

    const workflowId = `ci-${publication.head}`;
    const run: CiRunRecord = {
      version: 1,
      head: publication.head,
      beforeHead: value.expectedHead,
      workflowId,
      state: "pending",
      attempts: 0,
      publishedAt: publication.publishedAt,
    };
    const params = workflowParams(publication, value.expectedHead);
    const result = await this.#state.storage.transaction(async (transaction) => {
      const currentHead = await transaction.get<string>(HEAD_KEY);
      if ((currentHead ?? null) !== value.expectedHead) return currentHead ?? null;
      await transaction.put({
        [HEAD_KEY]: publication.head,
        [`${PUBLICATION_PREFIX}${publication.head}`]: publication,
        [`${RUN_PREFIX}${publication.head}`]: run,
        [`${OUTBOX_PREFIX}${publication.head}`]: { version: 1, run, params } satisfies CiOutboxRecord,
      });
      await transaction.setAlarm(Date.now());
      return undefined;
    });
    if (result !== undefined) {
      return Response.json({ error: "publication_conflict", currentHead: result }, { status: 409 });
    }
    await this.#trimHistory().catch((cause) => {
      console.error("Failed to trim retained CI repository history", cause);
    });
    return Response.json({ publication, run }, { status: 202 });
  }

  async #trimHistory(): Promise<void> {
    const [currentHead, runs, outbox] = await Promise.all([
      this.#state.storage.get<string>(HEAD_KEY),
      this.#state.storage.list<CiRunRecord>({ prefix: RUN_PREFIX }),
      this.#state.storage.list<CiOutboxRecord>({ prefix: OUTBOX_PREFIX }),
    ]);
    if (runs.size <= MAX_RETAINED_RUNS) return;
    const pending = new Set([...outbox.values()].map(({ run }) => run.head));
    const overflow = runs.size - MAX_RETAINED_RUNS;
    const candidates = [...runs.values()]
      .filter(({ head, state }) =>
        head !== currentHead && state === "dispatched" && !pending.has(head)
      )
      .sort((left, right) => left.publishedAt.localeCompare(right.publishedAt));
    const statuses = await Promise.all(candidates.map(async (run) => {
      try {
        return {
          run,
          terminal: TERMINAL_WORKFLOW_STATES.has(
            (await (await this.#env.CI_WORKFLOW.get(run.workflowId)).status()).status,
          ),
        };
      } catch {
        return { run, terminal: false };
      }
    }));
    const removable = statuses
      .filter(({ terminal }) => terminal)
      .slice(0, overflow)
      .map(({ run }) => run);
    if (removable.length === 0) return;
    await this.#env.CI_SOURCE.delete(removable.flatMap(({ head }) => [
      sourceArchiveKey(head),
      sourceTreeKey(head),
    ]));
    await this.#state.storage.delete(removable.flatMap(({ head }) => [
      `${RUN_PREFIX}${head}`,
      `${PUBLICATION_PREFIX}${head}`,
    ]));
  }
}

function workflowParams(
  publication: CiSourcePublication,
  beforeHead: string | null,
): NanocodexCiParams {
  return {
    provider: "nanocodex-source",
    providerData: {
      archiveKey: publication.archive.key,
      archiveSha256: publication.archive.sha256,
      archiveSize: publication.archive.size,
      treeKey: publication.tree.key,
      treeSha256: publication.tree.sha256,
      cargoLockBlob: publication.cargoLockBlob,
      cargoVendorKey: publication.cargoVendor.key,
      cargoVendorSha256: publication.cargoVendor.sha256,
      cargoVendorSize: publication.cargoVendor.size,
      rustSecRevision: publication.rustSecRevision,
      rustSecKey: publication.rustSec.key,
      rustSecSha256: publication.rustSec.sha256,
      rustSecSize: publication.rustSec.size,
    },
    event: { type: "push" },
    owner: "gakonst",
    repo: "nanocodex",
    sha: publication.head,
    remote: "cloudflare",
    trigger: "push",
    ref: publication.ref,
    branch: publication.branch,
    ...(beforeHead == null ? {} : { beforeSha: beforeHead }),
  };
}

function stableJson(value: CiSourcePublication): string {
  return JSON.stringify(value);
}

function boundedError(value: unknown): string {
  const text = value instanceof Error ? value.message : String(value);
  return text.slice(0, 2_000);
}

function dispatchTime(record: CiOutboxRecord): number {
  const timestamp = record.run.nextDispatchAt == null
    ? 0
    : Date.parse(record.run.nextDispatchAt);
  return Number.isFinite(timestamp) ? timestamp : 0;
}
