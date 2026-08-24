import { beforeAll, describe, expect, it, vi } from "vitest";

vi.mock("cloudflare:workers", () => ({
  DurableObject: class {
    readonly ctx: unknown;
    readonly env: unknown;

    constructor(ctx: unknown, env: unknown) {
      this.ctx = ctx;
      this.env = env;
    }
  },
}));

type RegistryModule = typeof import("../src/registry");
let registry: RegistryModule;

beforeAll(async () => {
  registry = await import("../src/registry");
});

function tenant(value: string): import("../src/registry").TenantId {
  return registry.validateTenantId(value);
}

function cursor<T extends Record<string, SqlStorageValue>>(
  rows: T[],
  rowsWritten = 0,
): SqlStorageCursor<T> {
  return {
    columnNames: rows.length === 0 ? [] : Object.keys(rows[0]),
    one: () => {
      if (rows.length !== 1) throw new Error(`expected one row, received ${rows.length}`);
      return rows[0];
    },
    raw: () => rows.map((row) => Object.values(row)),
    rowsRead: rows.length,
    rowsWritten,
    toArray: () => rows,
  } as unknown as SqlStorageCursor<T>;
}

class RegistryBindingSql {
  tenantId: string | null = null;

  exec<T extends Record<string, SqlStorageValue>>(query: string, ...bindings: unknown[]): SqlStorageCursor<T> {
    const statement = query.trim();
    if (statement.startsWith("SELECT schema_version")) {
      return cursor([{ schema_version: 2 }] as unknown as T[]);
    }
    if (statement.startsWith("SELECT tenant_id")) {
      return cursor([{ tenant_id: this.tenantId }] as unknown as T[]);
    }
    if (statement.startsWith("UPDATE registry_metadata SET tenant_id")) {
      if (this.tenantId === null) this.tenantId = bindings[0] as string;
      return cursor<T>([], 1);
    }
    if (statement.startsWith("SELECT * FROM apps ORDER BY")) return cursor<T>([]);
    if (statement.startsWith("CREATE TABLE")) return cursor<T>([]);
    throw new Error(`unexpected registry test SQL: ${statement}`);
  }
}

type BaseFixture = Readonly<{
  app: Record<string, SqlStorageValue>;
  activeRevision: Record<string, SqlStorageValue>;
  sourceHeadRevision: Record<string, SqlStorageValue>;
}>;

class AppBaseSql extends RegistryBindingSql {
  constructor(private readonly fixture: BaseFixture) {
    super();
  }

  override exec<T extends Record<string, SqlStorageValue>>(
    query: string,
    ...bindings: unknown[]
  ): SqlStorageCursor<T> {
    const statement = query.trim();
    if (statement === "SELECT * FROM apps WHERE app_id = ?") {
      return cursor([this.fixture.app as T]);
    }
    if (statement === "SELECT * FROM revisions WHERE app_id = ? AND revision_id = ?") {
      expect(bindings).toEqual([this.fixture.app.app_id, this.fixture.app.active_revision_id]);
      return cursor([this.fixture.activeRevision as T]);
    }
    if (statement.startsWith("SELECT revisions.* FROM activations")) {
      expect(bindings).toEqual([this.fixture.app.app_id]);
      return cursor([this.fixture.sourceHeadRevision as T]);
    }
    return super.exec(query, ...bindings);
  }
}

function durableRegistry(): InstanceType<RegistryModule["AppRegistry"]> {
  const sql = new RegistryBindingSql();
  const state = {
    storage: {
      sql,
      transactionSync: <T>(callback: () => T): T => callback(),
    },
  } as unknown as DurableObjectState;
  return new registry.AppRegistry(state, {});
}

function durableRegistryWithSql(sql: RegistryBindingSql): InstanceType<RegistryModule["AppRegistry"]> {
  const state = {
    storage: {
      sql,
      transactionSync: <T>(callback: () => T): T => callback(),
    },
  } as unknown as DurableObjectState;
  return new registry.AppRegistry(state, {});
}

describe("app registry validation", () => {
  it("accepts only bounded personal and team tenant IDs", () => {
    const userId = "user:123e4567-e89b-12d3-a456-426614174000";
    expect(registry.validateTenantId(userId)).toBe(userId);
    expect(registry.validateTenantId("team:provider:engineering.eu")).toBe("team:provider:engineering.eu");

    for (const value of [
      "user:not-a-uuid",
      "user:123E4567-E89B-12D3-A456-426614174000",
      "team:",
      "organization:engineering",
      `team:${"a".repeat(129)}`,
    ]) {
      expect(() => registry.validateTenantId(value)).toThrowError(expect.objectContaining({
        code: "invalid_input",
        status: 400,
      }));
    }
  });

  it("canonicalizes a bounded start job without inventing update state", () => {
    expect(registry.validateStartJob({
      jobId: "job-1",
      appId: "app-1",
      prompt: "Build a private notes app",
      createdAt: "2026-08-25T10:00:00.000Z",
    })).toEqual({
      jobId: "job-1",
      appId: "app-1",
      prompt: "Build a private notes app",
      createdAt: "2026-08-25T10:00:00.000Z",
    });
  });

  it("rejects ambiguous targets and non-canonical timestamps with stable errors", () => {
    for (const input of [
      {
        jobId: "job-1",
        appId: "app-1",
        updateAppId: "app-1",
        prompt: "update",
        createdAt: "2026-08-25T10:00:00.000Z",
      },
      {
        jobId: "job-1",
        appId: "app-1",
        prompt: "build",
        createdAt: "yesterday",
      },
    ]) {
      expect(() => registry.validateStartJob(input)).toThrowError(expect.objectContaining({
        code: "invalid_input",
        status: 400,
      }));
    }
    expect(() => registry.validateStartJob(null as unknown as Parameters<RegistryModule["validateStartJob"]>[0]))
      .toThrowError(expect.objectContaining({ code: "invalid_input", status: 400 }));
  });

  it("bounds artifact metadata before it reaches SQLite", () => {
    const digest = "a".repeat(64);
    expect(() => registry.validatePublishRevision({
      jobId: "job-1",
      revisionId: digest,
      displayName: "Notes",
      slug: "Notes",
      artifactKey: "apps/app-1/rev-1.bundle",
      artifactHash: digest,
      artifactBytes: 42,
      sourceSummary: "One module",
      sourceCommitOid: "1".repeat(40),
      generationModel: "gpt-5",
      mainModule: "app.js",
      policyVersion: 1,
      createdAt: "2026-08-25T10:01:00.000Z",
    })).toThrowError(expect.objectContaining({ code: "invalid_input", status: 400 }));

    expect(() => registry.validatePublishRevision({
      jobId: "job-1",
      revisionId: digest,
      artifactKey: "apps/app-1/rev-1.bundle",
      artifactHash: digest,
      artifactBytes: -1,
      sourceSummary: "One module",
      sourceCommitOid: "2".repeat(40),
      generationModel: "gpt-5",
      mainModule: "app.js",
      policyVersion: 1,
      createdAt: "2026-08-25T10:01:00.000Z",
    })).toThrowError(expect.objectContaining({ code: "invalid_input", status: 400 }));

    expect(() => registry.validatePublishRevision({
      jobId: "job-1",
      revisionId: digest,
      artifactKey: "apps/app-1/rev-1.bundle",
      artifactHash: "b".repeat(64),
      artifactBytes: 42,
      sourceSummary: "One module",
      sourceCommitOid: "3".repeat(40),
      generationModel: "gpt-5",
      mainModule: "app.js",
      policyVersion: 1,
      createdAt: "2026-08-25T10:01:00.000Z",
    })).toThrowError(expect.objectContaining({ code: "invalid_input", status: 400 }));
  });
});

describe("app registry tenant isolation", () => {
  it("derives stable, distinct Durable Object stubs from tenant IDs", () => {
    const stubs = new Map<string, object>();
    const namespace = {
      idFromName: (name: string) => name,
      get: (id: string) => {
        let stub = stubs.get(id);
        if (stub === undefined) {
          stub = { id };
          stubs.set(id, stub);
        }
        return stub;
      },
    } as unknown as DurableObjectNamespace<import("../src/registry").AppRegistry>;
    const personal = tenant("user:123e4567-e89b-12d3-a456-426614174000");
    const team = tenant("team:engineering");

    expect(registry.registryStub(namespace, personal)).toBe(registry.registryStub(namespace, personal));
    expect(registry.registryStub(namespace, personal)).not.toBe(registry.registryStub(namespace, team));
    expect([...stubs.keys()]).toEqual([personal, team]);
  });

  it("immutably binds a registry instance and denies every mismatched-tenant operation", () => {
    const object = durableRegistry();
    const otherObject = durableRegistry();
    const personal = tenant("user:123e4567-e89b-12d3-a456-426614174000");
    const other = tenant("team:engineering");
    expect(object.listApps(personal)).toEqual([]);
    expect(object.listApps(personal)).toEqual([]);
    expect(otherObject.listApps(other)).toEqual([]);

    const operations = [
      () => object.startJob(other, null as never),
      () => object.getJob(other, "job-1"),
      () => object.failJob(other, null as never),
      () => object.publishRevision(other, null as never),
      () => object.listApps(other),
      () => object.getApp(other, "app-1"),
      () => object.getAppBase(other, "app-1"),
      () => object.getBase(other, "app-1"),
      () => object.resolveSlug(other, "notes"),
      () => object.activateRevision(other, null as never),
    ];
    for (const operation of operations) {
      expect(operation).toThrowError(expect.objectContaining({
        code: "tenant_mismatch",
        status: 403,
      }));
    }
  });
});

describe("app registry canonical transitions", () => {
  const buildingUpdate = {
    status: "building" as const,
    targetAppId: "app-live",
    baseRevisionId: "rev-1",
    updateAppId: "app-live",
  };

  it("publishes an update only over the revision captured at job start", () => {
    expect(registry.canonicalPublishTransition(buildingUpdate, {
      appId: "app-live",
      activeRevisionId: "rev-1",
    })).toEqual({
      targetAppId: "app-live",
      previousActiveRevisionId: "rev-1",
      createApp: false,
    });

    expect(() => registry.canonicalPublishTransition(buildingUpdate, {
      appId: "app-live",
      activeRevisionId: "rev-2",
    })).toThrowError(expect.objectContaining({ code: "stale_active", status: 409 }));
  });

  it("distinguishes new app publication from an update", () => {
    expect(registry.canonicalPublishTransition({
      status: "building",
      targetAppId: "new-app",
      baseRevisionId: null,
      updateAppId: null,
    }, null)).toEqual({
      targetAppId: "new-app",
      previousActiveRevisionId: null,
      createApp: true,
    });

    expect(() => registry.canonicalPublishTransition({
      status: "building",
      targetAppId: "new-app",
      baseRevisionId: null,
      updateAppId: null,
    }, { appId: "new-app", activeRevisionId: "rev-existing" }))
      .toThrowError(expect.objectContaining({ code: "conflict", status: 409 }));
  });

  it("never republishes terminal jobs", () => {
    expect(() => registry.canonicalPublishTransition({
      ...buildingUpdate,
      status: "failed",
    }, { appId: "app-live", activeRevisionId: "rev-1" }))
      .toThrowError(expect.objectContaining({ code: "invalid_state", status: 409 }));
  });

  it("validates explicit rollback compare-and-swap input", () => {
    expect(registry.validateActivateRevision({
      appId: "app-live",
      revisionId: "rev-1",
      expectedActiveRevisionId: "rev-2",
      activatedAt: "2026-08-25T10:02:00.000Z",
      reason: "rollback",
    })).toEqual({
      appId: "app-live",
      revisionId: "rev-1",
      expectedActiveRevisionId: "rev-2",
      activatedAt: "2026-08-25T10:02:00.000Z",
      reason: "rollback",
    });
  });

  it("rejects stale or cross-app activation before mutation", () => {
    const app = { appId: "app-live", activeRevisionId: "rev-2" };
    const revision = { appId: "app-live", revisionId: "rev-1" };
    expect(() => registry.canonicalActivationTransition(app, revision, {
      appId: "app-live",
      revisionId: "rev-1",
      expectedActiveRevisionId: "rev-1",
    })).toThrowError(expect.objectContaining({ code: "stale_active", status: 409 }));

    expect(() => registry.canonicalActivationTransition(app, {
      appId: "other-app",
      revisionId: "rev-1",
    }, {
      appId: "app-live",
      revisionId: "rev-1",
      expectedActiveRevisionId: "rev-2",
    })).toThrowError(expect.objectContaining({ code: "not_found", status: 404 }));
  });
});

describe("app registry source base", () => {
  it("keeps the active generation revision separate from the append-only source head after rollback", () => {
    const owner = tenant("team:engineering");
    const revision = (revisionId: string, jobId: string, sourceCommitOid: string, createdAt: string) => ({
      revision_id: revisionId,
      app_id: "app-live",
      owner_id: owner,
      job_id: jobId,
      artifact_key: `apps/app-live/revisions/${revisionId}/worker.json`,
      artifact_hash: revisionId,
      artifact_bytes: 42,
      source_commit_oid: sourceCommitOid,
      source_summary: "{}",
      prompt: jobId,
      generation_model: "gpt-5",
      main_module: "app.js",
      policy_version: 1,
      created_at: createdAt,
    });
    const revisionA = revision("a".repeat(64), "job-a", "1".repeat(40), "2026-08-25T10:00:00.000Z");
    const revisionB = revision("b".repeat(64), "job-b", "2".repeat(40), "2026-08-25T10:01:00.000Z");
    const object = durableRegistryWithSql(new AppBaseSql({
      app: {
        app_id: "app-live",
        owner_id: owner,
        display_name: "Live app",
        slug: "live-app",
        active_revision_id: revisionA.revision_id,
        created_at: revisionA.created_at,
        updated_at: "2026-08-25T10:02:00.000Z",
      },
      activeRevision: revisionA,
      sourceHeadRevision: revisionB,
    }));

    expect(object.getAppBase(owner, "app-live")).toMatchObject({
      app: { activeRevisionId: revisionA.revision_id },
      revision: {
        revisionId: revisionA.revision_id,
        sourceCommitOid: revisionA.source_commit_oid,
      },
      sourceHeadCommitOid: revisionB.source_commit_oid,
    });
  });
});
