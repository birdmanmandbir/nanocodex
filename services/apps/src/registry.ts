import { DurableObject } from "cloudflare:workers";

declare const tenantIdBrand: unique symbol;

export type TenantId = string & { readonly [tenantIdBrand]: "TenantId" };

const LIMITS = Object.freeze({
  artifactKey: 1_024,
  displayName: 160,
  error: 4_096,
  generationModel: 128,
  id: 128,
  mainModule: 256,
  prompt: 65_536,
  sourceSummary: 8_192,
  tenantId: 160,
});

const ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:-]*$/;
const SLUG_PATTERN = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const USER_TENANT_PATTERN = /^user:[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const TEAM_TENANT_PATTERN = /^team:[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;

export interface RegistryEnv {}

export type RegistryErrorCode =
  | "invalid_input"
  | "not_found"
  | "conflict"
  | "build_in_progress"
  | "invalid_state"
  | "stale_active"
  | "tenant_mismatch"
  | "schema_mismatch";

export class RegistryError extends Error {
  constructor(
    readonly code: RegistryErrorCode,
    readonly status: 400 | 403 | 404 | 409 | 500,
    message: string,
  ) {
    super(message);
    this.name = "RegistryError";
  }
}

export type JobStatus = "building" | "completed" | "failed";

export type StartJobInput = Readonly<{
  jobId: string;
  appId: string;
  prompt: string;
  updateAppId?: string;
  createdAt: string;
}>;

export type FailJobInput = Readonly<{
  jobId: string;
  error: string;
  failedAt: string;
}>;

export type PublishRevisionInput = Readonly<{
  jobId: string;
  revisionId: string;
  displayName?: string;
  slug?: string;
  artifactKey: string;
  artifactHash: string;
  artifactBytes: number;
  sourceCommitOid: string;
  sourceSummary: string;
  generationModel: string;
  mainModule: string;
  policyVersion: number;
  createdAt: string;
}>;

export type ActivateRevisionInput = Readonly<{
  appId: string;
  revisionId: string;
  expectedActiveRevisionId: string | null;
  activatedAt: string;
  reason?: "activate" | "rollback";
}>;

export type Job = Readonly<{
  jobId: string;
  ownerId: TenantId;
  appId: string;
  updateAppId: string | null;
  targetAppId: string;
  baseRevisionId: string | null;
  prompt: string;
  status: JobStatus;
  revisionId: string | null;
  error: string | null;
  createdAt: string;
  completedAt: string | null;
}>;

export type Revision = Readonly<{
  revisionId: string;
  appId: string;
  ownerId: TenantId;
  jobId: string;
  artifactKey: string;
  artifactHash: string;
  artifactBytes: number;
  sourceCommitOid: string;
  sourceSummary: string;
  prompt: string;
  generationModel: string;
  mainModule: string;
  policyVersion: number;
  createdAt: string;
}>;

export type Activation = Readonly<{
  sequence: number;
  appId: string;
  revisionId: string;
  previousRevisionId: string | null;
  reason: "publish" | "activate" | "rollback";
  jobId: string | null;
  activatedAt: string;
}>;

export type AppRecord = Readonly<{
  appId: string;
  ownerId: TenantId;
  displayName: string;
  slug: string;
  activeRevisionId: string;
  createdAt: string;
  updatedAt: string;
}>;

export type AppSummary = AppRecord & Readonly<{
  liveSlug: string;
  revisions: readonly Revision[];
}>;

export type App = AppSummary & Readonly<{
  activationHistory: readonly Activation[];
}>;

export type AppBase = Readonly<{
  app: AppRecord;
  revision: Revision;
}>;

export type PublishTransition = Readonly<{
  targetAppId: string;
  previousActiveRevisionId: string | null;
  createApp: boolean;
}>;

export function validateTenantId(value: unknown): TenantId {
  const result = bounded(value, "tenantId", LIMITS.tenantId);
  if (!USER_TENANT_PATTERN.test(result) && !TEAM_TENANT_PATTERN.test(result)) {
    throw invalid("tenantId must be user:<uuid> or team:<id>");
  }
  return result as TenantId;
}

type AppRow = {
  app_id: string;
  owner_id: TenantId;
  display_name: string;
  slug: string;
  active_revision_id: string;
  created_at: string;
  updated_at: string;
};

type JobRow = {
  job_id: string;
  owner_id: TenantId;
  app_id: string;
  update_app_id: string | null;
  target_app_id: string;
  base_revision_id: string | null;
  prompt: string;
  status: string;
  revision_id: string | null;
  error: string | null;
  created_at: string;
  completed_at: string | null;
};

type RevisionRow = {
  revision_id: string;
  app_id: string;
  owner_id: TenantId;
  job_id: string;
  artifact_key: string;
  artifact_hash: string;
  artifact_bytes: number;
  source_commit_oid: string;
  source_summary: string;
  prompt: string;
  generation_model: string;
  main_module: string;
  policy_version: number;
  created_at: string;
};

type ActivationRow = {
  sequence: number;
  app_id: string;
  revision_id: string;
  previous_revision_id: string | null;
  reason: string;
  job_id: string | null;
  activated_at: string;
};

export function validateStartJob(input: StartJobInput): StartJobInput {
  exactKeys(input, ["jobId", "appId", "prompt", "updateAppId", "createdAt"], "start job");
  const jobId = validId(input.jobId, "jobId");
  const appId = validId(input.appId, "appId");
  const updateAppId = input.updateAppId === undefined
    ? undefined
    : validId(input.updateAppId, "updateAppId");
  if (updateAppId === appId) {
    throw invalid("updateAppId must differ from the reserved new appId");
  }
  return Object.freeze({
    jobId,
    appId,
    prompt: bounded(input.prompt, "prompt", LIMITS.prompt),
    ...(updateAppId === undefined ? {} : { updateAppId }),
    createdAt: timestamp(input.createdAt, "createdAt"),
  });
}

export function validateFailJob(input: FailJobInput): FailJobInput {
  exactKeys(input, ["jobId", "error", "failedAt"], "fail job");
  return Object.freeze({
    jobId: validId(input.jobId, "jobId"),
    error: bounded(input.error, "error", LIMITS.error),
    failedAt: timestamp(input.failedAt, "failedAt"),
  });
}

export function validatePublishRevision(input: PublishRevisionInput): PublishRevisionInput {
  exactKeys(input, [
    "jobId", "revisionId", "displayName", "slug", "artifactKey", "artifactHash",
    "artifactBytes", "sourceCommitOid", "sourceSummary", "generationModel", "mainModule", "policyVersion", "createdAt",
  ], "publish revision");
  if (!Number.isSafeInteger(input.artifactBytes) || input.artifactBytes < 1) {
    throw invalid("artifactBytes must be a positive safe integer");
  }
  if (!Number.isSafeInteger(input.policyVersion) || input.policyVersion < 1) {
    throw invalid("policyVersion must be a positive safe integer");
  }
  const revisionId = validId(input.revisionId, "revisionId");
  const artifactHash = bounded(input.artifactHash, "artifactHash", 64);
  if (!/^[0-9a-f]{64}$/.test(artifactHash)) {
    throw invalid("artifactHash must be a lowercase SHA-256 digest");
  }
  if (revisionId !== artifactHash) throw invalid("revisionId must equal artifactHash");
  if (!/^[0-9a-f]{40}$/.test(input.sourceCommitOid)) {
    throw invalid("sourceCommitOid must be a lowercase Git SHA-1 object ID");
  }
  return Object.freeze({
    jobId: validId(input.jobId, "jobId"),
    revisionId,
    ...(input.displayName === undefined
      ? {}
      : { displayName: bounded(input.displayName, "displayName", LIMITS.displayName) }),
    ...(input.slug === undefined ? {} : { slug: validSlug(input.slug) }),
    artifactKey: bounded(input.artifactKey, "artifactKey", LIMITS.artifactKey),
    artifactHash,
    artifactBytes: input.artifactBytes,
    sourceCommitOid: input.sourceCommitOid,
    sourceSummary: bounded(input.sourceSummary, "sourceSummary", LIMITS.sourceSummary),
    generationModel: bounded(input.generationModel, "generationModel", LIMITS.generationModel),
    mainModule: bounded(input.mainModule, "mainModule", LIMITS.mainModule),
    policyVersion: input.policyVersion,
    createdAt: timestamp(input.createdAt, "createdAt"),
  });
}

export function validateActivateRevision(input: ActivateRevisionInput): ActivateRevisionInput {
  exactKeys(input, [
    "appId", "revisionId", "expectedActiveRevisionId", "activatedAt", "reason",
  ], "activate revision");
  if (input.reason !== undefined && input.reason !== "activate" && input.reason !== "rollback") {
    throw invalid("reason must be activate or rollback");
  }
  return Object.freeze({
    appId: validId(input.appId, "appId"),
    revisionId: validId(input.revisionId, "revisionId"),
    expectedActiveRevisionId: input.expectedActiveRevisionId === null
      ? null
      : validId(input.expectedActiveRevisionId, "expectedActiveRevisionId"),
    activatedAt: timestamp(input.activatedAt, "activatedAt"),
    ...(input.reason === undefined ? {} : { reason: input.reason }),
  });
}

export function targetAppId(job: Pick<StartJobInput, "appId" | "updateAppId">): string {
  return job.updateAppId ?? job.appId;
}

export function canonicalPublishTransition(
  job: Pick<Job, "status" | "targetAppId" | "baseRevisionId" | "updateAppId">,
  app: Pick<AppRecord, "appId" | "activeRevisionId"> | null,
): PublishTransition {
  if (job.status !== "building") {
    throw new RegistryError("invalid_state", 409, `job is ${job.status}`);
  }
  if (job.updateAppId !== null && app === null) {
    throw new RegistryError("not_found", 404, "update target does not exist");
  }
  if (job.updateAppId === null && app !== null) {
    throw new RegistryError("conflict", 409, "new app already exists");
  }
  if (app !== null && app.appId !== job.targetAppId) {
    throw new RegistryError("conflict", 409, "job target does not match app");
  }
  if (app !== null && app.activeRevisionId !== job.baseRevisionId) {
    throw new RegistryError("stale_active", 409, "active revision changed while the job was building");
  }
  return Object.freeze({
    targetAppId: job.targetAppId,
    previousActiveRevisionId: app?.activeRevisionId ?? null,
    createApp: app === null,
  });
}

export function canonicalActivationTransition(
  app: Pick<AppRecord, "appId" | "activeRevisionId">,
  revision: Pick<Revision, "appId" | "revisionId"> | null,
  input: Pick<ActivateRevisionInput, "appId" | "revisionId" | "expectedActiveRevisionId">,
): Readonly<{ previousActiveRevisionId: string; changed: boolean }> {
  if (app.appId !== input.appId) {
    throw new RegistryError("not_found", 404, "app does not exist");
  }
  if (app.activeRevisionId !== input.expectedActiveRevisionId) {
    throw new RegistryError("stale_active", 409, "active revision does not match expectation");
  }
  if (revision === null || revision.appId !== input.appId || revision.revisionId !== input.revisionId) {
    throw new RegistryError("not_found", 404, "revision does not belong to app");
  }
  return Object.freeze({
    previousActiveRevisionId: app.activeRevisionId,
    changed: input.revisionId !== app.activeRevisionId,
  });
}

export class AppRegistry extends DurableObject<RegistryEnv> {
  constructor(ctx: DurableObjectState, env: RegistryEnv) {
    super(ctx, env);
    initializeSchema(this.ctx.storage.sql);
  }

  startJob(rawTenantId: TenantId, raw: StartJobInput): Job {
    const tenantId = this.bindTenant(rawTenantId);
    const input = validateStartJob(raw);
    return this.ctx.storage.transactionSync(() => {
      const existing = oneOrNull(this.ctx.storage.sql.exec<JobRow>(
        "SELECT * FROM jobs WHERE job_id = ?",
        input.jobId,
      ));
      if (existing !== null) {
        if (
          existing.owner_id === tenantId
          && existing.app_id === input.appId
          && existing.update_app_id === (input.updateAppId ?? null)
          && existing.prompt === input.prompt
          && existing.created_at === input.createdAt
        ) return jobFromRow(existing);
        throw new RegistryError("conflict", 409, "jobId is already in use");
      }

      const target = targetAppId(input);
      const activeBuild = oneOrNull(this.ctx.storage.sql.exec<{ job_id: string }>(
        "SELECT job_id FROM jobs WHERE target_app_id = ? AND status = 'building' LIMIT 1",
        target,
      ));
      if (activeBuild !== null) {
        throw new RegistryError("build_in_progress", 409, "an app build is already in progress");
      }
      const app = this.appRow(target);
      if (input.updateAppId !== undefined && app === null) {
        throw new RegistryError("not_found", 404, "update target does not exist");
      }
      if (input.updateAppId === undefined && app !== null) {
        throw new RegistryError("conflict", 409, "appId already exists");
      }
      const baseRevisionId = app?.active_revision_id ?? null;
      this.ctx.storage.sql.exec(
        `INSERT INTO jobs (
          job_id, owner_id, app_id, update_app_id, target_app_id, base_revision_id,
          prompt, status, created_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, 'building', ?)`,
        input.jobId,
        tenantId,
        input.appId,
        input.updateAppId ?? null,
        target,
        baseRevisionId,
        input.prompt,
        input.createdAt,
      );
      return jobFromRow(this.ctx.storage.sql.exec<JobRow>(
        "SELECT * FROM jobs WHERE job_id = ?",
        input.jobId,
      ).one());
    });
  }

  getJob(rawTenantId: TenantId, jobId: string): Job | null {
    this.bindTenant(rawTenantId);
    const id = validId(jobId, "jobId");
    const row = oneOrNull(this.ctx.storage.sql.exec<JobRow>("SELECT * FROM jobs WHERE job_id = ?", id));
    return row === null ? null : jobFromRow(row);
  }

  failJob(rawTenantId: TenantId, raw: FailJobInput): Job {
    this.bindTenant(rawTenantId);
    const input = validateFailJob(raw);
    return this.ctx.storage.transactionSync(() => {
      const row = this.requireJobRow(input.jobId);
      if (row.status === "failed" && row.error === input.error && row.completed_at === input.failedAt) {
        return jobFromRow(row);
      }
      if (row.status !== "building") {
        throw new RegistryError("invalid_state", 409, `job is already ${row.status}`);
      }
      if (input.failedAt < row.created_at) throw invalid("failedAt precedes job creation");
      this.ctx.storage.sql.exec(
        "UPDATE jobs SET status = 'failed', error = ?, completed_at = ? WHERE job_id = ?",
        input.error,
        input.failedAt,
        input.jobId,
      );
      return jobFromRow(this.requireJobRow(input.jobId));
    });
  }

  publishRevision(rawTenantId: TenantId, raw: PublishRevisionInput): App {
    const tenantId = this.bindTenant(rawTenantId);
    const input = validatePublishRevision(raw);
    return this.ctx.storage.transactionSync(() => {
      const jobRow = this.requireJobRow(input.jobId);
      const job = jobFromRow(jobRow);
      if (job.status === "completed") {
        const revision = oneOrNull(this.ctx.storage.sql.exec<RevisionRow>(
          "SELECT * FROM revisions WHERE job_id = ?",
          input.jobId,
        ));
        if (revision !== null && samePublication(revision, input, job.prompt)) {
          const app = this.requireApp(job.targetAppId);
          const newAppMetadataMatches = job.updateAppId !== null
            || (input.displayName === app.displayName && input.slug === app.slug);
          const suppliedMetadataMatches = (input.displayName === undefined || input.displayName === app.displayName)
            && (input.slug === undefined || input.slug === app.slug);
          if (newAppMetadataMatches && suppliedMetadataMatches) return app;
        }
        throw new RegistryError("conflict", 409, "completed job was published with different revision data");
      }
      if (job.status !== "building") {
        throw new RegistryError("invalid_state", 409, `job is ${job.status}`);
      }
      if (input.createdAt < job.createdAt) throw invalid("createdAt precedes job creation");

      const appRow = this.appRow(job.targetAppId);
      const appRecord = appRow === null ? null : appFromRow(appRow);
      const transition = canonicalPublishTransition(job, appRecord);
      const displayName = appRecord?.displayName ?? required(input.displayName, "displayName");
      const slug = appRecord?.slug ?? required(input.slug, "slug");
      if (input.displayName !== undefined && input.displayName !== displayName) {
        throw new RegistryError("conflict", 409, "displayName cannot change during revision publication");
      }
      if (input.slug !== undefined && input.slug !== slug) {
        throw new RegistryError("conflict", 409, "slug cannot change during revision publication");
      }
      const slugOwner = oneOrNull(this.ctx.storage.sql.exec<{ app_id: string }>(
        "SELECT app_id FROM apps WHERE slug = ?",
        slug,
      ));
      if (slugOwner !== null && slugOwner.app_id !== job.targetAppId) {
        throw new RegistryError("conflict", 409, "slug is already in use");
      }
      if (this.revisionRow(job.targetAppId, input.revisionId) !== null) {
        throw new RegistryError("conflict", 409, "revisionId is already in use for this app");
      }
      if (oneOrNull(this.ctx.storage.sql.exec<{ revision_id: string }>(
        "SELECT revision_id FROM revisions WHERE artifact_key = ?",
        input.artifactKey,
      )) !== null) {
        throw new RegistryError("conflict", 409, "artifactKey is already in use");
      }

      this.ctx.storage.sql.exec(
        `INSERT INTO revisions (
          revision_id, app_id, owner_id, job_id, artifact_key, artifact_hash, artifact_bytes,
          source_commit_oid, source_summary, prompt, generation_model, main_module, policy_version, created_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        input.revisionId,
        job.targetAppId,
        tenantId,
        input.jobId,
        input.artifactKey,
        input.artifactHash,
        input.artifactBytes,
        input.sourceCommitOid,
        input.sourceSummary,
        job.prompt,
        input.generationModel,
        input.mainModule,
        input.policyVersion,
        input.createdAt,
      );

      if (transition.createApp) {
        this.ctx.storage.sql.exec(
          `INSERT INTO apps (
            app_id, owner_id, display_name, slug, active_revision_id, created_at, updated_at
          ) VALUES (?, ?, ?, ?, ?, ?, ?)`,
          job.targetAppId,
          tenantId,
          displayName,
          slug,
          input.revisionId,
          input.createdAt,
          input.createdAt,
        );
      } else {
        const updated = this.ctx.storage.sql.exec(
          `UPDATE apps SET active_revision_id = ?, updated_at = ?
           WHERE app_id = ? AND active_revision_id = ?`,
          input.revisionId,
          input.createdAt,
          job.targetAppId,
          transition.previousActiveRevisionId,
        );
        if (updated.rowsWritten !== 1) {
          throw new RegistryError("stale_active", 409, "active revision changed while publishing");
        }
      }

      this.ctx.storage.sql.exec(
        `INSERT INTO activations (
          app_id, revision_id, previous_revision_id, reason, job_id, activated_at
        ) VALUES (?, ?, ?, 'publish', ?, ?)`,
        job.targetAppId,
        input.revisionId,
        transition.previousActiveRevisionId,
        input.jobId,
        input.createdAt,
      );
      this.ctx.storage.sql.exec(
        `UPDATE jobs SET status = 'completed', revision_id = ?, completed_at = ?
         WHERE job_id = ? AND status = 'building'`,
        input.revisionId,
        input.createdAt,
        input.jobId,
      );
      return this.requireApp(job.targetAppId);
    });
  }

  listApps(rawTenantId: TenantId): AppSummary[] {
    this.bindTenant(rawTenantId);
    return this.ctx.storage.sql.exec<AppRow>("SELECT * FROM apps ORDER BY created_at DESC, app_id").toArray()
      .map((row) => this.summaryFromRow(row));
  }

  getApp(rawTenantId: TenantId, appId: string): App | null {
    this.bindTenant(rawTenantId);
    const row = this.appRow(validId(appId, "appId"));
    return row === null ? null : this.appFromRow(row);
  }

  getAppBase(rawTenantId: TenantId, appId: string): AppBase | null {
    this.bindTenant(rawTenantId);
    const row = this.appRow(validId(appId, "appId"));
    if (row === null) return null;
    const app = appFromRow(row);
    const revision = this.revisionRow(app.appId, app.activeRevisionId);
    if (revision === null) {
      throw new RegistryError("schema_mismatch", 500, "active revision is missing");
    }
    return Object.freeze({ app, revision: revisionFromRow(revision) });
  }

  getBase(rawTenantId: TenantId, appId: string): AppBase | null {
    return this.getAppBase(rawTenantId, appId);
  }

  resolveSlug(rawTenantId: TenantId, slug: string): App | null {
    this.bindTenant(rawTenantId);
    const value = validSlug(slug);
    const row = oneOrNull(this.ctx.storage.sql.exec<AppRow>("SELECT * FROM apps WHERE slug = ?", value));
    return row === null ? null : this.appFromRow(row);
  }

  activateRevision(rawTenantId: TenantId, raw: ActivateRevisionInput): App {
    this.bindTenant(rawTenantId);
    const input = validateActivateRevision(raw);
    return this.ctx.storage.transactionSync(() => {
      const appRow = this.appRow(input.appId);
      if (appRow === null) throw new RegistryError("not_found", 404, "app does not exist");
      if (oneOrNull(this.ctx.storage.sql.exec<{ job_id: string }>(
        "SELECT job_id FROM jobs WHERE target_app_id = ? AND status = 'building' LIMIT 1",
        input.appId,
      )) !== null) {
        throw new RegistryError("build_in_progress", 409, "cannot activate a revision while a build is in progress");
      }
      const revision = this.revisionRow(input.appId, input.revisionId);
      const transition = canonicalActivationTransition(
        appFromRow(appRow),
        revision === null ? null : revisionFromRow(revision),
        input,
      );
      if (!transition.changed) return this.appFromRow(appRow);
      if (input.activatedAt < appRow.updated_at) throw invalid("activatedAt precedes the current activation");

      const updated = this.ctx.storage.sql.exec(
        `UPDATE apps SET active_revision_id = ?, updated_at = ?
         WHERE app_id = ? AND active_revision_id = ?`,
        input.revisionId,
        input.activatedAt,
        input.appId,
        input.expectedActiveRevisionId,
      );
      if (updated.rowsWritten !== 1) {
        throw new RegistryError("stale_active", 409, "active revision changed while activating");
      }
      this.ctx.storage.sql.exec(
        `INSERT INTO activations (
          app_id, revision_id, previous_revision_id, reason, job_id, activated_at
        ) VALUES (?, ?, ?, ?, NULL, ?)`,
        input.appId,
        input.revisionId,
        input.expectedActiveRevisionId,
        input.reason ?? "activate",
        input.activatedAt,
      );
      return this.requireApp(input.appId);
    });
  }

  private requireJobRow(jobId: string): JobRow {
    const row = oneOrNull(this.ctx.storage.sql.exec<JobRow>("SELECT * FROM jobs WHERE job_id = ?", jobId));
    if (row === null) throw new RegistryError("not_found", 404, "job does not exist");
    return row;
  }

  private bindTenant(rawTenantId: TenantId): TenantId {
    const tenantId = validateTenantId(rawTenantId);
    return this.ctx.storage.transactionSync(() => {
      let metadata = this.ctx.storage.sql.exec<{ tenant_id: string | null }>(
        "SELECT tenant_id FROM registry_metadata WHERE singleton = 1",
      ).one();
      if (metadata.tenant_id === null) {
        this.ctx.storage.sql.exec(
          "UPDATE registry_metadata SET tenant_id = ? WHERE singleton = 1 AND tenant_id IS NULL",
          tenantId,
        );
        metadata = this.ctx.storage.sql.exec<{ tenant_id: string | null }>(
          "SELECT tenant_id FROM registry_metadata WHERE singleton = 1",
        ).one();
      }
      if (metadata.tenant_id !== tenantId) {
        throw new RegistryError("tenant_mismatch", 403, "registry belongs to a different tenant");
      }
      return tenantId;
    });
  }

  private appRow(appId: string): AppRow | null {
    return oneOrNull(this.ctx.storage.sql.exec<AppRow>("SELECT * FROM apps WHERE app_id = ?", appId));
  }

  private revisionRow(appId: string, revisionId: string): RevisionRow | null {
    return oneOrNull(this.ctx.storage.sql.exec<RevisionRow>(
      "SELECT * FROM revisions WHERE app_id = ? AND revision_id = ?",
      appId,
      revisionId,
    ));
  }

  private summaryFromRow(row: AppRow): AppSummary {
    const app = appFromRow(row);
    return Object.freeze({
      ...app,
      liveSlug: app.slug,
      revisions: Object.freeze(this.ctx.storage.sql.exec<RevisionRow>(
        "SELECT * FROM revisions WHERE app_id = ? ORDER BY created_at DESC, revision_id",
        app.appId,
      ).toArray().map(revisionFromRow)),
    });
  }

  private appFromRow(row: AppRow): App {
    const summary = this.summaryFromRow(row);
    return Object.freeze({
      ...summary,
      activationHistory: Object.freeze(this.ctx.storage.sql.exec<ActivationRow>(
        "SELECT * FROM activations WHERE app_id = ? ORDER BY sequence DESC",
        summary.appId,
      ).toArray().map(activationFromRow)),
    });
  }

  private requireApp(appId: string): App {
    const row = this.appRow(appId);
    if (row === null) throw new RegistryError("not_found", 404, "app does not exist");
    return this.appFromRow(row);
  }
}

export function registryStub(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
): DurableObjectStub<AppRegistry> {
  const validatedTenantId = validateTenantId(tenantId);
  return namespace.get(namespace.idFromName(validatedTenantId));
}

export function startJob(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
  input: StartJobInput,
): Promise<Job> {
  return registryStub(namespace, tenantId).startJob(tenantId, input);
}

export function getJob(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
  jobId: string,
): Promise<Job | null> {
  return registryStub(namespace, tenantId).getJob(tenantId, jobId);
}

export function failJob(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
  input: FailJobInput,
): Promise<Job> {
  return registryStub(namespace, tenantId).failJob(tenantId, input);
}

export function publishRevision(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
  input: PublishRevisionInput,
): Promise<App> {
  return registryStub(namespace, tenantId).publishRevision(tenantId, input);
}

export function listApps(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
): Promise<AppSummary[]> {
  return registryStub(namespace, tenantId).listApps(tenantId);
}

export function getApp(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
  appId: string,
): Promise<App | null> {
  return registryStub(namespace, tenantId).getApp(tenantId, appId);
}

export function getAppBase(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
  appId: string,
): Promise<AppBase | null> {
  return registryStub(namespace, tenantId).getAppBase(tenantId, appId);
}

export function getBase(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
  appId: string,
): Promise<AppBase | null> {
  return registryStub(namespace, tenantId).getBase(tenantId, appId);
}

export function resolveSlug(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
  slug: string,
): Promise<App | null> {
  return registryStub(namespace, tenantId).resolveSlug(tenantId, slug);
}

export function activateRevision(
  namespace: DurableObjectNamespace<AppRegistry>,
  tenantId: TenantId,
  input: ActivateRevisionInput,
): Promise<App> {
  return registryStub(namespace, tenantId).activateRevision(tenantId, input);
}

function initializeSchema(sql: SqlStorage): void {
  sql.exec(`
    CREATE TABLE IF NOT EXISTS registry_metadata (
      singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
      schema_version INTEGER NOT NULL,
      tenant_id TEXT
    );
    INSERT OR IGNORE INTO registry_metadata (singleton, schema_version, tenant_id) VALUES (1, 2, NULL);
    CREATE TABLE IF NOT EXISTS apps (
      app_id TEXT PRIMARY KEY,
      owner_id TEXT NOT NULL,
      display_name TEXT NOT NULL,
      slug TEXT NOT NULL UNIQUE,
      active_revision_id TEXT NOT NULL,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      FOREIGN KEY (app_id, active_revision_id) REFERENCES revisions(app_id, revision_id)
        DEFERRABLE INITIALLY DEFERRED
    );
    CREATE TABLE IF NOT EXISTS jobs (
      job_id TEXT PRIMARY KEY,
      owner_id TEXT NOT NULL,
      app_id TEXT NOT NULL,
      update_app_id TEXT,
      target_app_id TEXT NOT NULL,
      base_revision_id TEXT,
      prompt TEXT NOT NULL,
      status TEXT NOT NULL CHECK (status IN ('building', 'completed', 'failed')),
      revision_id TEXT,
      error TEXT,
      created_at TEXT NOT NULL,
      completed_at TEXT,
      CHECK (
        (status = 'building' AND revision_id IS NULL AND error IS NULL AND completed_at IS NULL) OR
        (status = 'completed' AND revision_id IS NOT NULL AND error IS NULL AND completed_at IS NOT NULL) OR
        (status = 'failed' AND revision_id IS NULL AND error IS NOT NULL AND completed_at IS NOT NULL)
      )
    );
    CREATE TABLE IF NOT EXISTS revisions (
      revision_id TEXT NOT NULL,
      app_id TEXT NOT NULL,
      owner_id TEXT NOT NULL,
      job_id TEXT NOT NULL UNIQUE,
      artifact_key TEXT NOT NULL UNIQUE,
      artifact_hash TEXT NOT NULL,
      artifact_bytes INTEGER NOT NULL CHECK (artifact_bytes > 0),
      source_commit_oid TEXT NOT NULL,
      source_summary TEXT NOT NULL,
      prompt TEXT NOT NULL,
      generation_model TEXT NOT NULL,
      main_module TEXT NOT NULL,
      policy_version INTEGER NOT NULL CHECK (policy_version > 0),
      created_at TEXT NOT NULL,
      PRIMARY KEY (app_id, revision_id),
      FOREIGN KEY (app_id) REFERENCES apps(app_id) DEFERRABLE INITIALLY DEFERRED,
      FOREIGN KEY (job_id) REFERENCES jobs(job_id)
    );
    CREATE TABLE IF NOT EXISTS activations (
      sequence INTEGER PRIMARY KEY AUTOINCREMENT,
      app_id TEXT NOT NULL,
      revision_id TEXT NOT NULL,
      previous_revision_id TEXT,
      reason TEXT NOT NULL CHECK (reason IN ('publish', 'activate', 'rollback')),
      job_id TEXT,
      activated_at TEXT NOT NULL,
      FOREIGN KEY (app_id) REFERENCES apps(app_id),
      FOREIGN KEY (app_id, revision_id) REFERENCES revisions(app_id, revision_id),
      FOREIGN KEY (job_id) REFERENCES jobs(job_id)
    );
    CREATE INDEX IF NOT EXISTS revisions_by_app ON revisions(app_id, created_at DESC);
    CREATE INDEX IF NOT EXISTS activations_by_app ON activations(app_id, sequence DESC);
    CREATE UNIQUE INDEX IF NOT EXISTS jobs_one_build_per_target
      ON jobs(target_app_id) WHERE status = 'building';
    CREATE TRIGGER IF NOT EXISTS apps_tenant_owner
      BEFORE INSERT ON apps WHEN
        (SELECT tenant_id FROM registry_metadata WHERE singleton = 1) IS NULL OR
        NEW.owner_id != (SELECT tenant_id FROM registry_metadata WHERE singleton = 1)
      BEGIN SELECT RAISE(ABORT, 'app owner does not match registry tenant'); END;
    CREATE TRIGGER IF NOT EXISTS jobs_tenant_owner
      BEFORE INSERT ON jobs WHEN
        (SELECT tenant_id FROM registry_metadata WHERE singleton = 1) IS NULL OR
        NEW.owner_id != (SELECT tenant_id FROM registry_metadata WHERE singleton = 1)
      BEGIN SELECT RAISE(ABORT, 'job owner does not match registry tenant'); END;
    CREATE TRIGGER IF NOT EXISTS revisions_tenant_owner
      BEFORE INSERT ON revisions WHEN
        (SELECT tenant_id FROM registry_metadata WHERE singleton = 1) IS NULL OR
        NEW.owner_id != (SELECT tenant_id FROM registry_metadata WHERE singleton = 1)
      BEGIN SELECT RAISE(ABORT, 'revision owner does not match registry tenant'); END;
    CREATE TRIGGER IF NOT EXISTS registry_metadata_immutable
      BEFORE UPDATE ON registry_metadata WHEN
        OLD.tenant_id IS NOT NULL OR NEW.singleton != OLD.singleton OR
        NEW.schema_version != OLD.schema_version OR NEW.tenant_id IS NULL
      BEGIN SELECT RAISE(ABORT, 'registry metadata is immutable'); END;
    CREATE TRIGGER IF NOT EXISTS registry_metadata_no_delete
      BEFORE DELETE ON registry_metadata BEGIN SELECT RAISE(ABORT, 'registry metadata is immutable'); END;
    CREATE TRIGGER IF NOT EXISTS revisions_no_update
      BEFORE UPDATE ON revisions BEGIN SELECT RAISE(ABORT, 'revisions are immutable'); END;
    CREATE TRIGGER IF NOT EXISTS revisions_no_delete
      BEFORE DELETE ON revisions BEGIN SELECT RAISE(ABORT, 'revisions are immutable'); END;
    CREATE TRIGGER IF NOT EXISTS activations_no_update
      BEFORE UPDATE ON activations BEGIN SELECT RAISE(ABORT, 'activations are immutable'); END;
    CREATE TRIGGER IF NOT EXISTS activations_no_delete
      BEFORE DELETE ON activations BEGIN SELECT RAISE(ABORT, 'activations are immutable'); END;
    CREATE TRIGGER IF NOT EXISTS apps_immutable_metadata
      BEFORE UPDATE ON apps WHEN
        NEW.app_id != OLD.app_id OR NEW.owner_id != OLD.owner_id OR
        NEW.display_name != OLD.display_name OR NEW.slug != OLD.slug OR NEW.created_at != OLD.created_at
      BEGIN SELECT RAISE(ABORT, 'app metadata is immutable'); END;
    CREATE TRIGGER IF NOT EXISTS apps_no_delete
      BEFORE DELETE ON apps BEGIN SELECT RAISE(ABORT, 'apps cannot be deleted'); END;
    CREATE TRIGGER IF NOT EXISTS jobs_valid_transition
      BEFORE UPDATE ON jobs WHEN
        OLD.status != 'building' OR NEW.job_id != OLD.job_id OR NEW.owner_id != OLD.owner_id OR
        NEW.app_id != OLD.app_id OR NEW.update_app_id IS NOT OLD.update_app_id OR
        NEW.target_app_id != OLD.target_app_id OR NEW.base_revision_id IS NOT OLD.base_revision_id OR
        NEW.prompt != OLD.prompt OR NEW.created_at != OLD.created_at
      BEGIN SELECT RAISE(ABORT, 'invalid job transition'); END;
    CREATE TRIGGER IF NOT EXISTS jobs_no_delete
      BEFORE DELETE ON jobs BEGIN SELECT RAISE(ABORT, 'jobs cannot be deleted'); END;
  `);
  const version = sql.exec<{ schema_version: number }>(
    "SELECT schema_version FROM registry_metadata WHERE singleton = 1",
  ).one().schema_version;
  if (version !== 2) {
    throw new RegistryError("schema_mismatch", 500, `unsupported registry schema version ${version}`);
  }
}

function oneOrNull<T>(cursor: SqlStorageCursor<T & Record<string, SqlStorageValue>>): T | null {
  const rows = cursor.toArray();
  return (rows[0] as T | undefined) ?? null;
}

function jobFromRow(row: JobRow): Job {
  if (row.status !== "building" && row.status !== "completed" && row.status !== "failed") {
    throw new RegistryError("schema_mismatch", 500, `unknown job status ${row.status}`);
  }
  return Object.freeze({
    jobId: row.job_id,
    ownerId: row.owner_id,
    appId: row.app_id,
    updateAppId: row.update_app_id,
    targetAppId: row.target_app_id,
    baseRevisionId: row.base_revision_id,
    prompt: row.prompt,
    status: row.status,
    revisionId: row.revision_id,
    error: row.error,
    createdAt: row.created_at,
    completedAt: row.completed_at,
  });
}

function appFromRow(row: AppRow): AppRecord {
  return Object.freeze({
    appId: row.app_id,
    ownerId: row.owner_id,
    displayName: row.display_name,
    slug: row.slug,
    activeRevisionId: row.active_revision_id,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  });
}

function revisionFromRow(row: RevisionRow): Revision {
  return Object.freeze({
    revisionId: row.revision_id,
    appId: row.app_id,
    ownerId: row.owner_id,
    jobId: row.job_id,
    artifactKey: row.artifact_key,
    artifactHash: row.artifact_hash,
    artifactBytes: row.artifact_bytes,
    sourceCommitOid: row.source_commit_oid,
    sourceSummary: row.source_summary,
    prompt: row.prompt,
    generationModel: row.generation_model,
    mainModule: row.main_module,
    policyVersion: row.policy_version,
    createdAt: row.created_at,
  });
}

function activationFromRow(row: ActivationRow): Activation {
  if (row.reason !== "publish" && row.reason !== "activate" && row.reason !== "rollback") {
    throw new RegistryError("schema_mismatch", 500, `unknown activation reason ${row.reason}`);
  }
  return Object.freeze({
    sequence: row.sequence,
    appId: row.app_id,
    revisionId: row.revision_id,
    previousRevisionId: row.previous_revision_id,
    reason: row.reason,
    jobId: row.job_id,
    activatedAt: row.activated_at,
  });
}

function samePublication(row: RevisionRow, input: PublishRevisionInput, prompt: string): boolean {
  return row.revision_id === input.revisionId
    && row.artifact_key === input.artifactKey
    && row.artifact_hash === input.artifactHash
    && row.artifact_bytes === input.artifactBytes
    && row.source_commit_oid === input.sourceCommitOid
    && row.source_summary === input.sourceSummary
    && row.prompt === prompt
    && row.generation_model === input.generationModel
    && row.main_module === input.mainModule
    && row.policy_version === input.policyVersion
    && row.created_at === input.createdAt;
}

function validId(value: unknown, field: string): string {
  const result = bounded(value, field, LIMITS.id);
  if (!ID_PATTERN.test(result)) throw invalid(`${field} has invalid characters`);
  return result;
}

function validSlug(value: unknown): string {
  const result = bounded(value, "slug", 63);
  if (!SLUG_PATTERN.test(result)) throw invalid("slug must be a lowercase DNS label");
  return result;
}

function timestamp(value: unknown, field: string): string {
  const result = bounded(value, field, 32);
  const parsed = new Date(result);
  if (!Number.isFinite(parsed.valueOf()) || parsed.toISOString() !== result) {
    throw invalid(`${field} must be a canonical ISO timestamp`);
  }
  return result;
}

function bounded(value: unknown, field: string, max: number): string {
  if (typeof value !== "string" || value.length === 0 || value.length > max) {
    throw invalid(`${field} must contain 1-${max} characters`);
  }
  if (value.includes("\0")) throw invalid(`${field} must not contain NUL`);
  return value;
}

function required(value: string | undefined, field: string): string {
  if (value === undefined) throw invalid(`${field} is required for a new app`);
  return value;
}

function exactKeys(input: object, allowed: readonly string[], label: string): void {
  if (typeof input !== "object" || input === null || Array.isArray(input)) {
    throw invalid(`${label} must be an object`);
  }
  const allowedKeys = new Set(allowed);
  for (const key of Object.keys(input)) {
    if (!allowedKeys.has(key)) throw invalid(`${label} contains unknown field ${key}`);
  }
}

function invalid(message: string): RegistryError {
  return new RegistryError("invalid_input", 400, message);
}
