import {
  cargoVendorBundleKey,
  ciSourceLane,
  isCiSourcePublication,
  isSha1,
  isSha256,
  normalizeCiSourcePublication,
  sourceArchiveKey,
  sourceTreeKey,
  type CiDistributionRequest,
  type CiSourceLane,
  type CiSourcePublication,
  type NanocodexCiParams,
} from "./ciSource.ts";
import {
  terminateActiveSandboxes,
  terminationMarkerKey,
} from "./ciSandboxes.ts";

const MASTER_HEAD_KEY = "head";
const PULL_REQUEST_HEAD_PREFIX = "head:pull-request:";
const PULL_REQUEST_CLOSED_PREFIX = "closed:pull-request:";
const PULL_REQUEST_CLOSE_OPERATION_PREFIX = "close-operation:pull-request:";
const PULL_REQUEST_REOPEN_PREFIX = "reopen:pull-request:";
const SOURCE_PREFIX = "source:";
const PUBLICATION_PREFIX = "publication:";
const RUN_PREFIX = "run:";
const OUTBOX_PREFIX = "outbox:";
const DISTRIBUTION_LEASE_PREFIX = "lease:distribution:";
const PUBLICATION_LEASE_PREFIX = "lease:publication:";
const RELEASE_COMMIT_RESERVATION_KEY = "reservation:release-commit:active";
const RELEASE_COMMIT_RESERVATION_GENERATION_KEY =
  "reservation:release-commit:generation";
const CANCELLATION_PREFIX = "cancellation:";
const CARGO_VENDOR_MULTIPART_PREFIX = "cargo-vendor-multipart:";
const RETIRING_PREFIX = "retiring:";
const SOURCE_OBJECT_RETIRING_PREFIX = "retiring:source-object:";
const SOURCE_GC_CURSOR_KEY = "maintenance:source-gc-cursor";
const MAX_DISPATCH_DELAY_MS = 5 * 60 * 1_000;
const MAX_RETAINED_RUNS = 100;
const MAX_RETENTION_STATUS_CONCURRENCY = 16;
const MAX_SOURCE_GC_OBJECTS = 1_000;
const SOURCE_GC_GRACE_MS = 24 * 60 * 60 * 1_000;
const DISTRIBUTION_LEASE_MS = 6 * 60 * 60 * 1_000;
const PUBLICATION_LEASE_MS = 5 * 60 * 1_000;
const RELEASE_COMMIT_RESERVATION_MS = 2 * 60 * 1_000;
const SOURCE_GC_RECONCILE_MS = 5 * 60 * 1_000;
const MAX_CARGO_VENDOR_BYTES = 256 * 1024 * 1024;
const CARGO_VENDOR_PART_BYTES = 32 * 1024 * 1024;
const MAX_CARGO_VENDOR_PARTS = MAX_CARGO_VENDOR_BYTES / CARGO_VENDOR_PART_BYTES;
// Cloudflare R2 aborts incomplete multipart uploads after seven days by default.
// CI_SOURCE must retain that lifecycle bound (or configure a shorter one). It
// bounds the irreducible R2-create-before-upload-identity-commit crash window;
// an uncertain request identity is not retried before the old upload is due to
// have been aborted.
const R2_INCOMPLETE_MULTIPART_ABORT_MS = 7 * 24 * 60 * 60 * 1_000;
const PULL_REQUEST_CANCELLATION_GRACE_MS = 2 * 60 * 1_000;
const MAX_CANCELLATION_RETRY_MS = 5 * 60 * 1_000;
const MAX_TERMINAL_RESULT_BYTES = 8 * 1024 * 1024;
const MAX_TERMINATION_MARKER_BYTES = 64 * 1024;
const TERMINAL_WORKFLOW_STATES = new Set(["complete", "errored", "terminated"]);
const CLOSE_ID_PATTERN =
  /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const FENCED_ID_PATTERN =
  /^(0|[1-9][0-9]{0,15})\.[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const RELEASE_RESERVATION_OWNER = /^[A-Za-z0-9][A-Za-z0-9._:@/-]{0,191}$/;

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

export type CiTerminalResult = Record<string, unknown> & {
  version: 1;
  head: string;
  workflowId: string;
  status: "success" | "failure" | "terminated";
  completedAt: string;
};

export type CiLaneState = {
  publication: CiSourcePublication;
  run: CiRunRecord;
};

export type CiDistributionLease = {
  version: 1;
  kind: "distribution";
  leaseId: string;
  head: string;
  workflowId: string;
  acquiredAt: string;
  expiresAt: string;
};

export type CiPublicationLease = {
  version: 1;
  kind: "publication";
  leaseId: string;
  head: string;
  acquiredAt: string;
  expiresAt: string;
};

export type CiReleaseCommitReservation = {
  version: 1;
  kind: "release-commit";
  reservationId: string;
  owner: string;
  releaseKind: "stable" | "commit";
  releaseId: string;
  commit: string;
  publicationLeaseId: string;
  publicationLeaseGeneration: number;
  generation: number;
  acquiredAt: string;
  renewedAt: string;
  expiresAt: string;
};

export type CiReleaseCommitReservationProof = {
  reservation: CiReleaseCommitReservation;
  publication: CiSourcePublication;
  run: CiRunRecord;
};

type CiReleaseCommitReservationRequest = Pick<
  CiReleaseCommitReservation,
  | "owner"
  | "releaseKind"
  | "releaseId"
  | "commit"
  | "publicationLeaseId"
  | "publicationLeaseGeneration"
> & { version: 1 };

type CiPublicationLeaseRecord = CiPublicationLease & {
  publication: CiSourcePublication;
};

type CiSourceRecord = {
  version: 1;
  head: string;
  firstPublication: CiSourcePublication;
};

type CiOutboxRecord = {
  version: 1;
  run: CiRunRecord;
  params: NanocodexCiParams;
};

type CiRetiringRecord = {
  version: 1;
  head: string;
  markedAt: string;
};

type CiSourceObjectRetiringRecord = {
  version: 1;
  key: string;
  claimId: string;
  claimedAt: string;
  reconcileAfter: string;
};

type CiCargoVendorMultipartInput = {
  version: 1;
  requestId: string;
  cargoLockBlob: string;
  bundleSha256: string;
  size: number;
  partSize: number;
  partCount: number;
};

type CiCargoVendorMultipartRecord = CiCargoVendorMultipartInput & {
  stagingId: string;
} & (
  | {
    state: "creating";
    startedAt: string;
    recoverAfter: string;
  }
  | {
    state: "ready";
    startedAt: string;
    readyAt: string;
    uploadId: string;
  }
  | {
    state: "complete";
    completedAt: string;
  }
);

type CiCancellationRecord = {
  version: 1;
  head: string;
  workflowId: string;
  reason: "pull_request_superseded" | "pull_request_closed" | "operator_terminated";
  pullRequest?: number;
  requestedAt: string;
  notBefore: string;
  state: "pending" | "terminating" | "complete";
  attempts: number;
  replacementHead?: string;
  nextAttemptAt?: string;
  claimId?: string;
  claimedAt?: string;
  completedAt?: string;
  lastError?: string;
};

type CiPullRequestClosedRecord = {
  version: 1;
  closeId: string;
  mergeHead: string;
  pullRequestHead: string;
  closedAt: string;
};

type CiPullRequestReopen = {
  closeId: string;
  mergeHead: string;
  pullRequestHead: string;
};

type CiPullRequestReopenRecord = {
  version: 1;
  closeId: string;
  closedMergeHead: string;
  closedPullRequestHead: string;
  publicationHead: string;
  publicationPullRequestHead: string;
  reopenedAt: string;
};

type CiRepositoryEnv = {
  CI_WORKFLOW: Workflow<NanocodexCiParams>;
  CI_SOURCE: R2Bucket;
  BACKUP_BUCKET: R2Bucket;
  SANDBOX: DurableObjectNamespace<import("@cloudflare/ci/worker").CiSandbox>;
};

type CiLaneLocator =
  | { type: "master" }
  | { type: "pull_request"; number: number };

type PublishResult =
  | {
    type: "accepted" | "replay";
    publication: CiSourcePublication;
    run: CiRunRecord;
    reusedWorkflow: boolean;
  }
  | { type: "cas_conflict"; currentHead: string | null }
  | { type: "publication_conflict" }
  | { type: "pull_request_closed"; record: CiPullRequestClosedRecord }
  | { type: "reopen_conflict" }
  | { type: "source_conflict" }
  | { type: "source_retiring" }
  | { type: "publication_lease_invalid" }
  | { type: "release_commit_reserved"; reservation: CiReleaseCommitReservation }
  | { type: "repository_state_invalid" };

export class CiRepository {
  readonly #state: DurableObjectState;
  readonly #env: CiRepositoryEnv;
  readonly #activeSourceGcClaims = new Set<string>();

  constructor(state: DurableObjectState, env: CiRepositoryEnv) {
    this.#state = state;
    this.#env = env;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/state" && request.method === "GET") {
      return this.#laneState({ type: "master" });
    }
    const pullRequestState = url.pathname.match(
      /^\/pull-requests\/([1-9][0-9]*)\/state$/,
    );
    if (pullRequestState && request.method === "GET") {
      const number = pullRequestNumber(pullRequestState[1]!);
      return number == null
        ? Response.json({ error: "not_found" }, { status: 404 })
        : this.#laneState({ type: "pull_request", number });
    }
    if (pullRequestState && request.method === "DELETE") {
      const number = pullRequestNumber(pullRequestState[1]!);
      return number == null
        ? Response.json({ error: "not_found" }, { status: 404 })
        : this.#closePullRequest(request, number);
    }
    const retainedPullRequest = url.pathname.match(
      /^\/pull-requests\/([1-9][0-9]*)\/publications\/([a-f0-9]{40})$/,
    );
    if (retainedPullRequest && request.method === "GET") {
      const number = pullRequestNumber(retainedPullRequest[1]!);
      return number == null
        ? Response.json({ error: "not_found" }, { status: 404 })
        : this.#retainedLaneState(
          { type: "pull_request", number },
          retainedPullRequest[2]!,
        );
    }
    const retainedMaster = url.pathname.match(
      /^\/master\/publications\/([a-f0-9]{40})$/,
    );
    if (retainedMaster && request.method === "GET") {
      return this.#retainedLaneState({ type: "master" }, retainedMaster[1]!);
    }
    const publicationMatch = url.pathname.match(/^\/publications\/([a-f0-9]{40})$/);
    if (publicationMatch && request.method === "GET") {
      const head = publicationMatch[1]!;
      const [master, source] = await Promise.all([
        this.#state.storage.get<CiSourcePublication>(
          publicationStorageKey({ type: "master" }, head),
        ),
        this.#state.storage.get<CiSourceRecord>(`${SOURCE_PREFIX}${head}`),
      ]);
      if (!source) return Response.json({ error: "not_found" }, { status: 404 });
      if (!validSourceRecord(source, head)) {
        return Response.json({ error: "repository_state_invalid" }, { status: 503 });
      }
      const publication = master ?? source.firstPublication;
      return isCiSourcePublication(publication) && sameSource(publication, source.firstPublication)
        ? Response.json(publication)
        : Response.json({ error: "repository_state_invalid" }, { status: 503 });
    }
    const runMatch = url.pathname.match(/^\/runs\/([a-f0-9]{40})$/);
    if (runMatch && request.method === "GET") {
      const head = runMatch[1]!;
      const run = await this.#state.storage.get<CiRunRecord>(`${RUN_PREFIX}${head}`);
      if (!run) return Response.json({ error: "not_found" }, { status: 404 });
      return validRun(run, head)
        ? Response.json(run)
        : Response.json({ error: "repository_state_invalid" }, { status: 503 });
    }
    if (url.pathname === "/runs" && request.method === "GET") {
      const records = await this.#state.storage.list<CiRunRecord>({ prefix: RUN_PREFIX });
      const runs = [...records.values()]
        .filter((run) => validRun(run, run.head))
        .sort((left, right) => right.publishedAt.localeCompare(left.publishedAt))
        .slice(0, 50);
      return Response.json({
        runs,
        retainedCount: records.size,
        retentionLimit: MAX_RETAINED_RUNS,
      });
    }
    const distributionLease = url.pathname.match(
      /^\/leases\/distribution\/([a-f0-9]{40})$/,
    );
    if (distributionLease && request.method === "POST") {
      return this.#acquireDistributionLease(request, distributionLease[1]!);
    }
    const publicationLease = url.pathname.match(
      /^\/leases\/publication\/([a-f0-9]{40})$/,
    );
    if (publicationLease && request.method === "POST") {
      return this.#acquirePublicationLease(request, publicationLease[1]!);
    }
    if (
      url.pathname === "/reservations/release-commit/acquire" &&
      request.method === "POST"
    ) {
      return this.#acquireReleaseCommitReservation(request);
    }
    const releaseCommitReservation = url.pathname.match(
      /^\/reservations\/release-commit\/([^/]+)$/,
    );
    if (releaseCommitReservation && request.method === "DELETE") {
      return this.#releaseReleaseCommitReservation(
        request,
        releaseCommitReservation[1]!,
      );
    }
    const releaseCommitReservationHeartbeat = url.pathname.match(
      /^\/reservations\/release-commit\/([^/]+)\/heartbeat$/,
    );
    if (releaseCommitReservationHeartbeat && request.method === "POST") {
      return this.#heartbeatReleaseCommitReservation(
        request,
        releaseCommitReservationHeartbeat[1]!,
      );
    }
    const cancellation = url.pathname.match(/^\/cancellations\/([a-f0-9]{40})$/);
    if (cancellation && request.method === "POST") {
      return this.#requestOperatorCancellation(request, cancellation[1]!);
    }
    const cargoVendorMultipart = url.pathname.match(
      /^\/cargo-vendor\/multipart\/([a-f0-9-]+)$/,
    );
    if (cargoVendorMultipart && request.method === "POST") {
      return this.#createCargoVendorMultipart(request, cargoVendorMultipart[1]!);
    }
    if (url.pathname === "/maintenance/source-gc" && request.method === "POST") {
      try {
        return Response.json(await this.#collectSourceOrphans());
      } catch (cause) {
        console.error("Failed to collect orphaned CI source objects", cause);
        return Response.json({ error: "source_gc_failed" }, { status: 503 });
      }
    }
    if (url.pathname === "/publications" && request.method === "PUT") {
      return this.#publish(request);
    }
    return Response.json({ error: "not_found" }, { status: 404 });
  }

  async alarm(): Promise<void> {
    const now = Date.now();
    const outbox = await this.#state.storage.list<CiOutboxRecord>({ prefix: OUTBOX_PREFIX });
    for (const [key, record] of outbox) {
      if (!validOutboxRecord(key, record)) {
        throw new Error("CI Workflow outbox contains invalid repository state");
      }
    }
    const pending = [...outbox.entries()]
      .filter(([, record]) => dispatchTime(record) <= now)
      .sort(([, left], [, right]) =>
        dispatchTime(left) - dispatchTime(right) ||
        left.run.publishedAt.localeCompare(right.run.publishedAt) ||
        left.run.head.localeCompare(right.run.head)
      )[0];
    if (!pending) {
      const processedCancellation = await this.#processNextCancellation(now);
      if (processedCancellation) {
        await this.#trimHistory().catch((cause) => {
          console.error("Failed to trim retained CI repository history", cause);
        });
      }
      await this.#scheduleNextAlarm(outbox);
      return;
    }
    const [key, storedRecord] = pending;
    const record = await this.#authoritativeOutboxRecord(key, storedRecord);
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
        await this.#wakeCancellation(transaction, record.run.head, true);
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
          await this.#wakeCancellation(transaction, record.run.head, true);
        });
        await this.#scheduleNextAlarm();
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
      await this.#scheduleNextAlarm();
      return;
    }
    await this.#scheduleNextAlarm();
  }

  async #authoritativeOutboxRecord(
    key: string,
    stored: CiOutboxRecord,
  ): Promise<CiOutboxRecord> {
    return this.#state.storage.transaction(async (transaction) => {
      if (!validOutboxRecord(key, stored)) {
        throw new Error("CI Workflow outbox contains invalid repository state");
      }
      const head = stored.run.head;
      const [run, source] = await Promise.all([
        transaction.get<CiRunRecord>(`${RUN_PREFIX}${head}`),
        transaction.get<CiSourceRecord>(`${SOURCE_PREFIX}${head}`),
      ]);
      if (
        !run || !source || !validRun(run, head) || run.state !== "pending" ||
        !sameRun(run, stored.run) || !validSourceRecord(source, head) ||
        source.firstPublication.publishedAt !== run.publishedAt
      ) {
        throw new Error("CI Workflow outbox contains invalid repository state");
      }
      const publication = await exactRetainedFirstPublication(transaction, source);
      if (!publication) {
        throw new Error("CI Workflow outbox contains invalid repository state");
      }
      return {
        version: 1,
        run,
        params: ciWorkflowParams(publication, run.beforeHead),
      };
    });
  }

  async #laneState(lane: CiLaneLocator): Promise<Response> {
    const head = await this.#state.storage.get<string>(pointerStorageKey(lane));
    if (!head) {
      if (lane.type === "pull_request") {
        const closed = await this.#state.storage.get<CiPullRequestClosedRecord>(
          `${PULL_REQUEST_CLOSED_PREFIX}${lane.number}`,
        );
        if (closed) {
          if (!validClosedRecord(closed)) {
            return Response.json({ error: "repository_state_invalid" }, { status: 503 });
          }
          return Response.json({
            error: "pull_request_closed",
            number: lane.number,
            closeId: closed.closeId,
            mergeHead: closed.mergeHead,
            pullRequestHead: closed.pullRequestHead,
            closedAt: closed.closedAt,
          }, { status: 404 });
        }
      }
      return Response.json({ error: "not_published" }, { status: 404 });
    }
    const response = await this.#retainedLaneState(lane, head);
    return response.status === 404
      ? Response.json({ error: "repository_state_invalid" }, { status: 503 })
      : response;
  }

  async #acquireDistributionLease(request: Request, head: string): Promise<Response> {
    const value = await request.json().catch(() => undefined) as {
      workflowId?: unknown;
    } | undefined;
    if (
      !value || !hasExactKeys(value, ["workflowId"]) ||
      typeof value.workflowId !== "string" ||
      !validDistributionWorkflowId(value.workflowId, head)
    ) return Response.json({ error: "invalid_distribution_lease" }, { status: 400 });
    const workflowId = value.workflowId;
    const key = distributionLeaseStorageKey(workflowId);
    const now = Date.now();
    const acquiredAt = new Date(now).toISOString();
    const expiresAt = new Date(now + DISTRIBUTION_LEASE_MS).toISOString();
    const result = await this.#state.storage.transaction(async (transaction) => {
      const [
        source,
        publication,
        run,
        retiring,
        cancellation,
        existing,
      ] = await Promise.all([
        transaction.get<CiSourceRecord>(`${SOURCE_PREFIX}${head}`),
        transaction.get<CiSourcePublication>(
          publicationStorageKey({ type: "master" }, head),
        ),
        transaction.get<CiRunRecord>(`${RUN_PREFIX}${head}`),
        transaction.get<CiRetiringRecord>(`${RETIRING_PREFIX}${head}`),
        transaction.get<CiCancellationRecord>(`${CANCELLATION_PREFIX}${head}`),
        transaction.get<CiDistributionLease>(key),
      ]);
      if (
        !source || !publication || !run ||
        !validSourceRecord(source, head) ||
        !isCiSourcePublication(publication) ||
        !laneMatches(publication, { type: "master" }) ||
        !sameSource(publication, source.firstPublication) ||
        !validRun(run, head) || run.state !== "dispatched"
      ) return { type: "not_retained" as const };
      if (retiring) return { type: "retiring" as const };
      if (cancellation && !validCancellation(cancellation, head)) {
        return { type: "invalid" as const };
      }
      if (
        cancellation &&
        (cancellation.reason === "operator_terminated" || cancellation.state !== "pending")
      ) {
        return { type: "retiring" as const };
      }
      if (existing && validDistributionLease(existing, workflowId)) {
        if (Date.parse(existing.expiresAt) > now) {
          return { type: "replay" as const, lease: existing };
        }
        await transaction.delete(key);
      } else if (existing) {
        return { type: "invalid" as const };
      }
      const lease = {
        version: 1,
        kind: "distribution",
        leaseId: crypto.randomUUID(),
        head,
        workflowId,
        acquiredAt,
        expiresAt,
      } satisfies CiDistributionLease;
      await transaction.put(key, lease);
      if (cancellation?.state === "pending") {
        const nextAttemptAt = laterTimestamp(cancellation.notBefore, expiresAt);
        await transaction.put(`${CANCELLATION_PREFIX}${head}`, {
          ...cancellation,
          nextAttemptAt,
        } satisfies CiCancellationRecord);
        await this.#armTransactionAlarm(transaction, Date.parse(nextAttemptAt));
      }
      return { type: "acquired" as const, lease };
    });
    if (result.type === "not_retained") {
      return Response.json({ error: "lease_head_not_retained" }, { status: 409 });
    }
    if (result.type === "retiring") {
      return Response.json({ error: "source_retiring" }, { status: 409 });
    }
    if (result.type === "invalid") {
      return Response.json({ error: "repository_state_invalid" }, { status: 503 });
    }
    await this.#scheduleNextAlarm();
    return Response.json({ lease: result.lease }, {
      status: result.type === "acquired" ? 201 : 200,
      headers: { "cache-control": "no-store" },
    });
  }

  async #acquirePublicationLease(request: Request, head: string): Promise<Response> {
    const value = await request.json().catch(() => undefined) as {
      publication?: unknown;
    } | undefined;
    if (
      !value || !hasExactKeys(value, ["publication"]) ||
      !isCiSourcePublication(value.publication) || value.publication.head !== head
    ) return Response.json({ error: "invalid_publication_lease" }, { status: 400 });
    const publication = normalizeCiSourcePublication(value.publication);
    const key = `${PUBLICATION_LEASE_PREFIX}${head}`;
    const objectKeys = sourceObjectKeys(publication);
    const now = Date.now();
    const acquiredAt = new Date(now).toISOString();
    const expiresAt = new Date(now + PUBLICATION_LEASE_MS).toISOString();
    const result = await this.#state.storage.transaction(async (transaction) => {
      const [source, retiring, cancellation, existing, ...objectRetiring] = await Promise.all([
        transaction.get<CiSourceRecord>(`${SOURCE_PREFIX}${head}`),
        transaction.get<CiRetiringRecord>(`${RETIRING_PREFIX}${head}`),
        transaction.get<CiCancellationRecord>(`${CANCELLATION_PREFIX}${head}`),
        transaction.get<CiPublicationLeaseRecord>(key),
        ...objectKeys.map((objectKey) =>
          transaction.get<CiSourceObjectRetiringRecord>(sourceObjectRetiringKey(objectKey))
        ),
      ]);
      if (source && (!validSourceRecord(source, head) || !sameSource(source.firstPublication, publication))) {
        return { type: "source_conflict" as const };
      }
      if (retiring) return { type: "retiring" as const };
      if (cancellation && !validCancellation(cancellation, head)) {
        return { type: "invalid" as const };
      }
      if (
        cancellation &&
        (cancellation.reason === "operator_terminated" || cancellation.state !== "pending")
      ) return { type: "retiring" as const };
      for (let index = 0; index < objectRetiring.length; index += 1) {
        const marker = objectRetiring[index];
        if (!marker) continue;
        const objectKey = objectKeys[index]!;
        if (!validSourceObjectRetiring(marker, objectKey)) {
          return { type: "invalid" as const };
        }
        // Only the GC owner that has conclusively reconciled its R2 delete may
        // remove this fence. Publication acquisition never ages it out.
        return { type: "retiring" as const };
      }
      if (existing) {
        if (!validPublicationLeaseRecord(existing, head)) {
          return { type: "invalid" as const };
        }
        if (Date.parse(existing.expiresAt) > now) {
          return sameSource(existing.publication, publication)
            ? { type: "replay" as const, lease: publicPublicationLease(existing) }
            : { type: "source_conflict" as const };
        }
        await transaction.delete(key);
      }
      const lease = {
        version: 1,
        kind: "publication",
        leaseId: crypto.randomUUID(),
        head,
        acquiredAt,
        expiresAt,
        publication,
      } satisfies CiPublicationLeaseRecord;
      await transaction.put(key, lease);
      if (cancellation?.state === "pending") {
        const nextAttemptAt = laterTimestamp(cancellation.notBefore, expiresAt);
        await transaction.put(`${CANCELLATION_PREFIX}${head}`, {
          ...cancellation,
          nextAttemptAt,
        } satisfies CiCancellationRecord);
        await this.#armTransactionAlarm(transaction, Date.parse(nextAttemptAt));
      }
      return { type: "acquired" as const, lease: publicPublicationLease(lease) };
    });
    if (result.type === "source_conflict") {
      return Response.json({ error: "source_conflict" }, { status: 409 });
    }
    if (result.type === "retiring") {
      return Response.json({ error: "source_retiring" }, { status: 409 });
    }
    if (result.type === "invalid") {
      return Response.json({ error: "repository_state_invalid" }, { status: 503 });
    }
    await this.#scheduleNextAlarm();
    return Response.json({ lease: result.lease }, {
      status: result.type === "acquired" ? 201 : 200,
      headers: { "cache-control": "no-store" },
    });
  }

  async #acquireReleaseCommitReservation(request: Request): Promise<Response> {
    const value: unknown = await request.json().catch(() => undefined);
    if (!validReleaseCommitReservationRequest(value)) {
      return Response.json({ error: "invalid_release_commit_reservation" }, {
        status: 400,
        headers: { "cache-control": "no-store" },
      });
    }
    const input = value;
    const nonce = crypto.randomUUID();
    const outcome = await this.#state.storage.transaction(async (transaction) => {
      const now = Date.now();
      const active = await transaction.get<CiReleaseCommitReservation>(
        RELEASE_COMMIT_RESERVATION_KEY,
      );
      if (active) {
        if (!validReleaseCommitReservation(active)) {
          return { type: "invalid" as const };
        }
        if (Date.parse(active.expiresAt) > now) {
          if (!sameReleaseCommitReservationRequest(active, input)) {
            return { type: "conflict" as const };
          }
          const authority = await exactCurrentMasterState(
            transaction,
            input.commit,
          );
          if (authority.type !== "retained") return authority;
          const renewed = {
            ...active,
            renewedAt: new Date(now).toISOString(),
            expiresAt: new Date(now + RELEASE_COMMIT_RESERVATION_MS).toISOString(),
          } satisfies CiReleaseCommitReservation;
          await transaction.put(RELEASE_COMMIT_RESERVATION_KEY, renewed);
          return {
            type: "replayed" as const,
            proof: {
              reservation: renewed,
              publication: authority.publication,
              run: authority.run,
            } satisfies CiReleaseCommitReservationProof,
          };
        }
      }

      const authority = await exactCurrentMasterState(transaction, input.commit);
      if (authority.type !== "retained") return authority;
      const storedGeneration = await transaction.get<number>(
        RELEASE_COMMIT_RESERVATION_GENERATION_KEY,
      );
      const generation = Math.max(storedGeneration ?? 0, active?.generation ?? 0) + 1;
      if (!Number.isSafeInteger(generation)) return { type: "invalid" as const };
      const acquiredAt = new Date(now).toISOString();
      const { version: _version, ...identity } = input;
      const reservation = {
        version: 1,
        kind: "release-commit",
        reservationId: `${generation}.${nonce}`,
        ...identity,
        generation,
        acquiredAt,
        renewedAt: acquiredAt,
        expiresAt: new Date(now + RELEASE_COMMIT_RESERVATION_MS).toISOString(),
      } satisfies CiReleaseCommitReservation;
      await transaction.put(RELEASE_COMMIT_RESERVATION_KEY, reservation);
      await transaction.put(RELEASE_COMMIT_RESERVATION_GENERATION_KEY, generation);
      return {
        type: "acquired" as const,
        proof: {
          reservation,
          publication: authority.publication,
          run: authority.run,
        } satisfies CiReleaseCommitReservationProof,
      };
    });

    if (outcome.type === "stale") {
      return Response.json({ error: "release_head_stale" }, {
        status: 409,
        headers: { "cache-control": "no-store" },
      });
    }
    if (outcome.type === "conflict") {
      return Response.json({ error: "release_commit_reservation_conflict" }, {
        status: 409,
        headers: { "cache-control": "no-store" },
      });
    }
    if (outcome.type === "invalid") {
      return Response.json({ error: "repository_state_invalid" }, {
        status: 503,
        headers: { "cache-control": "no-store" },
      });
    }
    return Response.json(outcome.proof, {
      status: outcome.type === "acquired" ? 201 : 200,
      headers: { "cache-control": "no-store" },
    });
  }

  async #heartbeatReleaseCommitReservation(
    request: Request,
    reservationId: string,
  ): Promise<Response> {
    const value: unknown = await request.json().catch(() => undefined);
    if (
      !validReleaseCommitReservationMutation(value) ||
      !FENCED_ID_PATTERN.test(reservationId) ||
      reservationId.split(".", 1)[0] !== String(value.generation)
    ) {
      return Response.json({ error: "invalid_release_commit_reservation" }, {
        status: 400,
        headers: { "cache-control": "no-store" },
      });
    }
    const outcome = await this.#state.storage.transaction(async (transaction) => {
      const now = Date.now();
      const active = await transaction.get<CiReleaseCommitReservation>(
        RELEASE_COMMIT_RESERVATION_KEY,
      );
      if (!active) return { type: "not_held" as const };
      if (!validReleaseCommitReservation(active)) return { type: "invalid" as const };
      if (
        Date.parse(active.expiresAt) <= now || active.reservationId !== reservationId ||
        active.owner !== value.owner || active.generation !== value.generation
      ) return { type: "not_held" as const };
      const authority = await exactCurrentMasterState(transaction, active.commit);
      if (authority.type !== "retained") return { type: "invalid" as const };
      const renewed = {
        ...active,
        renewedAt: new Date(now).toISOString(),
        expiresAt: new Date(now + RELEASE_COMMIT_RESERVATION_MS).toISOString(),
      } satisfies CiReleaseCommitReservation;
      await transaction.put(RELEASE_COMMIT_RESERVATION_KEY, renewed);
      return { type: "renewed" as const, reservation: renewed };
    });
    if (outcome.type === "invalid") {
      return Response.json({ error: "repository_state_invalid" }, {
        status: 503,
        headers: { "cache-control": "no-store" },
      });
    }
    return outcome.type === "not_held"
      ? Response.json({ error: "release_commit_reservation_not_held" }, {
        status: 409,
        headers: { "cache-control": "no-store" },
      })
      : Response.json({ reservation: outcome.reservation }, {
        headers: { "cache-control": "no-store" },
      });
  }

  async #releaseReleaseCommitReservation(
    request: Request,
    reservationId: string,
  ): Promise<Response> {
    const value: unknown = await request.json().catch(() => undefined);
    if (
      !validReleaseCommitReservationMutation(value) ||
      !FENCED_ID_PATTERN.test(reservationId) ||
      reservationId.split(".", 1)[0] !== String(value.generation)
    ) {
      return Response.json({ error: "invalid_release_commit_reservation" }, {
        status: 400,
        headers: { "cache-control": "no-store" },
      });
    }
    const outcome = await this.#state.storage.transaction(async (transaction) => {
      const active = await transaction.get<CiReleaseCommitReservation>(
        RELEASE_COMMIT_RESERVATION_KEY,
      );
      if (!active) return "released" as const;
      if (!validReleaseCommitReservation(active)) return "invalid" as const;
      if (
        active.reservationId !== reservationId ||
        active.generation !== value.generation
      ) return "released" as const;
      if (active.owner !== value.owner) return "not_held" as const;
      await transaction.delete(RELEASE_COMMIT_RESERVATION_KEY);
      return "released" as const;
    });
    if (outcome === "invalid") {
      return Response.json({ error: "repository_state_invalid" }, {
        status: 503,
        headers: { "cache-control": "no-store" },
      });
    }
    if (outcome === "not_held") {
      return Response.json({ error: "release_commit_reservation_not_held" }, {
        status: 409,
        headers: { "cache-control": "no-store" },
      });
    }
    return new Response(null, {
      status: 204,
      headers: { "cache-control": "no-store" },
    });
  }

  async #createCargoVendorMultipart(
    request: Request,
    requestId: string,
  ): Promise<Response> {
    const value = await request.json().catch(() => undefined);
    if (!isCargoVendorMultipartInput(value) || value.requestId !== requestId) {
      return Response.json({ error: "invalid_cargo_vendor_multipart" }, { status: 400 });
    }
    const input = value;
    const storageKey = `${CARGO_VENDOR_MULTIPART_PREFIX}${requestId}`;
    const now = Date.now();
    const reservation = await this.#state.storage.transaction(async (transaction) => {
      const existing = await transaction.get<CiCargoVendorMultipartRecord>(storageKey);
      if (existing) {
        if (!validCargoVendorMultipartRecord(existing, requestId)) {
          return { type: "invalid" as const };
        }
        if (!sameCargoVendorMultipartRequest(existing, input)) {
          return { type: "conflict" as const };
        }
        if (existing.state !== "creating") {
          return { type: "replay" as const, record: existing };
        }
        if (Date.parse(existing.recoverAfter) > now) {
          return { type: "uncertain" as const, record: existing };
        }
      }
      const startedAt = new Date(now).toISOString();
      const creating = {
        ...input,
        stagingId: requestId,
        state: "creating",
        startedAt,
        recoverAfter: new Date(now + R2_INCOMPLETE_MULTIPART_ABORT_MS).toISOString(),
      } satisfies CiCargoVendorMultipartRecord;
      await transaction.put(storageKey, creating);
      return { type: "create" as const, record: creating };
    });
    if (reservation.type === "invalid") {
      return Response.json({ error: "repository_state_invalid" }, { status: 503 });
    }
    if (reservation.type === "conflict") {
      return Response.json({ error: "cargo_vendor_multipart_request_conflict" }, {
        status: 409,
        headers: { "cache-control": "no-store" },
      });
    }

    const key = cargoVendorBundleKey(input.cargoLockBlob, input.bundleSha256);
    let canonical: R2Object | null;
    try {
      canonical = await this.#env.CI_SOURCE.head(key);
    } catch (cause) {
      return Response.json({
        error: "cargo_vendor_multipart_unavailable",
        detail: boundedError(cause),
      }, { status: 503, headers: { "cache-control": "no-store" } });
    }
    if (canonical) {
      if (!matchesCargoVendorObject(canonical, input)) {
        return Response.json({ error: "immutable_object_conflict" }, {
          status: 409,
          headers: { "cache-control": "no-store" },
        });
      }
      const complete = {
        ...input,
        stagingId: requestId,
        state: "complete",
        completedAt: new Date().toISOString(),
      } satisfies CiCargoVendorMultipartRecord;
      await this.#state.storage.put(storageKey, complete);
      return Response.json({
        requestId,
        key,
        cargoLockBlob: input.cargoLockBlob,
        size: input.size,
        sha256: input.bundleSha256,
        uploaded: false,
      }, { headers: { "cache-control": "no-store" } });
    }
    if (reservation.type === "replay") {
      if (reservation.record.state === "complete") {
        return Response.json({ error: "cargo_vendor_bundle_missing" }, {
          status: 503,
          headers: { "cache-control": "no-store" },
        });
      }
      return Response.json(cargoVendorMultipartResponse(reservation.record), {
        headers: { "cache-control": "no-store" },
      });
    }
    if (reservation.type === "uncertain") {
      return Response.json({
        error: "cargo_vendor_multipart_create_uncertain",
        requestId,
        retryAt: reservation.record.recoverAfter,
      }, {
        status: 503,
        headers: {
          "cache-control": "no-store",
          "retry-after": String(Math.max(
            1,
            Math.ceil((Date.parse(reservation.record.recoverAfter) - now) / 1_000),
          )),
        },
      });
    }

    const stagingKey = cargoVendorMultipartStagingKey(input);
    let upload: R2MultipartUpload;
    try {
      upload = await this.#env.CI_SOURCE.createMultipartUpload(stagingKey, {
        httpMetadata: {
          contentType: "application/gzip",
          cacheControl: "public, max-age=31536000, immutable",
        },
        customMetadata: {
          sha256: input.bundleSha256,
          size: String(input.size),
          kind: "cargo-vendor-staging",
          cargoLockBlob: input.cargoLockBlob,
          canonicalKey: key,
          partSize: String(input.partSize),
          partCount: String(input.partCount),
          requestId,
        },
      });
    } catch (cause) {
      return Response.json({
        error: "cargo_vendor_multipart_create_uncertain",
        requestId,
        retryAt: reservation.record.recoverAfter,
        detail: boundedError(cause),
      }, { status: 503, headers: { "cache-control": "no-store" } });
    }
    if (
      upload.key !== stagingKey || typeof upload.uploadId !== "string" ||
      upload.uploadId.length === 0 || upload.uploadId.length > 1_024
    ) {
      return Response.json({
        error: "cargo_vendor_multipart_create_uncertain",
        requestId,
        retryAt: reservation.record.recoverAfter,
      }, { status: 503, headers: { "cache-control": "no-store" } });
    }
    const ready = {
      ...input,
      stagingId: requestId,
      state: "ready",
      startedAt: reservation.record.startedAt,
      readyAt: new Date().toISOString(),
      uploadId: upload.uploadId,
    } satisfies CiCargoVendorMultipartRecord;
    const persisted = await this.#state.storage.transaction(async (transaction) => {
      const current = await transaction.get<CiCargoVendorMultipartRecord>(storageKey);
      if (
        !current || !validCargoVendorMultipartRecord(current, requestId) ||
        current.state !== "creating" || current.startedAt !== reservation.record.startedAt ||
        !sameCargoVendorMultipartRequest(current, input)
      ) return false;
      await transaction.put(storageKey, ready);
      return true;
    });
    if (!persisted) {
      return Response.json({ error: "repository_state_invalid" }, {
        status: 503,
        headers: { "cache-control": "no-store" },
      });
    }
    return Response.json(cargoVendorMultipartResponse(ready), {
      headers: { "cache-control": "no-store" },
    });
  }

  async #requestOperatorCancellation(request: Request, head: string): Promise<Response> {
    const value = await request.json().catch(() => undefined) as {
      workflowId?: unknown;
    } | undefined;
    if (
      !value || !hasExactKeys(value, ["workflowId"]) ||
      value.workflowId !== `ci-${head}`
    ) return Response.json({ error: "invalid_cancellation" }, { status: 400 });
    const key = `${CANCELLATION_PREFIX}${head}`;
    const now = Date.now();
    const requestedAt = new Date(now).toISOString();
    const result = await this.#state.storage.transaction(async (transaction) => {
      const [run, source, existing, retiring] = await Promise.all([
        transaction.get<CiRunRecord>(`${RUN_PREFIX}${head}`),
        transaction.get<CiSourceRecord>(`${SOURCE_PREFIX}${head}`),
        transaction.get<CiCancellationRecord>(key),
        transaction.get<CiRetiringRecord>(`${RETIRING_PREFIX}${head}`),
      ]);
      if (existing) {
        if (!validCancellation(existing, head)) return { type: "invalid" as const };
        if (
          existing.reason === "operator_terminated" ||
          existing.state !== "pending"
        ) return { type: "replay" as const, record: existing };
      }
      if (
        !run || !source || !validRun(run, head) || run.workflowId !== value.workflowId ||
        !validSourceRecord(source, head)
      ) return { type: "not_found" as const };
      if (retiring) return { type: "retiring" as const };
      const record = {
        version: 1,
        head,
        workflowId: value.workflowId,
        reason: "operator_terminated",
        requestedAt,
        notBefore: requestedAt,
        state: "pending",
        attempts: existing?.attempts ?? 0,
        nextAttemptAt: requestedAt,
      } satisfies CiCancellationRecord;
      await transaction.delete(`${OUTBOX_PREFIX}${head}`);
      await transaction.put(key, record);
      await this.#armTransactionAlarm(transaction, now);
      return { type: "accepted" as const, record };
    });
    if (result.type === "not_found") {
      return Response.json({ error: "not_published" }, { status: 404 });
    }
    if (result.type === "retiring") {
      return Response.json({ error: "source_retiring" }, { status: 409 });
    }
    if (result.type === "invalid") {
      return Response.json({ error: "repository_state_invalid" }, { status: 503 });
    }
    await this.#scheduleNextAlarm();
    return cancellationResponse(result.record, result.record.state === "complete" ? 200 : 202);
  }

  async #closePullRequest(request: Request, number: number): Promise<Response> {
    const value = await request.json().catch(() => undefined) as {
      closeId?: unknown;
      expectedMergeHead?: unknown;
      expectedPullRequestHead?: unknown;
    } | undefined;
    if (
      !value || typeof value.closeId !== "string" ||
      !CLOSE_ID_PATTERN.test(value.closeId) ||
      !isSha1(value.expectedMergeHead) ||
      !isSha1(value.expectedPullRequestHead)
    ) {
      return Response.json({ error: "invalid_close" }, { status: 400 });
    }
    const closeId = value.closeId;
    const expectedMergeHead = value.expectedMergeHead;
    const expectedPullRequestHead = value.expectedPullRequestHead;
    const lane = { type: "pull_request" as const, number };
    const pointerKey = pointerStorageKey({ type: "pull_request", number });
    const closedKey = `${PULL_REQUEST_CLOSED_PREFIX}${number}`;
    const operationKey = `${PULL_REQUEST_CLOSE_OPERATION_PREFIX}${number}:${closeId}`;
    const result = await this.#state.storage.transaction(async (transaction) => {
      const [currentHead, closed, operation] = await Promise.all([
        transaction.get<string>(pointerKey),
        transaction.get<CiPullRequestClosedRecord>(closedKey),
        transaction.get<CiPullRequestClosedRecord>(operationKey),
      ]);
      if (operation) {
        if (!validClosedRecord(operation)) {
          return { type: "repository_state_invalid" as const };
        }
        return operation.mergeHead === expectedMergeHead &&
            operation.pullRequestHead === expectedPullRequestHead
          ? { type: "replay" as const, record: operation }
          : { type: "operation_conflict" as const };
      }
      if (!currentHead) {
        if (closed && !validClosedRecord(closed)) {
          return { type: "repository_state_invalid" as const };
        }
        return closed
          ? {
            type: "conflict" as const,
            currentMergeHead: closed.mergeHead,
            currentPullRequestHead: closed.pullRequestHead,
          }
          : { type: "not_found" as const };
      }
      const publication = await transaction.get<CiSourcePublication>(
        publicationStorageKey(lane, currentHead),
      );
      if (!publication || !isCiSourcePublication(publication) || !laneMatches(publication, lane)) {
        return { type: "repository_state_invalid" as const };
      }
      const currentLane = ciSourceLane(publication);
      if (currentLane.type !== "pull_request") {
        return { type: "repository_state_invalid" as const };
      }
      if (
        currentLane.mergeHead !== expectedMergeHead ||
        currentLane.pullRequestHead !== expectedPullRequestHead
      ) {
        return {
          type: "conflict" as const,
          currentMergeHead: currentLane.mergeHead,
          currentPullRequestHead: currentLane.pullRequestHead,
        };
      }
      const record = {
        version: 1,
        closeId,
        mergeHead: currentLane.mergeHead,
        pullRequestHead: currentLane.pullRequestHead,
        closedAt: new Date().toISOString(),
      } satisfies CiPullRequestClosedRecord;
      await this.#recordPullRequestCancellation(
        transaction,
        currentHead,
        number,
        "pull_request_closed",
      );
      await transaction.delete(pointerKey);
      await transaction.put({ [closedKey]: record, [operationKey]: record });
      return { type: "closed" as const, record };
    });
    if (result.type === "conflict") {
      return Response.json({
        error: "publication_conflict",
        currentMergeHead: result.currentMergeHead,
        currentPullRequestHead: result.currentPullRequestHead,
      }, { status: 409 });
    }
    if (result.type === "repository_state_invalid") {
      return Response.json({ error: "repository_state_invalid" }, { status: 503 });
    }
    if (result.type === "operation_conflict") {
      return Response.json({ error: "close_operation_conflict" }, { status: 409 });
    }
    if (result.type === "not_found") {
      return Response.json({ error: "not_published" }, { status: 404 });
    }
    await this.#trimHistory().catch((cause) => {
      console.error("Failed to trim retained CI repository history", cause);
    });
    await this.#scheduleNextAlarm();
    return Response.json({
      closed: true,
      number,
      closeId: result.record.closeId,
      mergeHead: result.record.mergeHead,
      pullRequestHead: result.record.pullRequestHead,
      closedAt: result.record.closedAt,
      replay: result.type === "replay",
    });
  }

  async #retainedLaneState(lane: CiLaneLocator, head: string): Promise<Response> {
    const [publication, run, source] = await Promise.all([
      this.#state.storage.get<CiSourcePublication>(publicationStorageKey(lane, head)),
      this.#state.storage.get<CiRunRecord>(`${RUN_PREFIX}${head}`),
      this.#state.storage.get<CiSourceRecord>(`${SOURCE_PREFIX}${head}`),
    ]);
    if (!publication) return Response.json({ error: "not_found" }, { status: 404 });
    if (
      !run || !source ||
      !isCiSourcePublication(publication) ||
      !laneMatches(publication, lane) ||
      !validRun(run, head) ||
      !validSourceRecord(source, head) ||
      !sameSource(publication, source.firstPublication)
    ) return Response.json({ error: "repository_state_invalid" }, { status: 503 });
    return Response.json({ publication, run } satisfies CiLaneState);
  }

  async #workflowExists(workflowId: string): Promise<boolean> {
    try {
      const status = await (await this.#env.CI_WORKFLOW.get(workflowId)).status();
      return status.status !== "unknown";
    } catch {
      return false;
    }
  }

  async #scheduleNextAlarm(
    knownOutbox?: Map<string, CiOutboxRecord>,
  ): Promise<void> {
    const [outbox, cancellations] = await Promise.all([
      knownOutbox ?? this.#state.storage.list<CiOutboxRecord>({ prefix: OUTBOX_PREFIX }),
      this.#state.storage.list<CiCancellationRecord>({ prefix: CANCELLATION_PREFIX }),
    ]);
    const times = [...outbox.values()].map(dispatchTime);
    for (const [key, cancellation] of cancellations) {
      const head = key.slice(CANCELLATION_PREFIX.length);
      if (!validCancellation(cancellation, head)) {
        throw new Error("CI cancellation outbox contains invalid repository state");
      }
      if (cancellation.state !== "complete" && cancellation.nextAttemptAt) {
        times.push(Date.parse(cancellation.nextAttemptAt));
      }
    }
    if (times.length === 0) return;
    await this.#state.storage.setAlarm(Math.max(Date.now(), Math.min(...times)));
  }

  async #wakeCancellation(
    transaction: DurableObjectTransaction,
    head: string,
    refreshGrace = false,
  ): Promise<void> {
    const key = `${CANCELLATION_PREFIX}${head}`;
    const cancellation = await transaction.get<CiCancellationRecord>(key);
    if (!cancellation) return;
    if (!validCancellation(cancellation, head)) {
      throw new Error("CI cancellation outbox contains invalid repository state");
    }
    if (cancellation.state !== "pending") return;
    const now = Date.now();
    const requestedAt = new Date(now).toISOString();
    if (cancellation.reason === "operator_terminated") {
      await transaction.put(key, { ...cancellation, nextAttemptAt: requestedAt });
      await this.#armTransactionAlarm(transaction, now);
      return;
    }
    const notBefore = new Date(now + PULL_REQUEST_CANCELLATION_GRACE_MS).toISOString();
    const nextAttemptAt = refreshGrace
      ? notBefore
      : laterTimestamp(cancellation.notBefore, requestedAt);
    await transaction.put(key, {
      ...cancellation,
      ...(refreshGrace ? { requestedAt, notBefore } : {}),
      nextAttemptAt,
    } satisfies CiCancellationRecord);
    await this.#armTransactionAlarm(transaction, Date.parse(nextAttemptAt));
  }

  async #recordPullRequestCancellation(
    transaction: DurableObjectTransaction,
    head: string,
    pullRequest: number,
    reason: "pull_request_superseded" | "pull_request_closed",
    replacementHead?: string,
  ): Promise<void> {
    const key = `${CANCELLATION_PREFIX}${head}`;
    const existing = await transaction.get<CiCancellationRecord>(key);
    if (existing) {
      if (!validCancellation(existing, head)) {
        throw new Error("CI cancellation outbox contains invalid repository state");
      }
      if (existing.reason === "operator_terminated") return;
      if (existing.state === "pending") {
        const requested = Date.now();
        const notBefore = new Date(
          requested + PULL_REQUEST_CANCELLATION_GRACE_MS,
        ).toISOString();
        await transaction.put(key, {
          ...existing,
          reason,
          pullRequest,
          requestedAt: new Date(requested).toISOString(),
          notBefore,
          ...(replacementHead ? { replacementHead } : {}),
          nextAttemptAt: notBefore,
        } satisfies CiCancellationRecord);
        await this.#armTransactionAlarm(transaction, Date.parse(notBefore));
      }
      return;
    }
    const requested = Date.now();
    const notBefore = new Date(requested + PULL_REQUEST_CANCELLATION_GRACE_MS).toISOString();
    await transaction.put(key, {
      version: 1,
      head,
      workflowId: `ci-${head}`,
      reason,
      pullRequest,
      requestedAt: new Date(requested).toISOString(),
      notBefore,
      state: "pending",
      attempts: 0,
      ...(replacementHead ? { replacementHead } : {}),
      nextAttemptAt: notBefore,
    } satisfies CiCancellationRecord);
    await this.#armTransactionAlarm(transaction, Date.parse(notBefore));
  }

  async #armTransactionAlarm(
    transaction: DurableObjectTransaction,
    timestamp: number,
  ): Promise<void> {
    const current = await transaction.getAlarm();
    if (current == null || current > timestamp) await transaction.setAlarm(timestamp);
  }

  async #moveLaneCancellation(
    transaction: DurableObjectTransaction,
    lane: CiLaneLocator,
    currentHead: string | undefined,
    nextHead: string,
    nextCancellation: CiCancellationRecord | undefined,
  ): Promise<void> {
    if (currentHead && currentHead !== nextHead) {
      if (lane.type === "pull_request") {
        await this.#recordPullRequestCancellation(
          transaction,
          currentHead,
          lane.number,
          "pull_request_superseded",
          nextHead,
        );
      } else {
        await this.#wakeCancellation(transaction, currentHead, true);
      }
    }
    if (!nextCancellation) return;
    if (!validCancellation(nextCancellation, nextHead)) {
      throw new Error("CI cancellation outbox contains invalid repository state");
    }
    if (nextCancellation.state !== "pending") return;
    const protectedCancellation = { ...nextCancellation };
    delete protectedCancellation.nextAttemptAt;
    await transaction.put(`${CANCELLATION_PREFIX}${nextHead}`, protectedCancellation);
  }

  async #processNextCancellation(now: number): Promise<boolean> {
    const cancellations = await this.#state.storage.list<CiCancellationRecord>({
      prefix: CANCELLATION_PREFIX,
    });
    const due = [...cancellations.entries()].filter(([key, cancellation]) => {
      const head = key.slice(CANCELLATION_PREFIX.length);
      if (!validCancellation(cancellation, head)) {
        throw new Error("CI cancellation outbox contains invalid repository state");
      }
      return cancellation.state !== "complete" && cancellation.nextAttemptAt != null &&
        Date.parse(cancellation.nextAttemptAt) <= now;
    }).sort(([, left], [, right]) =>
      Date.parse(left.nextAttemptAt!) - Date.parse(right.nextAttemptAt!) ||
      left.requestedAt.localeCompare(right.requestedAt) ||
      left.head.localeCompare(right.head)
    )[0];
    if (!due) return false;
    const [key, observed] = due;
    let cancellation: CiCancellationRecord | undefined;
    if (observed.state === "pending") {
      cancellation = await this.#state.storage.transaction(async (transaction) => {
        const current = await transaction.get<CiCancellationRecord>(key);
        if (!current || !validCancellation(current, observed.head) ||
          current.state !== "pending") return undefined;
        const [
          masterHead,
          pullRequestHeads,
          outbox,
          leases,
          publicationLeases,
          run,
          retiring,
        ] =
          await Promise.all([
            transaction.get<string>(MASTER_HEAD_KEY),
            transaction.list<string>({ prefix: PULL_REQUEST_HEAD_PREFIX }),
            transaction.list<CiOutboxRecord>({ prefix: OUTBOX_PREFIX }),
            transaction.list<CiDistributionLease>({ prefix: DISTRIBUTION_LEASE_PREFIX }),
            transaction.list<CiPublicationLeaseRecord>({ prefix: PUBLICATION_LEASE_PREFIX }),
            transaction.get<CiRunRecord>(`${RUN_PREFIX}${current.head}`),
            transaction.get<CiRetiringRecord>(`${RETIRING_PREFIX}${current.head}`),
          ]);
        if (!run || !validRun(run, current.head) || run.workflowId !== current.workflowId) {
          throw new Error("CI cancellation references an invalid run");
        }
        if (current.reason === "operator_terminated") {
          if (retiring) {
            await transaction.put(key, {
              ...current,
              nextAttemptAt: new Date(now + 1_000).toISOString(),
            } satisfies CiCancellationRecord);
            await this.#armTransactionAlarm(transaction, now + 1_000);
            return undefined;
          }
          const claimed = {
            ...current,
            state: "terminating",
            attempts: current.attempts + 1,
            claimId: crypto.randomUUID(),
            claimedAt: new Date(now).toISOString(),
          } satisfies CiCancellationRecord;
          delete claimed.nextAttemptAt;
          delete claimed.lastError;
          await transaction.put(key, claimed);
          return claimed;
        }
        if (
          (masterHead != null && !isSha1(masterHead)) ||
          [...pullRequestHeads.values()].some((head) => !isSha1(head)) ||
          [...outbox.entries()].some(([outboxKey, record]) =>
            !validOutboxRecord(outboxKey, record)
          )
        ) throw new Error("CI cancellation cannot prove invalid repository references absent");
        const unexpiredLeases: CiDistributionLease[] = [];
        const unexpiredPublicationLeases: CiPublicationLeaseRecord[] = [];
        const expiredLeaseKeys: string[] = [];
        let expiredReference = false;
        for (const [leaseKey, lease] of leases) {
          if (!validDistributionLeaseStorageEntry(leaseKey, lease)) {
            throw new Error("CI distribution lease contains invalid repository state");
          }
          if (Date.parse(lease.expiresAt) <= now) {
            expiredLeaseKeys.push(leaseKey);
            expiredReference ||= lease.head === current.head;
          }
          else if (lease.head === current.head) unexpiredLeases.push(lease);
        }
        for (const [leaseKey, lease] of publicationLeases) {
          if (!validPublicationLeaseStorageEntry(leaseKey, lease)) {
            throw new Error("CI publication lease contains invalid repository state");
          }
          if (Date.parse(lease.expiresAt) <= now) {
            expiredLeaseKeys.push(leaseKey);
            expiredReference ||= lease.head === current.head;
          } else if (lease.head === current.head) {
            unexpiredPublicationLeases.push(lease);
          }
        }
        if (expiredLeaseKeys.length > 0) await transaction.delete(expiredLeaseKeys);
        const referenced = masterHead === current.head ||
          [...pullRequestHeads.values()].some((head) => head === current.head) ||
          [...outbox.values()].some(({ run: pendingRun }) => pendingRun.head === current.head);
        if (retiring || referenced) {
          const dormant = { ...current };
          delete dormant.nextAttemptAt;
          await transaction.put(key, dormant);
          return undefined;
        }
        if (unexpiredLeases.length > 0 || unexpiredPublicationLeases.length > 0) {
          const nextAttemptAt = [
            ...unexpiredLeases.map(({ expiresAt }) => expiresAt),
            ...unexpiredPublicationLeases.map(({ expiresAt }) => expiresAt),
          ].sort()[0]!;
          await transaction.put(key, {
            ...current,
            nextAttemptAt,
          } satisfies CiCancellationRecord);
          await this.#armTransactionAlarm(transaction, Date.parse(nextAttemptAt));
          return undefined;
        }
        if (expiredReference) {
          const requestedAt = new Date(now).toISOString();
          const notBefore = new Date(
            now + PULL_REQUEST_CANCELLATION_GRACE_MS,
          ).toISOString();
          await transaction.put(key, {
            ...current,
            requestedAt,
            notBefore,
            nextAttemptAt: notBefore,
          } satisfies CiCancellationRecord);
          await this.#armTransactionAlarm(transaction, Date.parse(notBefore));
          return undefined;
        }
        if (Date.parse(current.notBefore) > now) {
          await transaction.put(key, { ...current, nextAttemptAt: current.notBefore });
          await this.#armTransactionAlarm(transaction, Date.parse(current.notBefore));
          return undefined;
        }
        const claimed = {
          ...current,
          state: "terminating",
          attempts: current.attempts + 1,
          claimId: crypto.randomUUID(),
          claimedAt: new Date(now).toISOString(),
        } satisfies CiCancellationRecord;
        delete claimed.nextAttemptAt;
        delete claimed.lastError;
        await transaction.put(key, claimed);
        return claimed;
      });
    } else {
      cancellation = await this.#state.storage.transaction(async (transaction) => {
        const current = await transaction.get<CiCancellationRecord>(key);
        if (!current || !validCancellation(current, observed.head) ||
          current.state !== "terminating") return undefined;
        const retrying = { ...current, attempts: current.attempts + 1 };
        delete retrying.nextAttemptAt;
        await transaction.put(key, retrying);
        return retrying;
      });
    }
    if (!cancellation) return false;
    await this.#executeCancellation(cancellation);
    return true;
  }

  async #executeCancellation(cancellation: CiCancellationRecord): Promise<void> {
    const markerKey = terminationMarkerKey(cancellation.head);
    try {
      const terminal = await this.#runHasTerminalEvidence(
        cancellation.head,
        cancellation.workflowId,
      );
      const [run, outbox] = await Promise.all([
        this.#state.storage.get<CiRunRecord>(`${RUN_PREFIX}${cancellation.head}`),
        this.#state.storage.get<CiOutboxRecord>(`${OUTBOX_PREFIX}${cancellation.head}`),
      ]);
      if (!run || !validRun(run, cancellation.head)) {
        throw new Error("CI cancellation references an invalid run");
      }
      const undispatched = cancellation.reason === "operator_terminated" &&
        run.state === "pending" && !outbox && !await this.#workflowExists(cancellation.workflowId);
      if (!terminal) {
        await this.#env.BACKUP_BUCKET.put(markerKey, JSON.stringify({
          version: 1,
          status: "requested",
          head: cancellation.head,
          workflowId: cancellation.workflowId,
          reason: cancellation.reason,
          pullRequest: cancellation.pullRequest,
          claimId: cancellation.claimId,
          requestedAt: cancellation.requestedAt,
        }), {
          httpMetadata: { contentType: "application/json" },
          customMetadata: { kind: "ci-run-termination", head: cancellation.head },
        });
        if (!undispatched) {
          try {
            await (await this.#env.CI_WORKFLOW.get(cancellation.workflowId)).terminate();
          } catch (cause) {
            if (!await this.#runHasTerminalEvidence(cancellation.head, cancellation.workflowId)) {
              throw cause;
            }
          }
          if (!await this.#runHasTerminalEvidence(cancellation.head, cancellation.workflowId)) {
            throw new Error("CI Workflow termination has not reached a terminal state");
          }
        }
      }
      const cleanup = await terminateActiveSandboxes(this.#env, cancellation.head);
      if (cleanup.failed.length > 0) {
        throw new Error(`failed to terminate ${cleanup.failed.length} active CI sandbox(es)`);
      }
      const completedAt = new Date().toISOString();
      await this.#env.BACKUP_BUCKET.put(markerKey, JSON.stringify({
        version: 1,
        status: "complete",
        head: cancellation.head,
        workflowId: cancellation.workflowId,
        reason: cancellation.reason,
        pullRequest: cancellation.pullRequest,
        claimId: cancellation.claimId,
        requestedAt: cancellation.requestedAt,
        completedAt,
        sandboxCleanup: cleanup,
      }), {
        httpMetadata: { contentType: "application/json" },
        customMetadata: { kind: "ci-run-termination", head: cancellation.head },
      });
      await this.#state.storage.transaction(async (transaction) => {
        const key = `${CANCELLATION_PREFIX}${cancellation.head}`;
        const current = await transaction.get<CiCancellationRecord>(key);
        if (!current || !validCancellation(current, cancellation.head) ||
          current.state !== "terminating" || current.claimId !== cancellation.claimId) return;
        const complete = { ...current, state: "complete", completedAt } satisfies CiCancellationRecord;
        delete complete.nextAttemptAt;
        delete complete.lastError;
        await transaction.put(key, complete);
      });
    } catch (cause) {
      const retryAt = Date.now() + Math.min(
        1_000 * 2 ** Math.min(cancellation.attempts, 8),
        MAX_CANCELLATION_RETRY_MS,
      );
      await this.#state.storage.transaction(async (transaction) => {
        const key = `${CANCELLATION_PREFIX}${cancellation.head}`;
        const current = await transaction.get<CiCancellationRecord>(key);
        if (!current || !validCancellation(current, cancellation.head) ||
          current.state !== "terminating" || current.claimId !== cancellation.claimId) return;
        await transaction.put(key, {
          ...current,
          lastError: boundedError(cause),
          nextAttemptAt: new Date(retryAt).toISOString(),
        } satisfies CiCancellationRecord);
        await this.#armTransactionAlarm(transaction, retryAt);
      });
      console.error("Failed to cancel superseded CI run", cause);
    }
  }

  async #runHasTerminalEvidence(head: string, workflowId: string): Promise<boolean> {
    try {
      const status = await (await this.#env.CI_WORKFLOW.get(workflowId)).status();
      if (TERMINAL_WORKFLOW_STATES.has(status.status)) return true;
      if (status.status !== "unknown") return false;
    } catch {
      // Exact retained evidence remains authoritative after Workflow metadata expires.
    }
    return this.#hasExactRetainedTerminalEvidence(head, workflowId);
  }

  async #hasExactRetainedTerminalEvidence(
    head: string,
    workflowId: string,
  ): Promise<boolean> {
    const [result, termination] = await Promise.all([
      this.#env.BACKUP_BUCKET.get(`runs/${head}/result.json`),
      this.#env.BACKUP_BUCKET.get(terminationMarkerKey(head)),
    ]);
    const [resultValue, terminationValue] = await Promise.all([
      boundedR2Json(result, MAX_TERMINAL_RESULT_BYTES),
      boundedR2Json(termination, MAX_TERMINATION_MARKER_BYTES),
    ]);
    return isCiTerminalResult(resultValue, head, workflowId) ||
      exactCompletedTermination(terminationValue, head, workflowId);
  }

  async #publish(request: Request): Promise<Response> {
    const value = await request.json().catch(() => undefined) as {
      expectedHead?: unknown;
      publication?: unknown;
      leaseId?: unknown;
      reopen?: unknown;
    } | undefined;
    if (
      value == null ||
      !(value.expectedHead === null || isSha1(value.expectedHead)) ||
      !isCiSourcePublication(value.publication) ||
      typeof value.leaseId !== "string" || !CLOSE_ID_PATTERN.test(value.leaseId)
    ) return Response.json({ error: "invalid_publication" }, { status: 400 });

    const expectedHead = value.expectedHead;
    const publication = normalizeCiSourcePublication(value.publication);
    const leaseId = value.leaseId;
    const lane = laneLocator(ciSourceLane(publication));
    const reopen = value.reopen;
    if (
      (reopen !== undefined && !isPullRequestReopen(reopen)) ||
      (lane.type === "master" && reopen !== undefined)
    ) return Response.json({ error: "invalid_publication" }, { status: 400 });
    const publicationKey = publicationStorageKey(lane, publication.head);
    const pointerKey = pointerStorageKey(lane);
    const closedKey = lane.type === "pull_request"
      ? `${PULL_REQUEST_CLOSED_PREFIX}${lane.number}`
      : undefined;
    const reopenKey = lane.type === "pull_request" && reopen !== undefined
      ? `${PULL_REQUEST_REOPEN_PREFIX}${lane.number}:${reopen.closeId}`
      : undefined;
    const sourceKey = `${SOURCE_PREFIX}${publication.head}`;
    const runKey = `${RUN_PREFIX}${publication.head}`;
    const outboxKey = `${OUTBOX_PREFIX}${publication.head}`;
    const result = await this.#state.storage.transaction(
      async (transaction): Promise<PublishResult> => {
        const [
          retiring,
          existingPublication,
          currentHead,
          source,
          existingRun,
          existingOutbox,
          cancellation,
          publicationLease,
          releaseCommitReservation,
          closed,
          reopenRecord,
        ] = await Promise.all([
          transaction.get<CiRetiringRecord>(`${RETIRING_PREFIX}${publication.head}`),
          transaction.get<CiSourcePublication>(publicationKey),
          transaction.get<string>(pointerKey),
          transaction.get<CiSourceRecord>(sourceKey),
          transaction.get<CiRunRecord>(runKey),
          transaction.get<CiOutboxRecord>(outboxKey),
          transaction.get<CiCancellationRecord>(`${CANCELLATION_PREFIX}${publication.head}`),
          transaction.get<CiPublicationLeaseRecord>(
            `${PUBLICATION_LEASE_PREFIX}${publication.head}`,
          ),
          lane.type === "master"
            ? transaction.get<CiReleaseCommitReservation>(
              RELEASE_COMMIT_RESERVATION_KEY,
            )
            : Promise.resolve(undefined),
          closedKey
            ? transaction.get<CiPullRequestClosedRecord>(closedKey)
            : Promise.resolve(undefined),
          reopenKey
            ? transaction.get<CiPullRequestReopenRecord>(reopenKey)
            : Promise.resolve(undefined),
        ]);
        if (retiring) return { type: "source_retiring" };
        if (cancellation && !validCancellation(cancellation, publication.head)) {
          return { type: "repository_state_invalid" };
        }
        if (
          cancellation &&
          (cancellation.reason === "operator_terminated" || cancellation.state !== "pending")
        ) {
          return { type: "source_retiring" };
        }
        if (releaseCommitReservation) {
          if (!validReleaseCommitReservation(releaseCommitReservation)) {
            return { type: "repository_state_invalid" };
          }
          if (Date.parse(releaseCommitReservation.expiresAt) <= Date.now()) {
            await transaction.delete(RELEASE_COMMIT_RESERVATION_KEY);
          } else if (currentHead !== publication.head) {
            return {
              type: "release_commit_reserved",
              reservation: releaseCommitReservation,
            };
          }
        }
        const retainedFirstPublication = source
          ? await exactRetainedFirstPublication(transaction, source)
          : undefined;
        if (
          source && (!retainedFirstPublication ||
            (existingRun && retainedFirstPublication.publishedAt !== existingRun.publishedAt))
        ) return { type: "repository_state_invalid" };
        if (
          !source &&
          (!publicationLease ||
            !validPublicationLeaseRecord(publicationLease, publication.head, leaseId) ||
            !sameSource(publicationLease.publication, publication) ||
            Date.parse(publicationLease.expiresAt) <= Date.now())
        ) return { type: "publication_lease_invalid" };
        if (closed) {
          if (
            currentHead || !validClosedRecord(closed) ||
            reopen === undefined ||
            reopen.closeId !== closed.closeId ||
            reopen.mergeHead !== closed.mergeHead ||
            reopen.pullRequestHead !== closed.pullRequestHead
          ) {
            return currentHead || !validClosedRecord(closed)
              ? { type: "repository_state_invalid" }
              : { type: "pull_request_closed", record: closed };
          }
          if (reopenRecord) return { type: "repository_state_invalid" };
        } else if (reopen !== undefined) {
          if (
            !reopenRecord || !validReopenRecord(reopenRecord) ||
            !reopenRecordMatches(reopenRecord, reopen, publication) ||
            currentHead !== publication.head
          ) return { type: "reopen_conflict" };
        }
        if (existingPublication) {
          if (!samePublication(existingPublication, publication)) {
            return { type: "publication_conflict" };
          }
          if (
            !source || !existingRun ||
            !validSourceRecord(source, publication.head) ||
            !validRun(existingRun, publication.head) ||
            !sameSource(existingPublication, source.firstPublication) ||
            !validOutboxForRun(outboxKey, existingOutbox, existingRun)
          ) return { type: "repository_state_invalid" };
          if (currentHead !== publication.head) {
            if ((currentHead ?? null) !== expectedHead) {
              return { type: "cas_conflict", currentHead: currentHead ?? null };
            }
            await this.#moveLaneCancellation(
              transaction,
              lane,
              currentHead,
              publication.head,
              cancellation,
            );
            await transaction.put(pointerKey, publication.head);
            if (closedKey && reopenKey && reopen !== undefined) {
              await transaction.put(
                reopenKey,
                pullRequestReopenRecord(publication, reopen),
              );
              await transaction.delete(closedKey);
            }
            return {
              type: "accepted",
              publication: existingPublication,
              run: existingRun,
              reusedWorkflow: true,
            };
          }
          return {
            type: "replay",
            publication: existingPublication,
            run: existingRun,
            reusedWorkflow: true,
          };
        }
        if ((currentHead ?? null) !== expectedHead) {
          return { type: "cas_conflict", currentHead: currentHead ?? null };
        }
        if ((source == null) !== (existingRun == null) || (!source && existingOutbox)) {
          return { type: "repository_state_invalid" };
        }
        if (source && existingRun) {
          if (
            !validSourceRecord(source, publication.head) ||
            !validRun(existingRun, publication.head) ||
            !validOutboxForRun(outboxKey, existingOutbox, existingRun)
          ) return { type: "repository_state_invalid" };
          if (!sameSource(source.firstPublication, publication)) {
            return { type: "source_conflict" };
          }
          await this.#moveLaneCancellation(
            transaction,
            lane,
            currentHead,
            publication.head,
            cancellation,
          );
          await transaction.put({
            [pointerKey]: publication.head,
            [publicationKey]: publication,
          });
          if (closedKey && reopenKey && reopen !== undefined) {
            await transaction.put(
              reopenKey,
              pullRequestReopenRecord(publication, reopen),
            );
            await transaction.delete(closedKey);
          }
          return {
            type: "accepted",
            publication,
            run: existingRun,
            reusedWorkflow: true,
          };
        }

        const workflowId = `ci-${publication.head}`;
        const run: CiRunRecord = {
          version: 1,
          head: publication.head,
          beforeHead: expectedHead,
          workflowId,
          state: "pending",
          attempts: 0,
          publishedAt: publication.publishedAt,
        };
        const params = ciWorkflowParams(publication, expectedHead);
        const sourceRecord: CiSourceRecord = {
          version: 1,
          head: publication.head,
          firstPublication: publication,
        };
        await this.#moveLaneCancellation(
          transaction,
          lane,
          currentHead,
          publication.head,
          cancellation,
        );
        await transaction.put({
          [pointerKey]: publication.head,
          [publicationKey]: publication,
          [sourceKey]: sourceRecord,
          [runKey]: run,
          [outboxKey]: { version: 1, run, params } satisfies CiOutboxRecord,
        });
        await transaction.delete(`${PUBLICATION_LEASE_PREFIX}${publication.head}`);
        if (closedKey && reopenKey && reopen !== undefined) {
          await transaction.put(
            reopenKey,
            pullRequestReopenRecord(publication, reopen),
          );
          await transaction.delete(closedKey);
        }
        await transaction.setAlarm(Date.now());
        return {
          type: "accepted",
          publication,
          run,
          reusedWorkflow: false,
        };
      },
    );
    switch (result.type) {
      case "accepted":
        await this.#trimHistory().catch((cause) => {
          console.error("Failed to trim retained CI repository history", cause);
        });
        await this.#scheduleNextAlarm();
        return Response.json({
          publication: result.publication,
          run: result.run,
          reusedWorkflow: result.reusedWorkflow,
        }, { status: 202 });
      case "replay":
        await this.#scheduleNextAlarm();
        return Response.json({
          publication: result.publication,
          run: result.run,
          reusedWorkflow: result.reusedWorkflow,
        });
      case "cas_conflict":
        return Response.json({
          error: "publication_conflict",
          currentHead: result.currentHead,
        }, { status: 409 });
      case "publication_conflict":
        return Response.json({ error: "publication_conflict" }, { status: 409 });
      case "pull_request_closed":
        return Response.json({
          error: "pull_request_closed",
          closeId: result.record.closeId,
          mergeHead: result.record.mergeHead,
          pullRequestHead: result.record.pullRequestHead,
          closedAt: result.record.closedAt,
        }, { status: 409 });
      case "reopen_conflict":
        return Response.json({ error: "reopen_conflict" }, { status: 409 });
      case "source_conflict":
        return Response.json({ error: "source_conflict" }, { status: 409 });
      case "source_retiring":
        return Response.json({ error: "source_retiring" }, { status: 409 });
      case "publication_lease_invalid":
        return Response.json({ error: "publication_lease_invalid" }, { status: 409 });
      case "release_commit_reserved":
        return Response.json({
          error: "release_commit_reserved",
          commit: result.reservation.commit,
          expiresAt: result.reservation.expiresAt,
        }, {
          status: 409,
          headers: { "cache-control": "no-store" },
        });
      case "repository_state_invalid":
        return Response.json({ error: "repository_state_invalid" }, { status: 503 });
    }
  }

  async #trimHistory(): Promise<void> {
    const [masterHead, pullRequestHeads, runs, outbox, leases, publicationLeases] = await Promise.all([
      this.#state.storage.get<string>(MASTER_HEAD_KEY),
      this.#state.storage.list<string>({ prefix: PULL_REQUEST_HEAD_PREFIX }),
      this.#state.storage.list<CiRunRecord>({ prefix: RUN_PREFIX }),
      this.#state.storage.list<CiOutboxRecord>({ prefix: OUTBOX_PREFIX }),
      this.#state.storage.list<CiDistributionLease>({ prefix: DISTRIBUTION_LEASE_PREFIX }),
      this.#state.storage.list<CiPublicationLeaseRecord>({ prefix: PUBLICATION_LEASE_PREFIX }),
    ]);
    if (runs.size <= MAX_RETAINED_RUNS) return;
    if (
      (masterHead != null && !isSha1(masterHead)) ||
      [...pullRequestHeads.values()].some((head) => !isSha1(head)) ||
      [...runs.entries()].some(([key, run]) =>
        !validRun(run, run.head) || key !== `${RUN_PREFIX}${run.head}`
      ) ||
      [...outbox.entries()].some(([key, record]) => !validOutboxRecord(key, record))
    ) throw new Error("CI retention found invalid repository references");
    const current = new Set([
      ...(masterHead ? [masterHead] : []),
      ...[...pullRequestHeads.values()].filter(isSha1),
    ]);
    const pending = new Set([...outbox.values()].map(({ run }) => run.head));
    const now = Date.now();
    const leased = new Set<string>();
    for (const [key, lease] of leases) {
      if (!validDistributionLeaseStorageEntry(key, lease)) {
        throw new Error("CI distribution lease contains invalid repository state");
      }
      if (Date.parse(lease.expiresAt) > now) leased.add(lease.head);
    }
    for (const [key, lease] of publicationLeases) {
      if (!validPublicationLeaseStorageEntry(key, lease)) {
        throw new Error("CI publication lease contains invalid repository state");
      }
      if (Date.parse(lease.expiresAt) > now) leased.add(lease.head);
    }
    const overflow = runs.size - MAX_RETAINED_RUNS;
    const candidates = [...runs.values()]
      .filter(({ head, state }) =>
        !current.has(head) && state === "dispatched" && !pending.has(head) &&
        !leased.has(head)
      )
      .sort((left, right) => left.publishedAt.localeCompare(right.publishedAt));
    const statuses: Array<{ run: CiRunRecord; terminal: boolean }> = [];
    for (let index = 0; index < candidates.length; index += MAX_RETENTION_STATUS_CONCURRENCY) {
      statuses.push(...await Promise.all(
        candidates.slice(index, index + MAX_RETENTION_STATUS_CONCURRENCY).map(async (run) => {
          return { run, terminal: await this.#runHasTerminalEvidence(run.head, run.workflowId) };
        }),
      ));
    }
    const removable = statuses
      .filter(({ terminal }) => terminal)
      .slice(0, overflow)
      .map(({ run }) => run);
    for (const run of removable) await this.#retireRun(run);
  }

  async #collectSourceOrphans(): Promise<{
    scanned: number;
    deleted: number;
    continuation: boolean;
  }> {
    const cursor = await this.#state.storage.get<string>(SOURCE_GC_CURSOR_KEY);
    const page = await this.#env.CI_SOURCE.list({
      limit: MAX_SOURCE_GC_OBJECTS,
      ...(cursor ? { cursor } : {}),
    });
    const now = Date.now();
    const cutoff = now - SOURCE_GC_GRACE_MS;
    const candidates = page.objects
      .filter((object) =>
        ownedCiSourceObject(object.key) && object.uploaded.getTime() <= cutoff
      )
      .map(({ key }) => key);
    const claimId = crypto.randomUUID();
    const claimedAt = new Date(now).toISOString();
    const reconcileAfter = new Date(now + SOURCE_GC_RECONCILE_MS).toISOString();
    const retiring = await this.#state.storage.transaction(async (transaction) => {
      const [sources, publications, leases, markers] = await Promise.all([
        transaction.list<CiSourceRecord>({ prefix: SOURCE_PREFIX }),
        transaction.list<CiSourcePublication>({ prefix: PUBLICATION_PREFIX }),
        transaction.list<CiPublicationLeaseRecord>({ prefix: PUBLICATION_LEASE_PREFIX }),
        transaction.list<CiSourceObjectRetiringRecord>({ prefix: SOURCE_OBJECT_RETIRING_PREFIX }),
      ]);
      const live = new Set<string>();
      for (const [key, source] of sources) {
        const head = key.slice(SOURCE_PREFIX.length);
        if (!isSha1(head) || !validSourceRecord(source, head)) {
          throw new Error("CI source garbage collection found invalid repository state");
        }
        for (const objectKey of sourceObjectKeys(source.firstPublication)) live.add(objectKey);
      }
      for (const [key, publication] of publications) {
        if (!validPublicationStorageEntry(key, publication)) {
          throw new Error("CI source garbage collection found an invalid publication record");
        }
        for (const objectKey of sourceObjectKeys(publication)) live.add(objectKey);
      }
      const expired: string[] = [];
      for (const [key, lease] of leases) {
        if (!validPublicationLeaseStorageEntry(key, lease)) {
          throw new Error("CI source garbage collection found an invalid publication lease");
        }
        if (Date.parse(lease.expiresAt) <= now) expired.push(key);
        else for (const objectKey of sourceObjectKeys(lease.publication)) live.add(objectKey);
      }
      const activeMarkers = new Map<string, CiSourceObjectRetiringRecord>();
      const claimed: string[] = [];
      for (const [key, marker] of markers) {
        if (!validSourceObjectRetiringStorageEntry(key, marker)) {
          throw new Error("CI source garbage collection found an invalid retirement fence");
        }
        if (live.has(marker.key)) {
          throw new Error("CI source garbage collection found a retirement fence on live source");
        }
        if (this.#activeSourceGcClaims.has(marker.claimId)) {
          const renewed = {
            ...marker,
            claimedAt,
            reconcileAfter,
          } satisfies CiSourceObjectRetiringRecord;
          await transaction.put(key, renewed);
          activeMarkers.set(marker.key, renewed);
          continue;
        }
        if (
          Date.parse(marker.reconcileAfter) <= now &&
          claimed.length < MAX_SOURCE_GC_OBJECTS
        ) {
          const recovered = {
            ...marker,
            claimId,
            claimedAt,
            reconcileAfter,
          } satisfies CiSourceObjectRetiringRecord;
          await transaction.put(key, recovered);
          activeMarkers.set(marker.key, recovered);
          claimed.push(marker.key);
          continue;
        }
        activeMarkers.set(marker.key, marker);
      }
      if (expired.length > 0) await transaction.delete(expired);
      for (const objectKey of candidates) {
        if (claimed.length >= MAX_SOURCE_GC_OBJECTS) break;
        if (live.has(objectKey) || activeMarkers.has(objectKey)) continue;
        const marker = {
          version: 1,
          key: objectKey,
          claimId,
          claimedAt,
          reconcileAfter,
        } satisfies CiSourceObjectRetiringRecord;
        await transaction.put(sourceObjectRetiringKey(objectKey), marker);
        activeMarkers.set(objectKey, marker);
        claimed.push(objectKey);
      }
      return claimed;
    });
    if (retiring.length > 0) this.#activeSourceGcClaims.add(claimId);
    try {
      if (retiring.length > 0) await this.#env.CI_SOURCE.delete(retiring);
      await this.#state.storage.transaction(async (transaction) => {
        for (const objectKey of retiring) {
          const key = sourceObjectRetiringKey(objectKey);
          const marker = await transaction.get<CiSourceObjectRetiringRecord>(key);
          if (marker?.claimId === claimId) await transaction.delete(key);
        }
        if (page.truncated) {
          if (!page.cursor) throw new Error("CI source listing truncated without a cursor");
          await transaction.put(SOURCE_GC_CURSOR_KEY, page.cursor);
        } else {
          await transaction.delete(SOURCE_GC_CURSOR_KEY);
        }
      });
    } catch (cause) {
      await this.#renewSourceGcClaims(retiring, claimId);
      throw cause;
    } finally {
      this.#activeSourceGcClaims.delete(claimId);
    }
    return {
      scanned: page.objects.length,
      deleted: retiring.length,
      continuation: page.truncated,
    };
  }

  async #renewSourceGcClaims(keys: string[], claimId: string): Promise<void> {
    const claimedAt = new Date().toISOString();
    const reconcileAfter = new Date(
      Date.now() + SOURCE_GC_RECONCILE_MS,
    ).toISOString();
    await this.#state.storage.transaction(async (transaction) => {
      for (const objectKey of keys) {
        const key = sourceObjectRetiringKey(objectKey);
        const marker = await transaction.get<CiSourceObjectRetiringRecord>(key);
        if (marker?.claimId === claimId) {
          await transaction.put(key, {
            ...marker,
            claimedAt,
            reconcileAfter,
          } satisfies CiSourceObjectRetiringRecord);
        }
      }
    });
  }

  async #retireRun(run: CiRunRecord): Promise<void> {
    const retiringKey = `${RETIRING_PREFIX}${run.head}`;
    const marked = await this.#state.storage.transaction(async (transaction) => {
      const [
        masterHead,
        pullRequestHeads,
        pending,
        storedRun,
        retiring,
        leases,
        publicationLeases,
        cancellation,
      ] = await Promise.all([
        transaction.get<string>(MASTER_HEAD_KEY),
        transaction.list<string>({ prefix: PULL_REQUEST_HEAD_PREFIX }),
        transaction.get<CiOutboxRecord>(`${OUTBOX_PREFIX}${run.head}`),
        transaction.get<CiRunRecord>(`${RUN_PREFIX}${run.head}`),
        transaction.get<CiRetiringRecord>(retiringKey),
        transaction.list<CiDistributionLease>({ prefix: DISTRIBUTION_LEASE_PREFIX }),
        transaction.list<CiPublicationLeaseRecord>({ prefix: PUBLICATION_LEASE_PREFIX }),
        transaction.get<CiCancellationRecord>(`${CANCELLATION_PREFIX}${run.head}`),
      ]);
      if (
        (masterHead != null && !isSha1(masterHead)) ||
        [...pullRequestHeads.values()].some((head) => !isSha1(head)) ||
        (pending && !validOutboxRecord(`${OUTBOX_PREFIX}${run.head}`, pending))
      ) throw new Error("CI retirement found invalid repository references");
      const current = masterHead === run.head ||
        [...pullRequestHeads.values()].some((head) => head === run.head);
      const leased = [...leases.entries()].some(([key, lease]) => {
        if (!validDistributionLeaseStorageEntry(key, lease)) {
          throw new Error("CI distribution lease contains invalid repository state");
        }
        return lease.head === run.head && Date.parse(lease.expiresAt) > Date.now();
      });
      const publicationPinned = [...publicationLeases.entries()].some(([key, lease]) => {
        if (!validPublicationLeaseStorageEntry(key, lease)) {
          throw new Error("CI publication lease contains invalid repository state");
        }
        return lease.head === run.head && Date.parse(lease.expiresAt) > Date.now();
      });
      if (
        current || pending || leased || publicationPinned ||
        (cancellation && (!validCancellation(cancellation, run.head) ||
          cancellation.state !== "complete")) ||
        !storedRun || storedRun.state !== "dispatched" ||
        storedRun.workflowId !== run.workflowId
      ) return false;
      if (!retiring) {
        await transaction.put(retiringKey, {
          version: 1,
          head: run.head,
          markedAt: new Date().toISOString(),
        } satisfies CiRetiringRecord);
      }
      return true;
    });
    if (!marked) return;
    try {
      await this.#env.CI_SOURCE.delete([
        sourceArchiveKey(run.head),
        sourceTreeKey(run.head),
      ]);
    } catch (cause) {
      await this.#state.storage.delete(retiringKey);
      throw cause;
    }
    await this.#state.storage.transaction(async (transaction) => {
      const publications = await transaction.list({
        prefix: PUBLICATION_PREFIX,
      });
      const publicationKeys = [...publications.entries()]
        .filter(([key]) => key.endsWith(`:${run.head}`))
        .map(([key]) => key);
      const leases = await transaction.list<CiDistributionLease>({
        prefix: DISTRIBUTION_LEASE_PREFIX,
      });
      const leaseKeys = [...leases.entries()]
        .filter(([, lease]) => lease.head === run.head)
        .map(([key]) => key);
      const publicationLeaseKey = `${PUBLICATION_LEASE_PREFIX}${run.head}`;
      await transaction.delete([
        `${RUN_PREFIX}${run.head}`,
        `${SOURCE_PREFIX}${run.head}`,
        `${OUTBOX_PREFIX}${run.head}`,
        retiringKey,
        ...leaseKeys,
        publicationLeaseKey,
        ...publicationKeys,
      ]);
    });
  }
}

function ownedCiSourceObject(key: string): boolean {
  return /^sources\/[a-f0-9]{40}\/(?:source\.tar\.gz|tree\.json)$/.test(key) ||
    /^cargo-vendor\/[a-f0-9]{40}\/[a-f0-9]{64}\/bundle\.tar\.gz$/.test(key) ||
    /^cargo-vendor-staging\/[a-f0-9]{40}\/[a-f0-9]{64}\/[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}\/bundle\.tar\.gz$/.test(
      key,
    ) ||
    /^rustsec-advisory-db\/[a-f0-9]{40}\/bundle\.tar\.gz$/.test(key);
}

export function ciWorkflowParams(
  publication: CiSourcePublication,
  beforeHead: string | null,
  distribution?: CiDistributionRequest,
): NanocodexCiParams {
  const lane = ciSourceLane(publication);
  if (distribution && !lane.deployable) {
    throw new Error("Distribution workflows require a deployable master source");
  }
  const sourceData = {
    archiveKey: publication.archive.key,
    archiveSha256: publication.archive.sha256,
    archiveSize: publication.archive.size,
    treeKey: publication.tree.key,
    treeSha256: publication.tree.sha256,
    cargoLockBlob: publication.cargoLockBlob,
    cargoVendorKey: cargoVendorBundleKey(
      publication.cargoLockBlob,
      publication.cargoVendor.sha256,
    ),
    cargoVendorSha256: publication.cargoVendor.sha256,
    cargoVendorSize: publication.cargoVendor.size,
    rustSecRevision: publication.rustSecRevision,
    rustSecKey: publication.rustSec.key,
    rustSecSha256: publication.rustSec.sha256,
    rustSecSize: publication.rustSec.size,
    publishedAt: publication.publishedAt,
  };
  const providerData: NanocodexCiParams["providerData"] = lane.type === "master"
    ? { ...sourceData, lane, ...(distribution ? { distribution } : {}) }
    : { ...sourceData, lane };
  return {
    provider: "nanocodex-source",
    providerData,
    event: { type: "push" },
    owner: "gakonst",
    repo: "nanocodex",
    sha: publication.head,
    remote: "cloudflare",
    trigger: "push",
    ref: lane.ref,
    branch: lane.branch,
    ...(beforeHead == null ? {} : { beforeSha: beforeHead }),
  };
}

function pointerStorageKey(lane: CiLaneLocator): string {
  return lane.type === "master"
    ? MASTER_HEAD_KEY
    : `${PULL_REQUEST_HEAD_PREFIX}${lane.number}`;
}

function publicationStorageKey(lane: CiLaneLocator, head: string): string {
  return lane.type === "master"
    ? `${PUBLICATION_PREFIX}master:${head}`
    : `${PUBLICATION_PREFIX}pull-request:${lane.number}:${head}`;
}

function laneLocator(lane: CiSourceLane): CiLaneLocator {
  return lane.type === "master"
    ? { type: "master" }
    : { type: "pull_request", number: lane.number };
}

function laneMatches(publication: CiSourcePublication, expected: CiLaneLocator): boolean {
  const actual = laneLocator(ciSourceLane(publication));
  return actual.type === expected.type &&
    (actual.type === "master" ||
      (expected.type === "pull_request" && actual.number === expected.number));
}

async function exactCurrentMasterState(
  transaction: DurableObjectTransaction,
  commit: string,
): Promise<
  | ({ type: "retained" } & CiLaneState)
  | { type: "stale" }
  | { type: "invalid" }
> {
  const [currentHead, publication, run, source] = await Promise.all([
    transaction.get<string>(MASTER_HEAD_KEY),
    transaction.get<CiSourcePublication>(
      publicationStorageKey({ type: "master" }, commit),
    ),
    transaction.get<CiRunRecord>(`${RUN_PREFIX}${commit}`),
    transaction.get<CiSourceRecord>(`${SOURCE_PREFIX}${commit}`),
  ]);
  if (currentHead !== commit) {
    return currentHead == null || isSha1(currentHead)
      ? { type: "stale" }
      : { type: "invalid" };
  }
  if (
    !publication || !run || !source || !isCiSourcePublication(publication) ||
    !laneMatches(publication, { type: "master" }) ||
    !validRun(run, commit) || run.state !== "dispatched" ||
    !validSourceRecord(source, commit) || !sameSource(publication, source.firstPublication)
  ) return { type: "invalid" };
  return { type: "retained", publication, run };
}

function validReleaseCommitReservationRequest(
  value: unknown,
): value is CiReleaseCommitReservationRequest {
  if (
    value == null || typeof value !== "object" || Array.isArray(value) ||
    !hasExactKeys(value as Record<string, unknown>, [
      "commit",
      "owner",
      "publicationLeaseGeneration",
      "publicationLeaseId",
      "releaseId",
      "releaseKind",
      "version",
    ])
  ) return false;
  const request = value as Record<string, unknown>;
  const releaseKind = request.releaseKind;
  const releaseId = request.releaseId;
  const publicationLeaseId = request.publicationLeaseId;
  const publicationLeaseGeneration = request.publicationLeaseGeneration;
  return request.version === 1 &&
    typeof request.owner === "string" && RELEASE_RESERVATION_OWNER.test(request.owner) &&
    (releaseKind === "stable" || releaseKind === "commit") &&
    typeof releaseId === "string" &&
    (releaseKind === "stable"
      ? /^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/.test(releaseId)
      : isSha1(releaseId)) &&
    isSha1(request.commit) && (releaseKind !== "commit" || releaseId === request.commit) &&
    typeof publicationLeaseId === "string" && FENCED_ID_PATTERN.test(publicationLeaseId) &&
    Number.isSafeInteger(publicationLeaseGeneration) &&
    (publicationLeaseGeneration as number) > 0 &&
    publicationLeaseId.split(".", 1)[0] === String(publicationLeaseGeneration);
}

function validReleaseCommitReservationMutation(
  value: unknown,
): value is { owner: string; generation: number } {
  if (
    value == null || typeof value !== "object" || Array.isArray(value) ||
    !hasExactKeys(value as Record<string, unknown>, ["generation", "owner"])
  ) return false;
  const mutation = value as Record<string, unknown>;
  return typeof mutation.owner === "string" &&
    RELEASE_RESERVATION_OWNER.test(mutation.owner) &&
    Number.isSafeInteger(mutation.generation) && (mutation.generation as number) > 0;
}

function validReleaseCommitReservation(
  reservation: CiReleaseCommitReservation,
): boolean {
  const acquiredAt = Date.parse(reservation.acquiredAt);
  const renewedAt = Date.parse(reservation.renewedAt);
  const expiresAt = Date.parse(reservation.expiresAt);
  return reservation.version === 1 && reservation.kind === "release-commit" &&
    FENCED_ID_PATTERN.test(reservation.reservationId) &&
    reservation.reservationId.split(".", 1)[0] === String(reservation.generation) &&
    RELEASE_RESERVATION_OWNER.test(reservation.owner) &&
    validReleaseCommitReservationRequest({
      version: 1,
      owner: reservation.owner,
      releaseKind: reservation.releaseKind,
      releaseId: reservation.releaseId,
      commit: reservation.commit,
      publicationLeaseId: reservation.publicationLeaseId,
      publicationLeaseGeneration: reservation.publicationLeaseGeneration,
    }) &&
    Number.isSafeInteger(reservation.generation) && reservation.generation > 0 &&
    Number.isFinite(acquiredAt) && acquiredAt <= Date.now() &&
    Number.isFinite(renewedAt) && renewedAt >= acquiredAt && renewedAt <= Date.now() &&
    Number.isFinite(expiresAt) && expiresAt > renewedAt &&
    expiresAt - renewedAt <= RELEASE_COMMIT_RESERVATION_MS;
}

function sameReleaseCommitReservationRequest(
  reservation: CiReleaseCommitReservation,
  request: CiReleaseCommitReservationRequest,
): boolean {
  return reservation.owner === request.owner &&
    reservation.releaseKind === request.releaseKind &&
    reservation.releaseId === request.releaseId && reservation.commit === request.commit &&
    reservation.publicationLeaseId === request.publicationLeaseId &&
    reservation.publicationLeaseGeneration === request.publicationLeaseGeneration;
}

function validSourceRecord(record: CiSourceRecord, head: string): boolean {
  return record.version === 1 && record.head === head &&
    isCiSourcePublication(record.firstPublication) &&
    record.firstPublication.head === head;
}

async function exactRetainedFirstPublication(
  transaction: DurableObjectTransaction,
  source: CiSourceRecord,
): Promise<CiSourcePublication | undefined> {
  if (!validSourceRecord(source, source.head)) return undefined;
  const lane = laneLocator(ciSourceLane(source.firstPublication));
  const publication = await transaction.get<CiSourcePublication>(
    publicationStorageKey(lane, source.head),
  );
  return publication && isCiSourcePublication(publication) &&
      laneMatches(publication, lane) && samePublication(publication, source.firstPublication)
    ? publication
    : undefined;
}

function validPublicationStorageEntry(
  key: string,
  publication: CiSourcePublication,
): boolean {
  if (!isCiSourcePublication(publication)) return false;
  return key === publicationStorageKey(
    laneLocator(ciSourceLane(publication)),
    publication.head,
  );
}

function validRun(run: CiRunRecord, head: string): boolean {
  return run.version === 1 && run.head === head && run.workflowId === `ci-${head}` &&
    (run.beforeHead === null || isSha1(run.beforeHead)) &&
    (run.state === "pending" || run.state === "dispatched") &&
    Number.isSafeInteger(run.attempts) && run.attempts >= 0 &&
    Number.isFinite(Date.parse(run.publishedAt));
}

function sameRun(left: CiRunRecord, right: CiRunRecord): boolean {
  return left.version === right.version && left.head === right.head &&
    left.beforeHead === right.beforeHead && left.workflowId === right.workflowId &&
    left.state === right.state && left.attempts === right.attempts &&
    left.publishedAt === right.publishedAt && left.dispatchedAt === right.dispatchedAt &&
    left.lastDispatchError === right.lastDispatchError &&
    left.nextDispatchAt === right.nextDispatchAt;
}

function samePublication(left: CiSourcePublication, right: CiSourcePublication): boolean {
  return JSON.stringify(normalizeCiSourcePublication(left)) ===
    JSON.stringify(normalizeCiSourcePublication(right));
}

function sameSource(left: CiSourcePublication, right: CiSourcePublication): boolean {
  return JSON.stringify([
    left.version,
    left.head,
    left.archive.key,
    left.archive.size,
    left.archive.sha256,
    left.tree.key,
    left.tree.size,
    left.tree.sha256,
    left.cargoLockBlob,
    left.cargoVendor.key,
    left.cargoVendor.size,
    left.cargoVendor.sha256,
    left.rustSecRevision,
    left.rustSec.key,
    left.rustSec.size,
    left.rustSec.sha256,
  ]) === JSON.stringify([
    right.version,
    right.head,
    right.archive.key,
    right.archive.size,
    right.archive.sha256,
    right.tree.key,
    right.tree.size,
    right.tree.sha256,
    right.cargoLockBlob,
    right.cargoVendor.key,
    right.cargoVendor.size,
    right.cargoVendor.sha256,
    right.rustSecRevision,
    right.rustSec.key,
    right.rustSec.size,
    right.rustSec.sha256,
  ]);
}

function pullRequestNumber(value: string): number | undefined {
  const number = Number(value);
  return Number.isSafeInteger(number) && number > 0 && String(number) === value
    ? number
    : undefined;
}

function isPullRequestReopen(value: unknown): value is CiPullRequestReopen {
  if (value == null || typeof value !== "object") return false;
  const reopen = value as Partial<CiPullRequestReopen>;
  return typeof reopen.closeId === "string" && CLOSE_ID_PATTERN.test(reopen.closeId) &&
    isSha1(reopen.mergeHead) && isSha1(reopen.pullRequestHead) &&
    hasExactKeys(value, ["closeId", "mergeHead", "pullRequestHead"]);
}

function validClosedRecord(value: CiPullRequestClosedRecord): boolean {
  return value.version === 1 && CLOSE_ID_PATTERN.test(value.closeId) &&
    isSha1(value.mergeHead) && isSha1(value.pullRequestHead) &&
    Number.isFinite(Date.parse(value.closedAt));
}

function validReopenRecord(value: CiPullRequestReopenRecord): boolean {
  return value.version === 1 && CLOSE_ID_PATTERN.test(value.closeId) &&
    isSha1(value.closedMergeHead) && isSha1(value.closedPullRequestHead) &&
    isSha1(value.publicationHead) && isSha1(value.publicationPullRequestHead) &&
    Number.isFinite(Date.parse(value.reopenedAt));
}

function reopenRecordMatches(
  record: CiPullRequestReopenRecord,
  reopen: CiPullRequestReopen,
  publication: CiSourcePublication,
): boolean {
  const lane = ciSourceLane(publication);
  return lane.type === "pull_request" &&
    record.closeId === reopen.closeId &&
    record.closedMergeHead === reopen.mergeHead &&
    record.closedPullRequestHead === reopen.pullRequestHead &&
    record.publicationHead === publication.head &&
    record.publicationPullRequestHead === lane.pullRequestHead;
}

function pullRequestReopenRecord(
  publication: CiSourcePublication,
  reopen: CiPullRequestReopen,
): CiPullRequestReopenRecord {
  const lane = ciSourceLane(publication);
  if (lane.type !== "pull_request") {
    throw new Error("Cannot reopen a master source publication");
  }
  return {
    version: 1,
    closeId: reopen.closeId,
    closedMergeHead: reopen.mergeHead,
    closedPullRequestHead: reopen.pullRequestHead,
    publicationHead: publication.head,
    publicationPullRequestHead: lane.pullRequestHead,
    reopenedAt: new Date().toISOString(),
  };
}

function hasExactKeys(value: object, expected: string[]): boolean {
  const keys = Object.keys(value).sort();
  return keys.length === expected.length &&
    keys.every((key, index) => key === expected[index]);
}

function isCargoVendorMultipartInput(
  value: unknown,
): value is CiCargoVendorMultipartInput {
  if (value == null || typeof value !== "object" || Array.isArray(value)) return false;
  const input = value as Partial<CiCargoVendorMultipartInput>;
  return hasExactKeys(value, [
    "bundleSha256",
    "cargoLockBlob",
    "partCount",
    "partSize",
    "requestId",
    "size",
    "version",
  ]) && input.version === 1 && typeof input.requestId === "string" &&
    CLOSE_ID_PATTERN.test(input.requestId) && isSha1(input.cargoLockBlob) &&
    isSha256(input.bundleSha256) && typeof input.size === "number" &&
    Number.isSafeInteger(input.size) && input.size > 0 &&
    input.size <= MAX_CARGO_VENDOR_BYTES && input.partSize === CARGO_VENDOR_PART_BYTES &&
    typeof input.partCount === "number" && Number.isSafeInteger(input.partCount) &&
    input.partCount === Math.ceil(input.size / CARGO_VENDOR_PART_BYTES) &&
    input.partCount > 0 && input.partCount <= MAX_CARGO_VENDOR_PARTS;
}

function validCargoVendorMultipartRecord(
  record: CiCargoVendorMultipartRecord,
  requestId: string,
): boolean {
  if (!isCargoVendorMultipartInput({
    version: record.version,
    requestId: record.requestId,
    cargoLockBlob: record.cargoLockBlob,
    bundleSha256: record.bundleSha256,
    size: record.size,
    partSize: record.partSize,
    partCount: record.partCount,
  }) || record.requestId !== requestId || record.stagingId !== requestId) return false;
  if (record.state === "creating") {
    const started = Date.parse(record.startedAt);
    const recover = Date.parse(record.recoverAfter);
    return Number.isFinite(started) && started <= Date.now() && Number.isFinite(recover) &&
      recover > started && recover - started <= R2_INCOMPLETE_MULTIPART_ABORT_MS;
  }
  if (record.state === "ready") {
    return Number.isFinite(Date.parse(record.startedAt)) &&
      Number.isFinite(Date.parse(record.readyAt)) &&
      typeof record.uploadId === "string" && record.uploadId.length > 0 &&
      record.uploadId.length <= 1_024;
  }
  return record.state === "complete" && Number.isFinite(Date.parse(record.completedAt));
}

function sameCargoVendorMultipartRequest(
  record: CiCargoVendorMultipartRecord,
  input: CiCargoVendorMultipartInput,
): boolean {
  return record.version === input.version && record.requestId === input.requestId &&
    record.cargoLockBlob === input.cargoLockBlob &&
    record.bundleSha256 === input.bundleSha256 && record.size === input.size &&
    record.partSize === input.partSize && record.partCount === input.partCount;
}

function cargoVendorMultipartStagingKey(input: CiCargoVendorMultipartInput): string {
  return `cargo-vendor-staging/${input.cargoLockBlob}/${input.bundleSha256}/${input.requestId}/bundle.tar.gz`;
}

function cargoVendorMultipartResponse(
  record: Extract<CiCargoVendorMultipartRecord, { state: "ready" }>,
): Record<string, unknown> {
  return {
    requestId: record.requestId,
    key: cargoVendorBundleKey(record.cargoLockBlob, record.bundleSha256),
    cargoLockBlob: record.cargoLockBlob,
    size: record.size,
    sha256: record.bundleSha256,
    uploadId: record.uploadId,
    stagingId: record.stagingId,
    partSize: record.partSize,
    partCount: record.partCount,
  };
}

function matchesCargoVendorObject(
  object: R2Object,
  input: CiCargoVendorMultipartInput,
): boolean {
  return object.key === cargoVendorBundleKey(input.cargoLockBlob, input.bundleSha256) &&
    object.size === input.size && object.customMetadata?.kind === "cargo-vendor" &&
    object.customMetadata?.cargoLockBlob === input.cargoLockBlob &&
    object.customMetadata?.sha256 === input.bundleSha256 &&
    object.customMetadata?.size === String(input.size) && object.checksums.sha256 != null &&
    checksumHex(object.checksums.sha256) === input.bundleSha256;
}

function checksumHex(buffer: ArrayBuffer): string {
  return [...new Uint8Array(buffer)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function validDistributionWorkflowId(workflowId: string, expectedHead?: string): boolean {
  const nightly = workflowId.match(/^nightly-([a-f0-9]{40})$/);
  const stable = workflowId.match(
    /^release-v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)-([a-f0-9]{40})$/,
  );
  const head = nightly?.[1] ?? stable?.[1];
  return head != null && (expectedHead == null || head === expectedHead);
}

function distributionLeaseStorageKey(workflowId: string): string {
  return `${DISTRIBUTION_LEASE_PREFIX}${workflowId}`;
}

function validDistributionLease(
  lease: CiDistributionLease,
  expectedWorkflowId?: string,
): boolean {
  const acquired = Date.parse(lease.acquiredAt);
  const expires = Date.parse(lease.expiresAt);
  return lease.version === 1 && lease.kind === "distribution" &&
    CLOSE_ID_PATTERN.test(lease.leaseId) && isSha1(lease.head) &&
    validDistributionWorkflowId(lease.workflowId, lease.head) &&
    (expectedWorkflowId == null || lease.workflowId === expectedWorkflowId) &&
    Number.isFinite(acquired) && acquired <= Date.now() &&
    Number.isFinite(expires) && expires > acquired &&
    expires - acquired <= DISTRIBUTION_LEASE_MS;
}

function validDistributionLeaseStorageEntry(
  key: string,
  lease: CiDistributionLease,
): boolean {
  return validDistributionLease(lease) && key === distributionLeaseStorageKey(lease.workflowId);
}

function sourceObjectKeys(publication: CiSourcePublication): string[] {
  return [
    publication.archive.key,
    publication.tree.key,
    cargoVendorBundleKey(
      publication.cargoLockBlob,
      publication.cargoVendor.sha256,
    ),
    publication.rustSec.key,
  ];
}

function validPublicationLeaseRecord(
  lease: CiPublicationLeaseRecord,
  expectedHead?: string,
  expectedLeaseId?: string,
): boolean {
  const acquired = Date.parse(lease.acquiredAt);
  const expires = Date.parse(lease.expiresAt);
  return lease.version === 1 && lease.kind === "publication" &&
    CLOSE_ID_PATTERN.test(lease.leaseId) && isSha1(lease.head) &&
    (expectedHead == null || lease.head === expectedHead) &&
    (expectedLeaseId == null || lease.leaseId === expectedLeaseId) &&
    Number.isFinite(acquired) && acquired <= Date.now() &&
    Number.isFinite(expires) && expires > acquired &&
    expires - acquired <= PUBLICATION_LEASE_MS &&
    isCiSourcePublication(lease.publication) && lease.publication.head === lease.head;
}

function validPublicationLeaseStorageEntry(
  key: string,
  lease: CiPublicationLeaseRecord,
): boolean {
  return validPublicationLeaseRecord(lease) && key === `${PUBLICATION_LEASE_PREFIX}${lease.head}`;
}

function publicPublicationLease(lease: CiPublicationLeaseRecord): CiPublicationLease {
  return {
    version: lease.version,
    kind: lease.kind,
    leaseId: lease.leaseId,
    head: lease.head,
    acquiredAt: lease.acquiredAt,
    expiresAt: lease.expiresAt,
  };
}

function sourceObjectRetiringKey(key: string): string {
  return `${SOURCE_OBJECT_RETIRING_PREFIX}${key}`;
}

function validSourceObjectRetiring(
  marker: CiSourceObjectRetiringRecord,
  expectedKey?: string,
): boolean {
  const claimed = Date.parse(marker.claimedAt);
  const reconcile = Date.parse(marker.reconcileAfter);
  return marker.version === 1 && ownedCiSourceObject(marker.key) &&
    (expectedKey == null || marker.key === expectedKey) &&
    CLOSE_ID_PATTERN.test(marker.claimId) && Number.isFinite(claimed) &&
    Number.isFinite(reconcile) && reconcile > claimed &&
    reconcile - claimed <= SOURCE_GC_RECONCILE_MS;
}

function validSourceObjectRetiringStorageEntry(
  key: string,
  marker: CiSourceObjectRetiringRecord,
): boolean {
  return validSourceObjectRetiring(marker) && key === sourceObjectRetiringKey(marker.key);
}

function validOutboxRecord(key: string, value: unknown): value is CiOutboxRecord {
  if (value == null || typeof value !== "object" || Array.isArray(value)) return false;
  const record = value as Partial<CiOutboxRecord>;
  if (record.run == null || typeof record.run !== "object") return false;
  return record.version === 1 && validRun(record.run, record.run.head) &&
    record.run.state === "pending" && key === `${OUTBOX_PREFIX}${record.run.head}`;
}

function validOutboxForRun(
  key: string,
  outbox: CiOutboxRecord | undefined,
  run: CiRunRecord,
): boolean {
  return run.state === "pending"
    ? outbox != null && validOutboxRecord(key, outbox) && sameRun(outbox.run, run)
    : outbox == null;
}

function validCancellation(cancellation: CiCancellationRecord, head: string): boolean {
  const requested = Date.parse(cancellation.requestedAt);
  const notBefore = Date.parse(cancellation.notBefore);
  const pullRequestCancellation = cancellation.reason === "pull_request_superseded" ||
    cancellation.reason === "pull_request_closed";
  return cancellation.version === 1 && cancellation.head === head && isSha1(head) &&
    cancellation.workflowId === `ci-${head}` &&
    (pullRequestCancellation || cancellation.reason === "operator_terminated") &&
    (pullRequestCancellation
      ? Number.isSafeInteger(cancellation.pullRequest) && Number(cancellation.pullRequest) > 0
      : cancellation.pullRequest == null && cancellation.replacementHead == null) &&
    Number.isFinite(requested) && Number.isFinite(notBefore) && notBefore >= requested &&
    notBefore - requested <= PULL_REQUEST_CANCELLATION_GRACE_MS &&
    (cancellation.state === "pending" || cancellation.state === "terminating" ||
      cancellation.state === "complete") &&
    Number.isSafeInteger(cancellation.attempts) && cancellation.attempts >= 0 &&
    (cancellation.replacementHead == null || isSha1(cancellation.replacementHead)) &&
    validOptionalTimestamp(cancellation.nextAttemptAt) &&
    validOptionalTimestamp(cancellation.claimedAt) &&
    validOptionalTimestamp(cancellation.completedAt) &&
    (cancellation.state === "pending"
      ? cancellation.claimId == null && cancellation.claimedAt == null &&
        cancellation.completedAt == null
      : CLOSE_ID_PATTERN.test(cancellation.claimId ?? "") &&
        cancellation.claimedAt != null &&
        (cancellation.state !== "complete" || cancellation.completedAt != null)) &&
    (cancellation.lastError == null || typeof cancellation.lastError === "string");
}

function validOptionalTimestamp(value: string | undefined): boolean {
  return value == null || Number.isFinite(Date.parse(value));
}

function laterTimestamp(left: string, right: string): string {
  return Date.parse(left) >= Date.parse(right) ? left : right;
}

export function isCiTerminalResult(
  value: unknown,
  head: string,
  workflowId: string,
): value is CiTerminalResult {
  if (value == null || typeof value !== "object" || Array.isArray(value)) return false;
  const result = value as Record<string, unknown>;
  return isSha1(head) && workflowId === `ci-${head}` && result.version === 1 &&
    result.head === head && result.workflowId === workflowId &&
    (result.status === "success" || result.status === "failure" ||
      result.status === "terminated") &&
    typeof result.completedAt === "string" && Number.isFinite(Date.parse(result.completedAt));
}

function exactCompletedTermination(
  value: unknown,
  head: string,
  workflowId: string,
): boolean {
  if (value == null || typeof value !== "object" || Array.isArray(value)) return false;
  const termination = value as Record<string, unknown>;
  return termination.version === 1 && termination.status === "complete" &&
    termination.head === head && termination.workflowId === workflowId &&
    typeof termination.claimId === "string" && CLOSE_ID_PATTERN.test(termination.claimId) &&
    typeof termination.completedAt === "string" &&
    Number.isFinite(Date.parse(termination.completedAt));
}

function cancellationResponse(cancellation: CiCancellationRecord, status: number): Response {
  return Response.json({
    status: cancellation.state === "complete" ? "complete" : "accepted",
    head: cancellation.head,
    workflowId: cancellation.workflowId,
    reason: cancellation.reason,
    ...(cancellation.claimId ? { claimId: cancellation.claimId } : {}),
    ...(cancellation.completedAt ? { completedAt: cancellation.completedAt } : {}),
  }, { status, headers: { "cache-control": "no-store" } });
}

async function boundedR2Json(
  object: R2ObjectBody | null,
  maximumBytes: number,
): Promise<unknown> {
  if (!object || object.size > maximumBytes) {
    await object?.body.cancel().catch(() => undefined);
    return undefined;
  }
  return object.json().catch(() => undefined);
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
