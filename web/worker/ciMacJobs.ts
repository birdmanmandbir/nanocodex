import { terminationMarkerKey } from "./ciSandboxes.ts";

const JOB_PREFIX = "job:";
const CLAIM_PREFIX = "claim:";
const OUTBOX_PREFIX = "outbox:";
const RECONCILIATION_PREFIX = "reconciliation:";
const RETENTION_PREFIX = "retention:";
const WORKER_PREFIX = "worker:";
const PUBLIC_PATH_PREFIX = "/api/ci/macos";
const JSON_HEADERS = { "content-type": "application/json" };
const SHA1 = /^[a-f0-9]{40}$/;
const SHA256 = /^[a-f0-9]{64}$/;
const WORKFLOW_ID = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,99}$/;
const WORKER_ID = /^[\x21-\x7e]{1,128}$/;
const TERMINAL_WORKFLOW_STATES = new Set(["complete", "errored", "terminated"]);
const MAX_SOURCE_BYTES = 128 * 1024 * 1024;
const MAX_CARGO_VENDOR_BYTES = 256 * 1024 * 1024;
const MAX_LOG_BYTES = 64 * 1024 * 1024;
const MAX_ASSET_BYTES = 128 * 1024 * 1024;
const MAX_DURATION_MS = 24 * 60 * 60 * 1_000;
const MAX_ERROR_BYTES = 2_000;
const MAX_OUTBOX_DELAY_MS = 5 * 60 * 1_000;
const MAX_RETENTION_OBJECTS_PER_ALARM = 1_000;
const MAX_QUEUED_RECONCILIATIONS_PER_ALARM = 16;
const QUEUED_RECONCILIATION_INITIAL_DELAY_MS = 5 * 60 * 1_000;
const MAX_QUEUED_RECONCILIATION_DELAY_MS = 6 * 60 * 60 * 1_000;
const WORKFLOW_METADATA_RECONCILIATION_DEADLINE_MS = 28 * 24 * 60 * 60 * 1_000;
const MAX_TERMINAL_RESULT_BYTES = 8 * 1024 * 1024;
const MAX_TERMINATION_MARKER_BYTES = 64 * 1024;
const CLOSE_ID_PATTERN =
  /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const MACOS_PLATFORM = "aarch64-apple-darwin";
const MACOS_ASSET_NAME = `nanocodex-${MACOS_PLATFORM}`;
export const CI_MAC_EVENT_TYPE = "macos-job-completed";

export const CI_MAC_CLAIM_LEASE_MS = 2 * 60 * 1_000;
export const CI_MAC_RETENTION_MS = 7 * 24 * 60 * 60 * 1_000;
export const CI_MAC_TASKS = [
  "workspace-test",
  "native-build",
  "release-build",
] as const;

export type CiMacTask = typeof CI_MAC_TASKS[number];

export type CiMacSource = {
  url: string;
  size: number;
  sha256: string;
};

export type CiMacCargoVendor = {
  url: string;
  size: number;
  sha256: string;
};

export type CiMacRelease = {
  channel: "nightly" | "stable";
  tagName: string;
  buildTimestamp: string;
};

export type CiMacHost = {
  hostname: string;
  platform: "darwin";
  arch: "arm64";
};

export type CiMacLog = {
  key: string;
  size: number;
  sha256: string;
  contentType: "text/plain; charset=utf-8";
};

export type CiMacAsset = {
  name: typeof MACOS_ASSET_NAME;
  platform: typeof MACOS_PLATFORM;
  key: string;
  size: number;
  sha256: string;
  contentType: "application/octet-stream";
};

export type CiMacResult = {
  outcome: "success" | "failure";
  exitCode: number;
  durationMs: number;
  error?: string;
  worker: string;
  host: CiMacHost;
  logs: { stdout: CiMacLog; stderr: CiMacLog };
  asset?: CiMacAsset;
};

export type CiMacWorkerRecord = {
  version: 1;
  id: string;
  host: CiMacHost;
  firstSeenAt: string;
  lastSeenAt: string;
};

export type CiMacJobRecord = {
  version: 1;
  id: string;
  head: string;
  workflowId: string;
  task: CiMacTask;
  source: CiMacSource;
  cargoVendor: CiMacCargoVendor;
  publishedAt?: string;
  release?: CiMacRelease;
  state: "queued" | "claimed" | "completed" | "cancelled";
  queuedAt: string;
  attempts: number;
  claim?: {
    id: string;
    worker: string;
    host: CiMacHost;
    claimedAt: string;
    heartbeatAt: string;
    leaseExpiresAt: string;
    logs: Partial<Record<"stdout" | "stderr", CiMacLog>>;
    asset?: CiMacAsset;
  };
  result?: CiMacResult;
  completionFingerprint?: string;
  completedAt?: string;
  cancelledAt?: string;
  cancellation?: { reason: "workflow_terminal"; workflowStatus: string };
  retention?: { deleteAfter: string };
  delivery?: {
    state: "pending" | "delivered" | "cancelled";
    attempts: number;
    deliveredAt?: string;
    workflowStatus?: string;
    lastError?: string;
  };
};

type CiMacJobsEnv = {
  BACKUP_BUCKET: R2Bucket;
  CI_WORKFLOW: Workflow;
};

type ClaimIndex = { version: 1; jobId: string };

type CompletionInput = {
  outcome: "success" | "failure";
  exitCode: number;
  durationMs: number;
  error?: string;
  logs: { stdout: CiMacLog; stderr: CiMacLog };
  asset?: CiMacAsset;
};

type OutboxRecord = {
  version: 1;
  jobId: string;
  workflowId: string;
  event: { type: typeof CI_MAC_EVENT_TYPE; payload: CiMacCompletionEvent };
  attempts: number;
  nextAttemptAt: string;
  lastError?: string;
};

type RetentionRecord = {
  version: 1;
  kind: "attempt" | "job";
  jobId: string;
  claimId?: string;
  prefix: string;
  deleteAfter: string;
  nextAttemptAt: string;
  attempts: number;
  lastError?: string;
};

type QueuedReconciliationRecord = {
  version: 1;
  jobId: string;
  head: string;
  workflowId: string;
  attempts: number;
  nextAttemptAt: string;
  metadataDeadlineAt: string;
  lastError?: string;
};

export type CiMacCompletionEvent = {
  version: 1;
  jobId: string;
  head: string;
  workflowId: string;
  task: CiMacTask;
  result: CiMacResult;
  completedAt: string;
};

type WorkflowState = Awaited<ReturnType<WorkflowInstance["status"]>>["status"];
type WorkflowStatusLookup =
  | { available: true; status: Exclude<WorkflowState, "unknown"> }
  | { available: false };

export class CiMacJobs {
  readonly #state: DurableObjectState;
  readonly #env: CiMacJobsEnv;

  constructor(state: DurableObjectState, env: CiMacJobsEnv) {
    this.#state = state;
    this.#env = env;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const path = normalizePath(url.pathname);
    if (path === "/maintenance/reconcile" && request.method === "POST") {
      const now = Date.now();
      await this.#backfillQueuedReconciliations(now);
      const reconciled = await this.#reconcileQueuedPage(now, true);
      await this.#scheduleNextAlarm();
      return Response.json({ reconciled }, { headers: JSON_HEADERS });
    }
    if (path === "/jobs" && request.method === "POST") {
      return this.#queue(await request.json().catch(() => undefined));
    }
    const job = path.match(
      /^\/jobs\/(macos-(?:(?:workspace-test|native-build)-[a-f0-9]{40}|release-build-[a-zA-Z0-9][a-zA-Z0-9._-]{0,99}))$/,
    );
    if (job && request.method === "GET") return this.#readJob(job[1]!);
    if (path === "/claims" && request.method === "POST") {
      return this.#claim(await request.json().catch(() => undefined));
    }
    if (path === "/workers" && request.method === "GET") {
      const workers = await this.#state.storage.list<CiMacWorkerRecord>({
        prefix: WORKER_PREFIX,
      });
      return Response.json({
        workers: [...workers.values()].sort((left, right) =>
          right.lastSeenAt.localeCompare(left.lastSeenAt) ||
          left.id.localeCompare(right.id)
        ),
      }, { headers: JSON_HEADERS });
    }
    const heartbeat = path.match(/^\/claims\/([0-9a-f-]{36})\/heartbeat$/);
    if (heartbeat && request.method === "POST") {
      return this.#heartbeat(heartbeat[1]!);
    }
    const log = path.match(
      /^\/claims\/([0-9a-f-]{36})\/logs\/(stdout|stderr)$/,
    );
    if (log && request.method === "PUT") {
      return this.#uploadLog(log[1]!, log[2]! as "stdout" | "stderr", request);
    }
    const asset = path.match(/^\/claims\/([0-9a-f-]{36})\/asset$/);
    if (asset && request.method === "PUT") {
      return this.#uploadAsset(asset[1]!, request);
    }
    const complete = path.match(/^\/claims\/([0-9a-f-]{36})\/complete$/);
    if (complete && request.method === "POST") {
      return this.#complete(
        complete[1]!,
        await request.json().catch(() => undefined),
      );
    }
    return error("not_found", 404);
  }

  async alarm(): Promise<void> {
    const now = Date.now();
    await this.#releaseExpiredClaims(now);
    await this.#backfillQueuedReconciliations(now);
    await this.#reconcileQueuedPage(now);
    const outbox = await this.#state.storage.list<OutboxRecord>({
      prefix: OUTBOX_PREFIX,
    });
    const pending = [...outbox.entries()]
      .filter(([, record]) => timestamp(record.nextAttemptAt) <= now)
      .sort(([, left], [, right]) =>
        timestamp(left.nextAttemptAt) - timestamp(right.nextAttemptAt) ||
        left.jobId.localeCompare(right.jobId)
      )[0];
    if (pending) await this.#deliver(pending[0], pending[1]);
    await this.#collectRetained(now);
    await this.#scheduleNextAlarm();
  }

  async #queue(input: unknown): Promise<Response> {
    if (!isQueueInput(input)) return error("invalid_job", 400);
    const id = jobId(input.task, input.head, input.workflowId);
    const now = Date.now();
    const queued = await this.#state.storage.transaction(
      async (transaction) => {
        const existing = await transaction.get<CiMacJobRecord>(jobKey(id));
        if (existing) {
          if (!sameJob(existing, input)) {
            return { status: 409, error: "job_conflict" };
          }
          if (existing.state === "queued") {
            const key = reconciliationKey(existing.id);
            const retained = await transaction.get<QueuedReconciliationRecord>(
              key,
            );
            const reconciliation = validQueuedReconciliation(retained, existing)
              ? retained
              : queuedReconciliation(existing, now);
            if (reconciliation !== retained) {
              await transaction.put(key, reconciliation);
            }
            await this.#armTransactionAlarm(
              transaction,
              timestamp(reconciliation.nextAttemptAt),
            );
          }
          return { status: 200, job: existing };
        }
        const job: CiMacJobRecord = {
          version: 1,
          id,
          head: input.head,
          workflowId: input.workflowId,
          task: input.task,
          source: input.source,
          cargoVendor: input.cargoVendor,
          ...(input.publishedAt ? { publishedAt: input.publishedAt } : {}),
          ...(input.release ? { release: input.release } : {}),
          state: "queued",
          queuedAt: new Date(now).toISOString(),
          attempts: 0,
        };
        const reconciliation = queuedReconciliation(
          job,
          now + QUEUED_RECONCILIATION_INITIAL_DELAY_MS,
        );
        await transaction.put({
          [jobKey(id)]: job,
          [reconciliationKey(id)]: reconciliation,
        });
        await this.#armTransactionAlarm(
          transaction,
          timestamp(reconciliation.nextAttemptAt),
        );
        return { status: 201, job };
      },
    );
    return "error" in queued && queued.error
      ? error(queued.error, queued.status)
      : Response.json({ job: queued.job }, {
        status: queued.status,
        headers: JSON_HEADERS,
      });
  }

  async #readJob(id: string): Promise<Response> {
    const job = await this.#state.storage.get<CiMacJobRecord>(jobKey(id));
    return job
      ? Response.json({ job }, { headers: JSON_HEADERS })
      : error("job_not_found", 404);
  }

  async #claim(input: unknown): Promise<Response> {
    if (!isClaimInput(input)) return error("invalid_claim", 400);
    const worker = input.worker.trim();
    const pollStartedAt = Date.now();
    await this.#touchWorker(worker, input.host, pollStartedAt);
    await this.#releaseExpiredClaims(pollStartedAt);
    const jobs = await this.#state.storage.list<CiMacJobRecord>({
      prefix: JOB_PREFIX,
    });
    const candidates = [...jobs.values()]
      .filter(({ state }) => state === "queued")
      .sort((left, right) =>
        left.queuedAt.localeCompare(right.queuedAt) ||
        left.id.localeCompare(right.id)
      );
    let unavailable = false;
    for (const candidate of candidates) {
      const workflow = await this.#workflowStatus(candidate.workflowId);
      if (!workflow.available) {
        unavailable = true;
        continue;
      }
      if (TERMINAL_WORKFLOW_STATES.has(workflow.status)) {
        await this.#cancelQueued(candidate.id, workflow.status);
        continue;
      }
      const claimId = crypto.randomUUID();
      const claimedAt = Date.now();
      const leaseExpiresAt = claimedAt + CI_MAC_CLAIM_LEASE_MS;
      const claimed = await this.#state.storage.transaction(
        async (transaction) => {
          const current = await transaction.get<CiMacJobRecord>(
            jobKey(candidate.id),
          );
          if (!current || current.state !== "queued") return undefined;
          const next: CiMacJobRecord = {
            ...current,
            state: "claimed",
            attempts: current.attempts + 1,
            claim: {
              id: claimId,
              worker,
              host: input.host,
              claimedAt: new Date(claimedAt).toISOString(),
              heartbeatAt: new Date(claimedAt).toISOString(),
              leaseExpiresAt: new Date(leaseExpiresAt).toISOString(),
              logs: {},
            },
          };
          await transaction.put({
            [jobKey(next.id)]: next,
            [claimKey(claimId)]: {
              version: 1,
              jobId: next.id,
            } satisfies ClaimIndex,
          });
          await transaction.delete(reconciliationKey(next.id));
          return next;
        },
      );
      if (!claimed) continue;
      await this.#scheduleNextAlarm();
      return Response.json({
        action: "run",
        claim: claimId,
        leaseExpiresAt: claimed.claim!.leaseExpiresAt,
        job: {
          id: claimed.id,
          head: claimed.head,
          workflowId: claimed.workflowId,
          task: claimed.task,
          source: claimed.source,
          cargoVendor: claimed.cargoVendor,
          ...(claimed.publishedAt ? { publishedAt: claimed.publishedAt } : {}),
          ...(claimed.release ? { release: claimed.release } : {}),
        },
      }, { headers: JSON_HEADERS });
    }
    if (unavailable) return error("workflow_status_unavailable", 503);
    return Response.json(
      { action: "idle", retryAfterMs: 5_000 },
      { headers: { ...JSON_HEADERS, "retry-after": "5" } },
    );
  }

  async #touchWorker(
    worker: string,
    host: CiMacHost,
    now: number,
  ): Promise<void> {
    const key = `${WORKER_PREFIX}${worker}`;
    await this.#state.storage.transaction(async (transaction) => {
      const existing = await transaction.get<CiMacWorkerRecord>(key);
      await transaction.put(
        key,
        {
          version: 1,
          id: worker,
          host,
          firstSeenAt: existing?.firstSeenAt ?? new Date(now).toISOString(),
          lastSeenAt: new Date(now).toISOString(),
        } satisfies CiMacWorkerRecord,
      );
    });
  }

  async #heartbeat(claimId: string): Promise<Response> {
    const active = await this.#activeClaim(claimId);
    if (!active) return error("claim_absent_or_expired", 404);
    const workflow = await this.#workflowStatus(active.workflowId);
    if (!workflow.available) return error("workflow_status_unavailable", 503);
    if (TERMINAL_WORKFLOW_STATES.has(workflow.status)) {
      await this.#cancelClaim(active, workflow.status);
      return cancellation(workflow.status, 200);
    }
    const now = Date.now();
    const leaseExpiresAt = now + CI_MAC_CLAIM_LEASE_MS;
    const renewed = await this.#state.storage.transaction(
      async (transaction) => {
        const current = await transaction.get<CiMacJobRecord>(
          jobKey(active.id),
        );
        if (!isCurrentClaim(current, claimId, now)) return false;
        current.claim.heartbeatAt = new Date(now).toISOString();
        current.claim.leaseExpiresAt = new Date(leaseExpiresAt).toISOString();
        const workerKey = `${WORKER_PREFIX}${current.claim.worker}`;
        const worker = await transaction.get<CiMacWorkerRecord>(workerKey);
        await transaction.put({
          [jobKey(current.id)]: current,
          [workerKey]: {
            version: 1,
            id: current.claim.worker,
            host: current.claim.host,
            firstSeenAt: worker?.firstSeenAt ?? current.claim.claimedAt,
            lastSeenAt: new Date(now).toISOString(),
          } satisfies CiMacWorkerRecord,
        });
        return true;
      },
    );
    if (!renewed) return error("claim_absent_or_expired", 404);
    await this.#scheduleNextAlarm();
    return new Response(null, {
      status: 204,
      headers: {
        "x-nanocodex-lease-expires-at": new Date(leaseExpiresAt).toISOString(),
      },
    });
  }

  async #uploadLog(
    claimId: string,
    stream: "stdout" | "stderr",
    request: Request,
  ): Promise<Response> {
    if (request.headers.get("content-type") !== "text/plain; charset=utf-8") {
      await request.body?.cancel();
      return error("invalid_log_content_type", 415);
    }
    const metadata = uploadMetadata(request, MAX_LOG_BYTES, true);
    if ("error" in metadata) return error(metadata.error, metadata.status);
    const active = await this.#claimForMutation(claimId);
    if (active instanceof Response) {
      await request.body?.cancel();
      return active;
    }
    const existing = active.claim!.logs[stream];
    if (existing) {
      await request.body?.cancel();
      return existing.size === metadata.size &&
          existing.sha256 === metadata.sha256
        ? Response.json(existing, { headers: JSON_HEADERS })
        : error("log_conflict", 409);
    }
    const key = `macos/jobs/${active.id}/attempts/${claimId}/${stream}.log`;
    const descriptor = await this.#storeObject(
      request,
      key,
      metadata,
      "text/plain; charset=utf-8",
      { job: active.id, claim: claimId, stream },
    );
    if (descriptor instanceof Response) return descriptor;
    const log: CiMacLog = {
      ...descriptor,
      contentType: "text/plain; charset=utf-8",
    };
    const attached = await this.#state.storage.transaction(
      async (transaction) => {
        const current = await transaction.get<CiMacJobRecord>(
          jobKey(active.id),
        );
        if (!isCurrentClaim(current, claimId, Date.now())) return false;
        const retained = current.claim.logs[stream];
        if (retained && stableJson(retained) !== stableJson(log)) return false;
        current.claim.logs[stream] = log;
        await transaction.put(jobKey(current.id), current);
        return true;
      },
    );
    return attached
      ? Response.json(log, { status: 201, headers: JSON_HEADERS })
      : error("stale_claim", 409);
  }

  async #uploadAsset(claimId: string, request: Request): Promise<Response> {
    const metadata = uploadMetadata(request, MAX_ASSET_BYTES, false);
    if ("error" in metadata) return error(metadata.error, metadata.status);
    const active = await this.#claimForMutation(claimId);
    if (active instanceof Response) {
      await request.body?.cancel();
      return active;
    }
    if (!taskProducesAsset(active.task)) {
      await request.body?.cancel();
      return error("asset_not_allowed", 409);
    }
    if (
      request.headers.get("x-nanocodex-name") !== MACOS_ASSET_NAME ||
      request.headers.get("content-type") !== "application/octet-stream"
    ) {
      await request.body?.cancel();
      return error("invalid_asset", 400);
    }
    const existing = active.claim!.asset;
    if (existing) {
      await request.body?.cancel();
      return existing.size === metadata.size &&
          existing.sha256 === metadata.sha256
        ? Response.json(existing, { headers: JSON_HEADERS })
        : error("asset_conflict", 409);
    }
    const key =
      `macos/jobs/${active.id}/attempts/${claimId}/assets/${MACOS_ASSET_NAME}`;
    const descriptor = await this.#storeObject(
      request,
      key,
      metadata,
      "application/octet-stream",
      { job: active.id, claim: claimId, platform: MACOS_PLATFORM },
    );
    if (descriptor instanceof Response) return descriptor;
    const asset: CiMacAsset = {
      name: MACOS_ASSET_NAME,
      platform: MACOS_PLATFORM,
      ...descriptor,
      contentType: "application/octet-stream",
    };
    const attached = await this.#state.storage.transaction(
      async (transaction) => {
        const current = await transaction.get<CiMacJobRecord>(
          jobKey(active.id),
        );
        if (!isCurrentClaim(current, claimId, Date.now())) return false;
        if (
          current.claim.asset &&
          stableJson(current.claim.asset) !== stableJson(asset)
        ) return false;
        current.claim.asset = asset;
        await transaction.put(jobKey(current.id), current);
        return true;
      },
    );
    return attached
      ? Response.json(asset, { status: 201, headers: JSON_HEADERS })
      : error("stale_claim", 409);
  }

  async #complete(claimId: string, input: unknown): Promise<Response> {
    if (!isCompletionInput(input)) return error("invalid_result", 400);
    const index = await this.#state.storage.get<ClaimIndex>(claimKey(claimId));
    if (!index) return error("claim_absent_or_expired", 404);
    const job = await this.#state.storage.get<CiMacJobRecord>(
      jobKey(index.jobId),
    );
    if (!job || job.claim?.id !== claimId) {
      return error("claim_absent_or_expired", 404);
    }
    const validated = validateCompletion(job, input);
    if (validated instanceof Response) return validated;
    const fingerprint = stableJson(input);
    if (job.state === "completed") {
      return job.completionFingerprint === fingerprint
        ? Response.json({ job }, { headers: JSON_HEADERS })
        : error("result_conflict", 409);
    }
    if (job.state === "cancelled") {
      return cancellation(
        job.cancellation?.workflowStatus ?? "terminated",
        409,
      );
    }
    const workflow = await this.#workflowStatus(job.workflowId);
    if (!workflow.available) return error("workflow_status_unavailable", 503);
    if (TERMINAL_WORKFLOW_STATES.has(workflow.status)) {
      await this.#cancelClaim(job, workflow.status);
      return cancellation(workflow.status, 409);
    }
    if (!isCurrentClaim(job, claimId, Date.now())) {
      await this.#releaseExpiredClaims(Date.now());
      return error("claim_absent_or_expired", 404);
    }
    const completedAt = new Date(Date.now()).toISOString();
    const event: CiMacCompletionEvent = {
      version: 1,
      jobId: job.id,
      head: job.head,
      workflowId: job.workflowId,
      task: job.task,
      result: validated,
      completedAt,
    };
    const outbox: OutboxRecord = {
      version: 1,
      jobId: job.id,
      workflowId: job.workflowId,
      event: { type: CI_MAC_EVENT_TYPE, payload: event },
      attempts: 0,
      nextAttemptAt: completedAt,
    };
    const completed = await this.#state.storage.transaction(
      async (transaction) => {
        const current = await transaction.get<CiMacJobRecord>(jobKey(job.id));
        if (current?.state === "completed") {
          return current.completionFingerprint === fingerprint
            ? { kind: "replayed" as const, job: current }
            : { kind: "conflict" as const };
        }
        if (current?.state === "cancelled") {
          return {
            kind: "cancelled" as const,
            workflowStatus: current.cancellation?.workflowStatus ??
              "terminated",
          };
        }
        if (!isCurrentClaim(current, claimId, Date.now())) return undefined;
        const next: CiMacJobRecord = {
          ...current,
          state: "completed",
          result: validated,
          completionFingerprint: fingerprint,
          completedAt,
          retention: { deleteAfter: retentionDeadline(completedAt) },
          delivery: { state: "pending", attempts: 0 },
        };
        await transaction.put({
          [jobKey(next.id)]: next,
          [outboxKey(next.id)]: outbox,
          [jobRetentionKey(next.id)]: jobRetention(next.id, completedAt),
        });
        await transaction.delete(reconciliationKey(next.id));
        return { kind: "completed" as const, job: next };
      },
    );
    if (!completed) return error("stale_claim", 409);
    if (completed.kind === "replayed") {
      return Response.json({ job: completed.job }, { headers: JSON_HEADERS });
    }
    if (completed.kind === "conflict") return error("result_conflict", 409);
    if (completed.kind === "cancelled") {
      return cancellation(completed.workflowStatus, 409);
    }
    await this.#state.storage.setAlarm(Date.now());
    return Response.json({ job: completed.job }, {
      status: 202,
      headers: JSON_HEADERS,
    });
  }

  async #claimForMutation(claimId: string): Promise<CiMacJobRecord | Response> {
    const active = await this.#activeClaim(claimId);
    if (!active) return error("claim_absent_or_expired", 404);
    const workflow = await this.#workflowStatus(active.workflowId);
    if (!workflow.available) return error("workflow_status_unavailable", 503);
    if (TERMINAL_WORKFLOW_STATES.has(workflow.status)) {
      await this.#cancelClaim(active, workflow.status);
      return cancellation(workflow.status, 409);
    }
    return active;
  }

  async #activeClaim(claimId: string): Promise<CiMacJobRecord | undefined> {
    const index = await this.#state.storage.get<ClaimIndex>(claimKey(claimId));
    if (!index) return undefined;
    const job = await this.#state.storage.get<CiMacJobRecord>(
      jobKey(index.jobId),
    );
    if (!isCurrentClaim(job, claimId, Date.now())) {
      await this.#releaseExpiredClaims(Date.now());
      return undefined;
    }
    return job;
  }

  async #backfillQueuedReconciliations(now: number): Promise<void> {
    const [jobs, reconciliations] = await Promise.all([
      this.#state.storage.list<CiMacJobRecord>({ prefix: JOB_PREFIX }),
      this.#state.storage.list<QueuedReconciliationRecord>({
        prefix: RECONCILIATION_PREFIX,
      }),
    ]);
    const page = [...jobs.values()]
      .filter((job) =>
        job.state === "queued" &&
        !validQueuedReconciliation(
          reconciliations.get(reconciliationKey(job.id)),
          job,
        )
      )
      .sort((left, right) =>
        left.queuedAt.localeCompare(right.queuedAt) ||
        left.id.localeCompare(right.id)
      )
      .slice(0, MAX_QUEUED_RECONCILIATIONS_PER_ALARM);
    if (page.length === 0) return;
    await this.#state.storage.transaction(async (transaction) => {
      let nextAlarm: number | undefined;
      for (const candidate of page) {
        const job = await transaction.get<CiMacJobRecord>(jobKey(candidate.id));
        if (!job || job.state !== "queued") continue;
        const key = reconciliationKey(job.id);
        const retained = await transaction.get<QueuedReconciliationRecord>(key);
        if (validQueuedReconciliation(retained, job)) continue;
        const reconciliation = queuedReconciliation(job, now);
        await transaction.put(key, reconciliation);
        nextAlarm = Math.min(nextAlarm ?? now, now);
      }
      if (nextAlarm !== undefined) {
        await this.#armTransactionAlarm(transaction, nextAlarm);
      }
    });
  }

  async #reconcileQueuedPage(now: number, force = false): Promise<number> {
    const reconciliations = await this.#state.storage.list<
      QueuedReconciliationRecord
    >({
      prefix: RECONCILIATION_PREFIX,
    });
    const page = [...reconciliations.entries()]
      .filter(([, record]) => force || timestamp(record.nextAttemptAt) <= now)
      .sort(([, left], [, right]) =>
        timestamp(left.nextAttemptAt) - timestamp(right.nextAttemptAt) ||
        left.jobId.localeCompare(right.jobId)
      )
      .slice(0, MAX_QUEUED_RECONCILIATIONS_PER_ALARM);
    for (const [key, reconciliation] of page) {
      const job = await this.#state.storage.get<CiMacJobRecord>(
        jobKey(reconciliation.jobId),
      );
      if (!job || !validQueuedReconciliation(reconciliation, job)) {
        await this.#repairQueuedReconciliation(key, reconciliation, now);
        continue;
      }
      const workflow = await this.#workflowStatus(job.workflowId);
      if (workflow.available) {
        if (TERMINAL_WORKFLOW_STATES.has(workflow.status)) {
          await this.#cancelQueued(job.id, workflow.status, false);
        } else {
          await this.#deferQueuedReconciliation(key, reconciliation, now);
        }
        continue;
      }
      const retained = await this.#retainedTerminalStatus(job);
      if (retained) {
        await this.#cancelQueued(job.id, retained, false);
      } else {
        await this.#deferQueuedReconciliation(
          key,
          reconciliation,
          now,
          "Workflow status and exact retained terminal evidence are unavailable",
        );
      }
    }
    return page.length;
  }

  async #repairQueuedReconciliation(
    key: string,
    observed: QueuedReconciliationRecord,
    now: number,
  ): Promise<void> {
    await this.#state.storage.transaction(async (transaction) => {
      const current = await transaction.get<QueuedReconciliationRecord>(key);
      if (!current || stableJson(current) !== stableJson(observed)) return;
      const job = await transaction.get<CiMacJobRecord>(jobKey(observed.jobId));
      if (!job || job.state !== "queued" || key !== reconciliationKey(job.id)) {
        await transaction.delete(key);
        return;
      }
      const reconciliation = queuedReconciliation(job, now);
      await transaction.put(key, reconciliation);
      await this.#armTransactionAlarm(transaction, now);
    });
  }

  async #deferQueuedReconciliation(
    key: string,
    observed: QueuedReconciliationRecord,
    now: number,
    lastError?: string,
  ): Promise<void> {
    const attempts = observed.attempts + 1;
    const delay = Math.min(
      QUEUED_RECONCILIATION_INITIAL_DELAY_MS * 2 ** Math.min(attempts, 8),
      MAX_QUEUED_RECONCILIATION_DELAY_MS,
    );
    const deadline = timestamp(observed.metadataDeadlineAt);
    const delayed = now + delay;
    const nextAttemptAt = now < deadline
      ? Math.min(delayed, deadline)
      : delayed;
    const deferred: QueuedReconciliationRecord = {
      ...observed,
      attempts,
      nextAttemptAt: new Date(nextAttemptAt).toISOString(),
      ...(lastError ? { lastError } : {}),
    };
    if (!lastError) delete deferred.lastError;
    await this.#state.storage.transaction(async (transaction) => {
      const current = await transaction.get<QueuedReconciliationRecord>(key);
      if (!current || stableJson(current) !== stableJson(observed)) return;
      const job = await transaction.get<CiMacJobRecord>(jobKey(observed.jobId));
      if (!validQueuedReconciliation(current, job)) {
        await transaction.delete(key);
        return;
      }
      await transaction.put(key, deferred);
      await this.#armTransactionAlarm(transaction, nextAttemptAt);
    });
  }

  async #retainedTerminalStatus(
    job: CiMacJobRecord,
  ): Promise<WorkflowState | undefined> {
    const [result, termination] = await Promise.allSettled([
      this.#boundedRetainedJson(
        `runs/${job.head}/result.json`,
        MAX_TERMINAL_RESULT_BYTES,
      ),
      this.#boundedRetainedJson(
        terminationMarkerKey(job.head),
        MAX_TERMINATION_MARKER_BYTES,
      ),
    ]);
    if (result.status === "fulfilled") {
      const status = exactTerminalResultStatus(result.value, job);
      if (status) return status;
    }
    return termination.status === "fulfilled" &&
        exactCompletedTermination(termination.value, job)
      ? "terminated"
      : undefined;
  }

  async #boundedRetainedJson(
    key: string,
    maximumBytes: number,
  ): Promise<unknown> {
    const object = await this.#env.BACKUP_BUCKET.get(key);
    return boundedR2Json(object, maximumBytes);
  }

  async #armTransactionAlarm(
    transaction: DurableObjectTransaction,
    candidate: number,
  ): Promise<void> {
    const alarm = Math.max(Date.now(), candidate);
    const current = await transaction.getAlarm();
    if (current == null || alarm < current) await transaction.setAlarm(alarm);
  }

  async #cancelQueued(
    id: string,
    workflowStatus: WorkflowState,
    schedule = true,
  ): Promise<void> {
    const now = new Date(Date.now()).toISOString();
    await this.#state.storage.transaction(async (transaction) => {
      const job = await transaction.get<CiMacJobRecord>(jobKey(id));
      if (!job || job.state !== "queued") return;
      const retention = jobRetention(id, now);
      job.state = "cancelled";
      job.cancelledAt = now;
      job.cancellation = { reason: "workflow_terminal", workflowStatus };
      job.retention = { deleteAfter: retention.deleteAfter };
      await transaction.put({
        [jobKey(id)]: job,
        [jobRetentionKey(id)]: retention,
      });
      await transaction.delete(reconciliationKey(id));
      await this.#armTransactionAlarm(
        transaction,
        timestamp(retention.nextAttemptAt),
      );
    });
    if (schedule) await this.#scheduleNextAlarm();
  }

  async #cancelClaim(
    job: CiMacJobRecord,
    workflowStatus: WorkflowState,
  ): Promise<void> {
    const now = new Date(Date.now()).toISOString();
    await this.#state.storage.transaction(async (transaction) => {
      const current = await transaction.get<CiMacJobRecord>(jobKey(job.id));
      if (!current || current.state !== "claimed" || !current.claim) return;
      const claimId = current.claim.id;
      const retention = jobRetention(current.id, now);
      current.state = "cancelled";
      current.cancelledAt = now;
      current.cancellation = { reason: "workflow_terminal", workflowStatus };
      current.retention = { deleteAfter: retention.deleteAfter };
      await transaction.put({
        [jobKey(current.id)]: current,
        [jobRetentionKey(current.id)]: retention,
      });
      await transaction.delete(claimKey(claimId));
      await transaction.delete(reconciliationKey(current.id));
      await this.#armTransactionAlarm(
        transaction,
        timestamp(retention.nextAttemptAt),
      );
    });
    await this.#scheduleNextAlarm();
  }

  async #releaseExpiredClaims(now: number): Promise<void> {
    await this.#state.storage.transaction(async (transaction) => {
      const jobs = await transaction.list<CiMacJobRecord>({
        prefix: JOB_PREFIX,
      });
      let requeued = false;
      for (const job of jobs.values()) {
        if (
          job.state !== "claimed" || !job.claim ||
          timestamp(job.claim.leaseExpiresAt) > now
        ) continue;
        const claimId = job.claim.id;
        job.state = "queued";
        delete job.claim;
        const reconciliation = queuedReconciliation(job, now);
        await transaction.put({
          [jobKey(job.id)]: job,
          [attemptRetentionKey(claimId)]: attemptRetention(
            job.id,
            claimId,
            now,
          ),
          [reconciliationKey(job.id)]: reconciliation,
        });
        await transaction.delete(claimKey(claimId));
        requeued = true;
      }
      if (requeued) await this.#armTransactionAlarm(transaction, now);
    });
  }

  async #collectRetained(now: number): Promise<void> {
    const records = await this.#state.storage.list<RetentionRecord>({
      prefix: RETENTION_PREFIX,
    });
    const due = [...records.entries()]
      .filter(([, record]) => timestamp(record.nextAttemptAt) <= now)
      .sort(([, left], [, right]) =>
        timestamp(left.nextAttemptAt) - timestamp(right.nextAttemptAt) ||
        left.prefix.localeCompare(right.prefix)
      )[0];
    if (!due) return;
    const [key, record] = due;
    const job = await this.#state.storage.get<CiMacJobRecord>(
      jobKey(record.jobId),
    );
    const disposition = retentionDisposition(job, record, now);
    if (disposition === "drop") {
      await this.#state.storage.delete(key);
      return;
    }
    if (disposition === "defer") {
      await this.#deferRetention(key, record, now, undefined);
      return;
    }
    try {
      const page = await this.#env.BACKUP_BUCKET.list({
        prefix: record.prefix,
        limit: MAX_RETENTION_OBJECTS_PER_ALARM,
      });
      const keys = page.objects.map(({ key }) => key);
      if (keys.length > 0) await this.#env.BACKUP_BUCKET.delete(keys);
      if (page.truncated) {
        await this.#state.storage.transaction(async (transaction) => {
          const current = await transaction.get<RetentionRecord>(key);
          if (!current || stableJson(current) !== stableJson(record)) return;
          current.nextAttemptAt = new Date(now).toISOString();
          await transaction.put(key, current);
        });
        return;
      }
      await this.#state.storage.transaction(async (transaction) => {
        const current = await transaction.get<RetentionRecord>(key);
        if (!current || stableJson(current) !== stableJson(record)) return;
        const currentJob = await transaction.get<CiMacJobRecord>(
          jobKey(record.jobId),
        );
        if (retentionDisposition(currentJob, current, now) !== "collect") {
          return;
        }
        if (current.kind === "job") {
          if (currentJob?.claim) {
            await transaction.delete(claimKey(currentJob.claim.id));
          }
          await transaction.delete(outboxKey(current.jobId));
          await transaction.delete(reconciliationKey(current.jobId));
          await transaction.delete(jobKey(current.jobId));
        }
        await transaction.delete(key);
      });
    } catch (cause) {
      await this.#deferRetention(key, record, now, cause);
    }
  }

  async #deferRetention(
    key: string,
    record: RetentionRecord,
    now: number,
    cause: unknown,
  ): Promise<void> {
    const attempts = cause === undefined
      ? record.attempts
      : record.attempts + 1;
    const delay = cause === undefined
      ? MAX_OUTBOX_DELAY_MS
      : Math.min(1_000 * 2 ** Math.min(attempts, 8), MAX_OUTBOX_DELAY_MS);
    await this.#state.storage.transaction(async (transaction) => {
      const current = await transaction.get<RetentionRecord>(key);
      if (!current || stableJson(current) !== stableJson(record)) return;
      current.attempts = attempts;
      current.nextAttemptAt = new Date(now + delay).toISOString();
      if (cause !== undefined) current.lastError = boundedError(cause);
      await transaction.put(key, current);
    });
  }

  async #storeObject(
    request: Request,
    key: string,
    metadata: { size: number; sha256: string },
    contentType: string,
    customMetadata: Record<string, string>,
  ): Promise<{ key: string; size: number; sha256: string } | Response> {
    if (metadata.size > 0 && request.body == null) {
      return error("missing_body", 400);
    }
    const body = request.body ?? new Uint8Array();
    const object = await this.#env.BACKUP_BUCKET.put(key, body, {
      onlyIf: { etagDoesNotMatch: "*" },
      sha256: metadata.sha256,
      httpMetadata: { contentType },
      customMetadata: { ...customMetadata, sha256: metadata.sha256 },
    });
    const retained = object ?? await this.#env.BACKUP_BUCKET.head(key);
    if (!matchesObject(retained, key, metadata.size, metadata.sha256)) {
      return error("checksum_mismatch", 422);
    }
    return { key, size: metadata.size, sha256: metadata.sha256 };
  }

  async #deliver(key: string, outbox: OutboxRecord): Promise<void> {
    try {
      const instance = await this.#env.CI_WORKFLOW.get(outbox.workflowId);
      const status = await instance.status();
      if (status.status === "unknown") {
        throw new Error("Workflow status is unknown");
      }
      if (TERMINAL_WORKFLOW_STATES.has(status.status)) {
        await this.#finishDelivery(outbox.jobId, key, {
          state: "cancelled",
          workflowStatus: status.status,
        });
        return;
      }
      await instance.sendEvent(outbox.event);
      await this.#finishDelivery(outbox.jobId, key, { state: "delivered" });
    } catch (cause) {
      const attempts = outbox.attempts + 1;
      const retryAt = Date.now() + Math.min(
        1_000 * 2 ** Math.min(attempts, 8),
        MAX_OUTBOX_DELAY_MS,
      );
      const retry: OutboxRecord = {
        ...outbox,
        attempts,
        nextAttemptAt: new Date(retryAt).toISOString(),
        lastError: boundedError(cause),
      };
      await this.#state.storage.transaction(async (transaction) => {
        const current = await transaction.get<OutboxRecord>(key);
        if (!current || current.attempts !== outbox.attempts) return;
        await transaction.put(key, retry);
        const job = await transaction.get<CiMacJobRecord>(jobKey(outbox.jobId));
        if (!job?.delivery || job.delivery.state !== "pending") return;
        job.delivery.attempts = attempts;
        job.delivery.lastError = retry.lastError;
        await transaction.put(jobKey(job.id), job);
      });
    }
  }

  async #finishDelivery(
    id: string,
    key: string,
    delivery: { state: "delivered" } | {
      state: "cancelled";
      workflowStatus: string;
    },
  ): Promise<void> {
    await this.#state.storage.transaction(async (transaction) => {
      const job = await transaction.get<CiMacJobRecord>(jobKey(id));
      if (!job?.delivery || job.delivery.state !== "pending") {
        await transaction.delete(key);
        return;
      }
      job.delivery = delivery.state === "delivered"
        ? {
          state: "delivered",
          attempts: job.delivery.attempts + 1,
          deliveredAt: new Date(Date.now()).toISOString(),
        }
        : {
          state: "cancelled",
          attempts: job.delivery.attempts,
          workflowStatus: delivery.workflowStatus,
        };
      await transaction.put(jobKey(id), job);
      await transaction.delete(key);
    });
  }

  async #workflowStatus(workflowId: string): Promise<WorkflowStatusLookup> {
    try {
      const status =
        (await (await this.#env.CI_WORKFLOW.get(workflowId)).status()).status;
      return status === "unknown"
        ? { available: false }
        : { available: true, status };
    } catch {
      return { available: false };
    }
  }

  async #scheduleNextAlarm(): Promise<void> {
    const [jobs, outbox, reconciliations, retention] = await Promise.all([
      this.#state.storage.list<CiMacJobRecord>({ prefix: JOB_PREFIX }),
      this.#state.storage.list<OutboxRecord>({ prefix: OUTBOX_PREFIX }),
      this.#state.storage.list<QueuedReconciliationRecord>({
        prefix: RECONCILIATION_PREFIX,
      }),
      this.#state.storage.list<RetentionRecord>({ prefix: RETENTION_PREFIX }),
    ]);
    const candidates = [
      ...[...jobs.values()].flatMap((job) =>
        job.state === "claimed" && job.claim
          ? [timestamp(job.claim.leaseExpiresAt)]
          : []
      ),
      ...[...outbox.values()].map(({ nextAttemptAt }) =>
        timestamp(nextAttemptAt)
      ),
      ...[...jobs.values()].flatMap((job) => {
        if (job.state !== "queued") return [];
        const reconciliation = reconciliations.get(reconciliationKey(job.id));
        return validQueuedReconciliation(reconciliation, job)
          ? [timestamp(reconciliation.nextAttemptAt)]
          : [Date.now()];
      }),
      ...[...retention.values()].map(({ nextAttemptAt }) =>
        timestamp(nextAttemptAt)
      ),
    ].filter(Number.isFinite);
    if (candidates.length > 0) {
      await this.#state.storage.setAlarm(
        Math.max(Date.now(), Math.min(...candidates)),
      );
    }
  }
}

function isQueueInput(value: unknown): value is {
  head: string;
  workflowId: string;
  task: CiMacTask;
  source: CiMacSource;
  cargoVendor: CiMacCargoVendor;
  publishedAt?: string;
  release?: CiMacRelease;
} {
  if (value == null || typeof value !== "object") return false;
  const input = value as Record<string, unknown>;
  if (
    typeof input.head !== "string" || !SHA1.test(input.head) ||
    typeof input.workflowId !== "string" ||
    !WORKFLOW_ID.test(input.workflowId) ||
    !isTask(input.task) || !isSource(input.source, input.head) ||
    !isCargoVendor(input.cargoVendor, input.source)
  ) return false;
  if (
    (input.task === "workspace-test" &&
      (input.publishedAt !== undefined || input.release !== undefined)) ||
    (input.task === "native-build" &&
      (input.workflowId !== `ci-${input.head}` ||
        !isCanonicalTimestamp(input.publishedAt) ||
        input.release !== undefined)) ||
    (input.task === "release-build" &&
      (input.publishedAt !== undefined || !isRelease(input.release)))
  ) return false;
  return Object.keys(input).every((key) =>
    [
      "head",
      "workflowId",
      "task",
      "source",
      "cargoVendor",
      "publishedAt",
      "release",
    ].includes(key)
  );
}

function isCanonicalTimestamp(value: unknown): value is string {
  if (
    typeof value !== "string" ||
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(value)
  ) return false;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) && new Date(parsed).toISOString() === value;
}

function isRelease(value: unknown): value is CiMacRelease {
  if (value == null || typeof value !== "object") return false;
  const release = value as Record<string, unknown>;
  const valid = release.channel === "nightly"
    ? release.tagName === "nightly"
    : release.channel === "stable" && typeof release.tagName === "string" &&
      /^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$/.test(
        release.tagName,
      );
  return valid && typeof release.buildTimestamp === "string" &&
    Number.isFinite(Date.parse(release.buildTimestamp)) &&
    Object.keys(release).every((key) =>
      key === "channel" || key === "tagName" || key === "buildTimestamp"
    );
}

function isSource(value: unknown, head: string): value is CiMacSource {
  if (value == null || typeof value !== "object") return false;
  const source = value as Record<string, unknown>;
  if (
    typeof source.url !== "string" ||
    typeof source.size !== "number" || !Number.isSafeInteger(source.size) ||
    source.size <= 0 || source.size > MAX_SOURCE_BYTES ||
    typeof source.sha256 !== "string" || !SHA256.test(source.sha256)
  ) return false;
  try {
    const url = new URL(source.url);
    return isSecureCiUrl(url) &&
      url.pathname === `/api/ci/source/${head}/archive` &&
      url.search === "" && url.hash === "" &&
      Object.keys(source).every((key) =>
        ["url", "size", "sha256"].includes(key)
      );
  } catch {
    return false;
  }
}

function isCargoVendor(
  value: unknown,
  sourceValue: unknown,
): value is CiMacCargoVendor {
  if (
    value == null || typeof value !== "object" || !isSourceShape(sourceValue)
  ) return false;
  const vendor = value as Record<string, unknown>;
  if (
    typeof vendor.url !== "string" ||
    typeof vendor.size !== "number" || !Number.isSafeInteger(vendor.size) ||
    vendor.size <= 0 || vendor.size > MAX_CARGO_VENDOR_BYTES ||
    typeof vendor.sha256 !== "string" || !SHA256.test(vendor.sha256)
  ) return false;
  try {
    const source = new URL(sourceValue.url);
    const url = new URL(vendor.url);
    const identity = url.pathname.match(
      /^\/api\/ci\/cargo-vendor\/[a-f0-9]{40}\/([a-f0-9]{64})\/bundle\.tar\.gz$/,
    );
    return isSecureCiUrl(url) && url.origin === source.origin &&
      identity?.[1] === vendor.sha256 &&
      url.search === "" && url.hash === "" &&
      Object.keys(vendor).every((key) =>
        ["url", "size", "sha256"].includes(key)
      );
  } catch {
    return false;
  }
}

function isSourceShape(value: unknown): value is { url: string } {
  return value != null && typeof value === "object" &&
    typeof (value as Record<string, unknown>).url === "string";
}

function isSecureCiUrl(url: URL): boolean {
  const loopback = url.hostname === "localhost" ||
    url.hostname.endsWith(".localhost") ||
    url.hostname === "127.0.0.1" || url.hostname === "[::1]";
  return !url.username && !url.password &&
    (url.protocol === "https:" || (url.protocol === "http:" && loopback));
}

function isClaimInput(
  value: unknown,
): value is { worker: string; host: CiMacHost } {
  if (value == null || typeof value !== "object") return false;
  const input = value as Record<string, unknown>;
  return typeof input.worker === "string" &&
    input.worker.trim() === input.worker && WORKER_ID.test(input.worker) &&
    isHost(input.host) &&
    Object.keys(input).every((key) => key === "worker" || key === "host");
}

function isCompletionInput(value: unknown): value is CompletionInput {
  if (value == null || typeof value !== "object") return false;
  const input = value as Record<string, unknown>;
  if (
    !["success", "failure"].includes(input.outcome as string) ||
    typeof input.exitCode !== "number" ||
    !Number.isSafeInteger(input.exitCode) ||
    input.exitCode < 0 || input.exitCode > 255 ||
    typeof input.durationMs !== "number" ||
    !Number.isSafeInteger(input.durationMs) ||
    input.durationMs < 0 || input.durationMs > MAX_DURATION_MS ||
    (input.outcome === "success"
      ? input.exitCode !== 0
      : input.exitCode === 0) ||
    (input.error !== undefined &&
      (typeof input.error !== "string" || input.error.length === 0 ||
        new TextEncoder().encode(input.error).byteLength > MAX_ERROR_BYTES)) ||
    (input.outcome === "success" && input.error !== undefined) ||
    (input.outcome === "failure" && input.asset !== undefined) ||
    !isLogs(input.logs)
  ) return false;
  if (input.asset !== undefined && !isAsset(input.asset)) return false;
  return Object.keys(input).every((key) =>
    ["outcome", "exitCode", "durationMs", "error", "logs", "asset"].includes(
      key,
    )
  );
}

function validateCompletion(
  job: CiMacJobRecord,
  input: CompletionInput,
): CiMacResult | Response {
  const stdout = job.claim?.logs.stdout;
  const stderr = job.claim?.logs.stderr;
  if (!stdout || !stderr) return error("logs_incomplete", 409);
  if (
    stableJson(input.logs.stdout) !== stableJson(stdout) ||
    stableJson(input.logs.stderr) !== stableJson(stderr)
  ) return error("logs_mismatch", 409);
  if (job.task === "workspace-test" && (input.asset || job.claim?.asset)) {
    return error("asset_not_allowed", 409);
  }
  if (taskProducesAsset(job.task) && input.outcome === "success") {
    if (!job.claim?.asset || !input.asset) return error("asset_required", 409);
    if (stableJson(job.claim.asset) !== stableJson(input.asset)) {
      return error("asset_mismatch", 409);
    }
  }
  return {
    outcome: input.outcome,
    exitCode: input.exitCode,
    durationMs: input.durationMs,
    ...(input.error === undefined ? {} : { error: input.error }),
    worker: job.claim!.worker,
    host: job.claim!.host,
    logs: { stdout, stderr },
    ...(input.outcome === "success" && input.asset
      ? { asset: input.asset }
      : {}),
  };
}

function isAsset(value: unknown): value is CiMacAsset {
  if (value == null || typeof value !== "object") return false;
  const asset = value as Record<string, unknown>;
  return asset.name === MACOS_ASSET_NAME && asset.platform === MACOS_PLATFORM &&
    typeof asset.key === "string" &&
    /^macos\/jobs\/macos-(?:native-build-[a-f0-9]{40}|release-build-[a-zA-Z0-9][a-zA-Z0-9._-]{0,99})\/attempts\/[0-9a-f-]{36}\/assets\/nanocodex-aarch64-apple-darwin$/
      .test(
        asset.key,
      ) &&
    typeof asset.size === "number" && Number.isSafeInteger(asset.size) &&
    asset.size > 0 && asset.size <= MAX_ASSET_BYTES &&
    typeof asset.sha256 === "string" && SHA256.test(asset.sha256) &&
    asset.contentType === "application/octet-stream" &&
    Object.keys(asset).every((key) =>
      ["name", "platform", "key", "size", "sha256", "contentType"].includes(key)
    );
}

function isHost(value: unknown): value is CiMacHost {
  if (value == null || typeof value !== "object") return false;
  const host = value as Record<string, unknown>;
  return typeof host.hostname === "string" &&
    /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,254}$/.test(host.hostname) &&
    host.platform === "darwin" && host.arch === "arm64" &&
    Object.keys(host).every((key) =>
      ["hostname", "platform", "arch"].includes(key)
    );
}

function isLogs(
  value: unknown,
): value is { stdout: CiMacLog; stderr: CiMacLog } {
  if (value == null || typeof value !== "object") return false;
  const logs = value as Record<string, unknown>;
  return isLog(logs.stdout, "stdout") && isLog(logs.stderr, "stderr") &&
    Object.keys(logs).every((key) => key === "stdout" || key === "stderr");
}

function isLog(value: unknown, stream: "stdout" | "stderr"): value is CiMacLog {
  if (value == null || typeof value !== "object") return false;
  const log = value as Record<string, unknown>;
  return typeof log.key === "string" &&
    new RegExp(
      `^macos/jobs/macos-(?:(?:workspace-test|native-build)-[a-f0-9]{40}|release-build-[a-zA-Z0-9][a-zA-Z0-9._-]{0,99})/attempts/[0-9a-f-]{36}/${stream}\\.log$`,
    ).test(log.key) &&
    typeof log.size === "number" && Number.isSafeInteger(log.size) &&
    log.size >= 0 && log.size <= MAX_LOG_BYTES &&
    typeof log.sha256 === "string" && SHA256.test(log.sha256) &&
    log.contentType === "text/plain; charset=utf-8" &&
    Object.keys(log).every((key) =>
      ["key", "size", "sha256", "contentType"].includes(key)
    );
}

function uploadMetadata(
  request: Request,
  maximum: number,
  allowEmpty: boolean,
): { size: number; sha256: string } | { error: string; status: number } {
  const length = request.headers.get("content-length");
  const size = length == null ? Number.NaN : Number(length);
  if (!Number.isSafeInteger(size) || size < (allowEmpty ? 0 : 1)) {
    return { error: "invalid_content_length", status: 400 };
  }
  if (size > maximum) return { error: "upload_too_large", status: 413 };
  const sha256 = request.headers.get("x-nanocodex-sha256");
  return sha256 && SHA256.test(sha256)
    ? { size, sha256 }
    : { error: "invalid_sha256", status: 400 };
}

function isCurrentClaim(
  job: CiMacJobRecord | undefined,
  claimId: string,
  now: number,
): job is CiMacJobRecord & { claim: NonNullable<CiMacJobRecord["claim"]> } {
  return job?.state === "claimed" && job.claim?.id === claimId &&
    timestamp(job.claim.leaseExpiresAt) > now;
}

function sameJob(
  job: CiMacJobRecord,
  input: {
    head: string;
    workflowId: string;
    task: CiMacTask;
    source: CiMacSource;
    cargoVendor: CiMacCargoVendor;
    publishedAt?: string;
    release?: CiMacRelease;
  },
): boolean {
  return job.head === input.head && job.workflowId === input.workflowId &&
    job.task === input.task &&
    stableJson(job.source) === stableJson(input.source) &&
    stableJson(job.cargoVendor) === stableJson(input.cargoVendor) &&
    stableJson(job.publishedAt) === stableJson(input.publishedAt) &&
    stableJson(job.release) === stableJson(input.release);
}

function isTask(value: unknown): value is CiMacTask {
  return CI_MAC_TASKS.includes(value as CiMacTask);
}

function taskProducesAsset(task: CiMacTask): boolean {
  return task === "native-build" || task === "release-build";
}

function matchesObject(
  object: R2Object | null,
  key: string,
  size: number,
  sha256: string,
): boolean {
  return object != null && object.key === key && object.size === size &&
    object.customMetadata?.sha256 === sha256 &&
    object.checksums.sha256 != null &&
    checksumHex(object.checksums.sha256) === sha256;
}

function jobId(task: CiMacTask, head: string, workflowId: string): string {
  return task === "release-build"
    ? `macos-${task}-${workflowId}`
    : `macos-${task}-${head}`;
}

function jobKey(id: string): string {
  return `${JOB_PREFIX}${id}`;
}

function claimKey(id: string): string {
  return `${CLAIM_PREFIX}${id}`;
}

function outboxKey(id: string): string {
  return `${OUTBOX_PREFIX}${id}`;
}

function reconciliationKey(id: string): string {
  return `${RECONCILIATION_PREFIX}${id}`;
}

function queuedReconciliation(
  job: CiMacJobRecord,
  nextAttemptAt: number,
): QueuedReconciliationRecord {
  return {
    version: 1,
    jobId: job.id,
    head: job.head,
    workflowId: job.workflowId,
    attempts: 0,
    nextAttemptAt: new Date(nextAttemptAt).toISOString(),
    metadataDeadlineAt: new Date(
      timestamp(job.queuedAt) + WORKFLOW_METADATA_RECONCILIATION_DEADLINE_MS,
    ).toISOString(),
  };
}

function validQueuedReconciliation(
  value: QueuedReconciliationRecord | undefined,
  job: CiMacJobRecord | undefined,
): value is QueuedReconciliationRecord {
  if (!value || !job || job.state !== "queued") return false;
  return value.version === 1 && value.jobId === job.id &&
    value.head === job.head &&
    value.workflowId === job.workflowId &&
    Number.isSafeInteger(value.attempts) &&
    value.attempts >= 0 && isCanonicalTimestamp(value.nextAttemptAt) &&
    value.metadataDeadlineAt === new Date(
        timestamp(job.queuedAt) + WORKFLOW_METADATA_RECONCILIATION_DEADLINE_MS,
      ).toISOString() &&
    (value.lastError === undefined ||
      (typeof value.lastError === "string" &&
        new TextEncoder().encode(value.lastError).byteLength <=
          MAX_ERROR_BYTES));
}

function jobRetentionKey(id: string): string {
  return `${RETENTION_PREFIX}job:${id}`;
}

function attemptRetentionKey(claimId: string): string {
  return `${RETENTION_PREFIX}attempt:${claimId}`;
}

function retentionDeadline(from: string | number): string {
  const start = typeof from === "number" ? from : timestamp(from);
  return new Date(start + CI_MAC_RETENTION_MS).toISOString();
}

function jobRetention(id: string, now: string): RetentionRecord {
  const deleteAfter = retentionDeadline(now);
  return {
    version: 1,
    kind: "job",
    jobId: id,
    prefix: `macos/jobs/${id}/`,
    deleteAfter,
    nextAttemptAt: deleteAfter,
    attempts: 0,
  };
}

function attemptRetention(
  id: string,
  claimId: string,
  now: number,
): RetentionRecord {
  const deleteAfter = retentionDeadline(now);
  return {
    version: 1,
    kind: "attempt",
    jobId: id,
    claimId,
    prefix: `macos/jobs/${id}/attempts/${claimId}/`,
    deleteAfter,
    nextAttemptAt: deleteAfter,
    attempts: 0,
  };
}

function retentionDisposition(
  job: CiMacJobRecord | undefined,
  retention: RetentionRecord,
  now: number,
): "collect" | "defer" | "drop" {
  if (retention.prefix !== expectedRetentionPrefix(retention)) return "drop";
  if (timestamp(retention.deleteAfter) > now) return "defer";
  if (retention.kind === "attempt") {
    if (!retention.claimId) return "drop";
    if (
      job?.claim?.id === retention.claimId ||
      jobReferencesPrefix(job, retention.prefix)
    ) return "drop";
    return "collect";
  }
  if (!job) return "collect";
  if (
    (job.state !== "completed" && job.state !== "cancelled") ||
    job.retention?.deleteAfter !== retention.deleteAfter
  ) return "drop";
  return job.delivery?.state === "pending" ? "defer" : "collect";
}

function expectedRetentionPrefix(retention: RetentionRecord): string {
  return retention.kind === "attempt" && retention.claimId
    ? `macos/jobs/${retention.jobId}/attempts/${retention.claimId}/`
    : retention.kind === "job"
    ? `macos/jobs/${retention.jobId}/`
    : "";
}

function jobReferencesPrefix(
  job: CiMacJobRecord | undefined,
  prefix: string,
): boolean {
  const result = job?.result;
  return result != null && (
    result.logs.stdout.key.startsWith(prefix) ||
    result.logs.stderr.key.startsWith(prefix) ||
    result.asset?.key.startsWith(prefix) === true
  );
}

function normalizePath(path: string): string {
  if (path === PUBLIC_PATH_PREFIX) return "/";
  return path.startsWith(`${PUBLIC_PATH_PREFIX}/`)
    ? path.slice(PUBLIC_PATH_PREFIX.length)
    : path;
}

function cancellation(workflowStatus: string, status: 200 | 409): Response {
  return Response.json(
    { action: "cancel", reason: "workflow_terminal", workflowStatus },
    { status, headers: JSON_HEADERS },
  );
}

function error(code: string, status: number): Response {
  return Response.json({ error: code }, { status, headers: JSON_HEADERS });
}

function timestamp(value: string): number {
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : 0;
}

function exactTerminalResultStatus(
  value: unknown,
  job: CiMacJobRecord,
): WorkflowState | undefined {
  if (value == null || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  const result = value as Record<string, unknown>;
  if (
    result.version !== 1 || result.head !== job.head ||
    result.workflowId !== job.workflowId ||
    !terminalEvidenceTimestamp(result.completedAt, job)
  ) return undefined;
  if (result.status === "success") return "complete";
  if (result.status === "failure") return "errored";
  return result.status === "terminated" ? "terminated" : undefined;
}

function exactCompletedTermination(
  value: unknown,
  job: CiMacJobRecord,
): boolean {
  if (value == null || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const termination = value as Record<string, unknown>;
  return termination.version === 1 && termination.status === "complete" &&
    termination.head === job.head &&
    termination.workflowId === job.workflowId &&
    typeof termination.claimId === "string" &&
    CLOSE_ID_PATTERN.test(termination.claimId) &&
    terminalEvidenceTimestamp(termination.completedAt, job);
}

function terminalEvidenceTimestamp(
  value: unknown,
  job: CiMacJobRecord,
): boolean {
  return isCanonicalTimestamp(value) &&
    timestamp(value) >= timestamp(job.queuedAt);
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

function stableJson(value: unknown): string {
  return JSON.stringify(canonicalJson(value)) ?? "undefined";
}

function canonicalJson(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonicalJson);
  if (value == null || typeof value !== "object") return value;
  const object = value as Record<string, unknown>;
  return Object.fromEntries(
    Object.keys(object).sort().map((key) => [key, canonicalJson(object[key])]),
  );
}

function boundedError(value: unknown): string {
  const message = value instanceof Error ? value.message : String(value);
  return message.slice(0, MAX_ERROR_BYTES);
}

function checksumHex(buffer: ArrayBuffer): string {
  return [...new Uint8Array(buffer)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}
