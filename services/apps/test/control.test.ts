import { beforeAll, describe, expect, it, vi } from "vitest";

import { RUNTIME_SESSION_AUDIENCE, type RuntimeSessionClaims } from "../src/auth";
import { buildProject, serializeArtifact } from "../src/builder";
import type { Env } from "../src/index";

vi.mock("cloudflare:workers", () => ({
  DurableObject: class {
    ctx: unknown;
    env: unknown;
    constructor(ctx: unknown, env: unknown) { this.ctx = ctx; this.env = env; }
  },
  WorkerEntrypoint: class {
    ctx: unknown;
    env: unknown;
    constructor(ctx: unknown, env: unknown) { this.ctx = ctx; this.env = env; }
  },
  WorkflowEntrypoint: class {},
}));

let AppPlatform: typeof import("../src/index").AppPlatform;
let RuntimePlatform: typeof import("../src/index").RuntimePlatform;

beforeAll(async () => {
  ({ AppPlatform, RuntimePlatform } = await import("../src/index"));
});

const USER_ID = "0198e2c4-365e-7a66-a58f-d4e5b46a7dad";
const TENANT_ID = `user:${USER_ID}` as const;
const TICKET_SECRET = "ticket-secret-that-is-at-least-32-characters";

function configuredEnv(overrides: Partial<Env> = {}): Env {
  const registry = {
    getApp: vi.fn(async () => null),
    getJob: vi.fn(async () => null),
    listApps: vi.fn(async () => []),
    startJob: vi.fn(async (tenantId: string, input: Record<string, string>) => ({
      appId: input.appId,
      baseRevisionId: null,
      completedAt: null,
      createdAt: input.createdAt,
      error: null,
      jobId: input.jobId,
      ownerId: tenantId,
      prompt: input.prompt,
      revisionId: null,
      status: "building",
      targetAppId: input.updateAppId ?? input.appId,
      updateAppId: input.updateAppId ?? null,
    })),
  };
  const launchTicket = {
    consume: vi.fn(async () => true),
    issue: vi.fn(async () => undefined),
  };
  return {
    AI: { run: vi.fn() } as unknown as Ai,
    APP_ARTIFACTS: { get: vi.fn(), put: vi.fn() } as unknown as R2Bucket,
    APP_BUILDS: { create: vi.fn(async ({ id }) => ({ id })) } as unknown as Workflow,
    APP_GIT: { request: vi.fn() },
    APP_LAUNCH_TICKETS: {
      getByName: vi.fn(() => launchTicket),
    } as unknown as Env["APP_LAUNCH_TICKETS"],
    APP_REGISTRY: {
      get: vi.fn(() => registry),
      idFromName: vi.fn((tenantId: string) => tenantId),
    } as unknown as Env["APP_REGISTRY"],
    APP_STATE: { get: vi.fn(), idFromName: vi.fn() } as unknown as Env["APP_STATE"],
    ASSETS: { fetch: vi.fn(async () => new Response("console")) } as unknown as Fetcher,
    LAUNCH_TICKET_SECRET: TICKET_SECRET,
    LOADER: { get: vi.fn() } as unknown as WorkerLoader,
    NANOCODEX_AGENTS: { createAgent: vi.fn() } as unknown as Env["NANOCODEX_AGENTS"],
    RUNTIME_ORIGIN: "https://runtime.example.test",
    ...overrides,
  };
}

function appGateway(env: Env, ctx: Record<string, unknown> = {}) {
  return new AppPlatform(
    { props: { clientId: "nanocodex-managed" }, ...ctx } as unknown as ExecutionContext,
    env,
  );
}

function runtimeGateway(env: Env, ctx: Record<string, unknown> = {}) {
  return new RuntimePlatform(
    { props: { clientId: "nanocodex-app-runtime" }, ...ctx } as unknown as ExecutionContext,
    env,
  );
}

describe("tenant app control plane", () => {
  it("keeps the console and its SPA fallback rooted under /apps", async () => {
    const rootPaths: string[] = [];
    const rootEnv = configuredEnv({
      ASSETS: {
        fetch: vi.fn(async (request: Request) => {
          rootPaths.push(new URL(request.url).pathname);
          return new Response("console");
        }),
      } as unknown as Fetcher,
    });
    const redirect = await appGateway(rootEnv).request(
      USER_ID,
      new Request("https://nanocodex.test/apps"),
    );
    expect(redirect.status).toBe(308);
    expect(redirect.headers.get("location")).toBe("/apps/");

    const root = await appGateway(rootEnv).request(
      USER_ID,
      new Request("https://nanocodex.test/apps/"),
    );
    expect(root.status).toBe(200);
    expect(await root.text()).toBe("console");
    expect(rootPaths).toEqual(["/"]);

    const nestedPaths: string[] = [];
    const nestedEnv = configuredEnv({
      ASSETS: {
        fetch: vi.fn(async (request: Request) => {
          const pathname = new URL(request.url).pathname;
          nestedPaths.push(pathname);
          return pathname === "/settings"
            ? new Response("missing", { status: 404 })
            : new Response("console");
        }),
      } as unknown as Fetcher,
    });
    const nested = await appGateway(nestedEnv).request(
      USER_ID,
      new Request("https://nanocodex.test/apps/settings"),
    );
    expect(nested.status).toBe(200);
    expect(await nested.text()).toBe("console");
    expect(nestedPaths).toEqual(["/settings", "/"]);
  });

  it("derives the personal tenant from managed identity and starts a durable build", async () => {
    const env = configuredEnv();
    const gateway = appGateway(env);
    const denied = await gateway.request("forged", new Request("https://nanocodex.test/apps/api/apps"));
    expect(denied.status).toBe(401);

    const response = await gateway.request(USER_ID, new Request("https://nanocodex.test/apps/api/builds", {
      body: JSON.stringify({ prompt: "Build me a tiny issue tracker" }),
      headers: { "content-type": "application/json", origin: "https://nanocodex.test" },
      method: "POST",
    }));
    expect(response.status).toBe(202);
    const body = await response.json<{ job: { id: string; status: string } }>();
    expect(body.job).toMatchObject({ id: expect.any(String), status: "building" });
    expect(env.APP_REGISTRY.idFromName).toHaveBeenCalledWith(TENANT_ID);
    expect(env.APP_BUILDS.create).toHaveBeenCalledWith(expect.objectContaining({
      id: body.job.id,
      params: expect.objectContaining({ tenantId: TENANT_ID }),
    }));

    const crossOrigin = await gateway.request(USER_ID, new Request("https://nanocodex.test/apps/api/builds", {
      body: JSON.stringify({ prompt: "attack" }),
      headers: { origin: "https://attacker.test" },
      method: "POST",
    }));
    expect(crossOrigin.status).toBe(403);
  });

  it("mints an account-authorized one-time launch instead of exposing a runtime URL", async () => {
    const app = appRecord();
    const registry = { getApp: vi.fn(async () => app) };
    const ticketStore = { issue: vi.fn(async () => undefined) };
    const env = configuredEnv({
      APP_LAUNCH_TICKETS: { getByName: vi.fn(() => ticketStore) } as unknown as Env["APP_LAUNCH_TICKETS"],
      APP_REGISTRY: {
        get: vi.fn(() => registry),
        idFromName: vi.fn((tenant: string) => tenant),
      } as unknown as Env["APP_REGISTRY"],
    });
    const response = await appGateway(env).request(USER_ID, new Request(
      `https://nanocodex.test/apps/api/apps/${app.appId}/launch`,
      { method: "POST", headers: { origin: "https://nanocodex.test" } },
    ));
    expect(response.status).toBe(303);
    const location = new URL(response.headers.get("location")!);
    expect(location.origin).toBe("https://runtime.example.test");
    expect(location.pathname).toBe("/__auth/launch");
    expect(location.searchParams.get("ticket")).toMatch(/^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]{43}$/);
    expect(ticketStore.issue).toHaveBeenCalledOnce();
  });

  it("loads an exact immutable revision for only the attested runtime tenant and app", async () => {
    const artifact = await buildProject({
      entryPoint: "src/index.ts",
      files: [{ path: "src/index.ts", content: "export default {fetch(){return new Response('ok')}}" }],
      name: "Tiny app",
      slug: "tiny-app",
    }, async () => ({
      mainModule: "bundle.js",
      modules: { "bundle.js": "export default {fetch(){return new Response('ok')}}" },
    }));
    const app = appRecord(artifact.revision, serializeArtifact(artifact).length);
    const registry = { getApp: vi.fn(async (_tenant: string, appId: string) => appId === app.appId ? app : null) };
    let manifest: Record<string, unknown> | undefined;
    let invoked: Request | undefined;
    const loader = {
      get: vi.fn((_name: string, load: () => Promise<Record<string, unknown>>) => ({
        getEntrypoint: () => ({
          fetch: async (request: Request) => {
            manifest = await load();
            invoked = request;
            return new Response(null, { status: 302, headers: { location: "/next", "set-cookie": "forged=1" } });
          },
        }),
      })),
    };
    const env = configuredEnv({
      APP_ARTIFACTS: { get: vi.fn(async () => ({ text: async () => serializeArtifact(artifact) })) } as unknown as R2Bucket,
      APP_REGISTRY: {
        get: vi.fn(() => registry),
        idFromName: vi.fn((tenant: string) => tenant),
      } as unknown as Env["APP_REGISTRY"],
      LOADER: loader as unknown as WorkerLoader,
    });
    const capability = { marker: "host-owned" };
    const claims = runtimeClaims(app.appId, app.slug);
    const response = await runtimeGateway(env, {
      exports: { NanocodexCapability: () => capability },
    }).invokeApp(claims, new Request(
      "https://runtime.example.test/a/tiny-app/api/items?q=one",
      { headers: { authorization: "Bearer forged", cookie: "private=1" } },
    ));

    expect(response.status).toBe(302);
    expect(response.headers.get("location")).toBe("/a/tiny-app/next");
    expect(response.headers.get("set-cookie")).toBeNull();
    expect(loader.get).toHaveBeenCalledWith(
      `${TENANT_ID}:${app.appId}:${artifact.revision}:1`,
      expect.any(Function),
    );
    expect(manifest).toMatchObject({
      env: { NANOCODEX: capability },
      globalOutbound: null,
      limits: { cpuMs: 100, subRequests: 16 },
      mainModule: "bundle.js",
    });
    expect(invoked?.url).toBe("https://app.internal/api/items?q=one");
    expect(invoked?.headers.get("cookie")).toBeNull();
    expect(invoked?.headers.get("authorization")).toBeNull();
    expect(invoked?.headers.get("x-forwarded-prefix")).toBe("/a/tiny-app");
  });

  it("conceals an app from a mismatched tenant/runtime claim", async () => {
    const registry = { getApp: vi.fn(async () => null) };
    const env = configuredEnv({
      APP_REGISTRY: {
        get: vi.fn(() => registry),
        idFromName: vi.fn((tenant: string) => tenant),
      } as unknown as Env["APP_REGISTRY"],
    });
    const response = await runtimeGateway(env).invokeApp(
      runtimeClaims("0198e2c4-365e-7a66-a58f-d4e5b46a7dae", "tiny-app"),
      new Request("https://runtime.example.test/a/tiny-app/"),
    );
    expect(response.status).toBe(404);
  });
});

function runtimeClaims(appId: string, slug: string): RuntimeSessionClaims {
  return {
    actorUserId: USER_ID,
    appId,
    audience: RUNTIME_SESSION_AUDIENCE,
    expiry: Math.floor(Date.now() / 1_000) + 3_600,
    nonce: "abcdefghijklmnopqrstuvwxyz",
    slug,
    tenantId: TENANT_ID,
    version: 1,
  };
}

function appRecord(
  revisionId = "a".repeat(64),
  artifactBytes = 123,
) {
  const appId = "0198e2c4-365e-7a66-a58f-d4e5b46a7dae";
  const createdAt = "2026-08-24T00:00:00.000Z";
  const revision = {
    appId,
    artifactBytes,
    artifactHash: revisionId,
    artifactKey: `apps/${appId}/revisions/${revisionId}/worker.json`,
    createdAt,
    generationModel: "model",
    jobId: "job-one",
    mainModule: "bundle.js",
    ownerId: TENANT_ID,
    policyVersion: 1,
    prompt: "prompt",
    revisionId,
    sourceCommitOid: "b".repeat(40),
    sourceSummary: JSON.stringify({ entryPoint: "src/index.ts", files: [] }),
  };
  return {
    activationHistory: [],
    activeRevisionId: revisionId,
    appId,
    createdAt,
    displayName: "Tiny app",
    liveSlug: "tiny-app",
    ownerId: TENANT_ID,
    revisions: [revision],
    slug: "tiny-app",
    updatedAt: createdAt,
  };
}
