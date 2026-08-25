import { beforeAll, describe, expect, it, vi } from "vitest";

import {
  FRAME_SESSION_AUDIENCE,
  type FrameSessionClaims,
  issueLaunchTicket,
  verifyLaunchTicket,
} from "../src/auth";
import { buildProject, serializeArtifact } from "../src/builder";
import type { AppAccess, Env } from "../src/index";

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
let controlWorker: typeof import("../src/index").default;

beforeAll(async () => {
  ({ AppPlatform, RuntimePlatform, default: controlWorker } = await import("../src/index"));
});

const USER_ID = "0198e2c4-365e-7a66-a58f-d4e5b46a7dad";
const SECOND_USER_ID = "0198e2c4-365e-7a66-a58f-d4e5b46a7dac";
const TENANT_ID = `user:${USER_ID}` as const;
const PERSONAL_ACCESS = Object.freeze({
  actorUserId: USER_ID,
  tenantId: TENANT_ID,
  kind: "personal",
  role: "owner",
}) satisfies AppAccess;
const TEAM_ID = "a".repeat(64);
const TEAM_TENANT = `team:${TEAM_ID}` as const;
const TEAM_OWNER_ACCESS = Object.freeze({
  actorUserId: USER_ID,
  tenantId: TEAM_TENANT,
  kind: "team",
  role: "owner",
}) satisfies AppAccess;
const TEAM_MEMBER_ACCESS = Object.freeze({ ...TEAM_OWNER_ACCESS, role: "member" }) satisfies AppAccess;

function teamAuthorization(role: "owner" | "member" = "owner", userId = USER_ID) {
  return {
    authorized: true as const,
    team: { id: TEAM_ID, name: "Builders", created_at: 1 },
    membership: { user_id: userId, role, joined_at: 2 },
  };
}
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
    NANOCODEX_AGENTS: {
      authorizeTeam: vi.fn(async () => ({ authorized: false })),
      createAgent: vi.fn(),
    } as unknown as Env["NANOCODEX_AGENTS"],
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
  it("serves console assets through the default service-binding entrypoint", async () => {
    const env = configuredEnv();
    const response = await controlWorker.fetch!(
      new Request("https://assets.local/index.html"),
      env,
    );
    expect(response.status).toBe(200);
    expect(await response.text()).toBe("console");
    expect(env.ASSETS.fetch).toHaveBeenCalledWith(expect.objectContaining({ method: "GET" }));
  });

  it("derives the personal tenant from managed identity and starts a durable build", async () => {
    const env = configuredEnv();
    const gateway = appGateway(env);
    const denied = await gateway.request(
      "forged" as unknown as AppAccess,
      new Request("https://nanocodex.test/apps/api/apps?workspace=personal"),
    );
    expect(denied.status).toBe(401);

    const unapproved = await gateway.request(PERSONAL_ACCESS, new Request("https://nanocodex.test/apps/api/builds?workspace=personal", {
      body: JSON.stringify({ prompt: "Build without reviewing app powers" }),
      headers: { "content-type": "application/json", origin: "https://nanocodex.test" },
      method: "POST",
    }));
    expect(unapproved.status).toBe(400);
    expect(await unapproved.json()).toMatchObject({ error: "capability_approval_required" });

    const response = await gateway.request(PERSONAL_ACCESS, new Request("https://nanocodex.test/apps/api/builds?workspace=personal", {
      body: JSON.stringify({
        grants: ["profile:read", "state:read", "state:write", "ai:generate", "agents:run"],
        prompt: "Build me a tiny issue tracker",
      }),
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

    const crossOrigin = await gateway.request(PERSONAL_ACCESS, new Request("https://nanocodex.test/apps/api/builds?workspace=personal", {
      body: JSON.stringify({ prompt: "attack" }),
      headers: { origin: "https://attacker.test" },
      method: "POST",
    }));
    expect(crossOrigin.status).toBe(403);
    expect(env.NANOCODEX_AGENTS.authorizeTeam).not.toHaveBeenCalled();
  });

  it("allows team members to read and launch while reserving builds and activation for owners", async () => {
    const app = appRecord(undefined, undefined, TEAM_TENANT);
    const job = {
      appId: app.appId,
      baseRevisionId: null,
      completedAt: null,
      createdAt: app.createdAt,
      error: null,
      jobId: "team-job",
      ownerId: TEAM_TENANT,
      prompt: "team prompt",
      revisionId: null,
      status: "building",
      targetAppId: app.appId,
      updateAppId: null,
    };
    const registry = {
      getApp: vi.fn(async () => app),
      getJob: vi.fn(async () => job),
      listApps: vi.fn(async () => [app]),
      startJob: vi.fn(async () => job),
    };
    const env = configuredEnv({
      APP_REGISTRY: {
        get: vi.fn(() => registry),
        idFromName: vi.fn((tenant: string) => tenant),
      } as unknown as Env["APP_REGISTRY"],
    });
    const member = appGateway(env);
    const workspace = `workspace=${TEAM_TENANT}`;

    const listed = await member.request(
      TEAM_MEMBER_ACCESS,
      new Request(`https://nanocodex.test/apps/api/apps?${workspace}`),
    );
    expect(listed.status).toBe(200);
    expect(await listed.json()).toMatchObject({
      tenant: { id: TEAM_TENANT, kind: "team", role: "member" },
    });
    expect((await member.request(
      TEAM_MEMBER_ACCESS,
      new Request(`https://nanocodex.test/apps/api/apps/${app.appId}?${workspace}`),
    )).status).toBe(200);
    expect((await member.request(
      TEAM_MEMBER_ACCESS,
      new Request(`https://nanocodex.test/apps/api/builds/team-job?${workspace}`),
    )).status).toBe(200);
    const launch = await member.request(TEAM_MEMBER_ACCESS, new Request(
      `https://nanocodex.test/apps/api/apps/${app.appId}/launch?${workspace}`,
      { method: "POST", headers: { origin: "https://nanocodex.test" } },
    ));
    expect(launch.status).toBe(303);
    expect(new URL(launch.headers.get("location")!).searchParams.get("workspace")).toBe(TEAM_TENANT);

    const memberBuild = await member.request(TEAM_MEMBER_ACCESS, new Request(
      `https://nanocodex.test/apps/api/builds?${workspace}`,
      { method: "POST", headers: { origin: "https://nanocodex.test" } },
    ));
    expect(memberBuild.status).toBe(403);
    const memberActivate = await member.request(TEAM_MEMBER_ACCESS, new Request(
      `https://nanocodex.test/apps/api/apps/${app.appId}/activate?${workspace}`,
      { method: "POST", headers: { origin: "https://nanocodex.test" } },
    ));
    expect(memberActivate.status).toBe(403);

    const ownerBuild = await member.request(TEAM_OWNER_ACCESS, new Request(
      `https://nanocodex.test/apps/api/builds?${workspace}`,
      {
        body: JSON.stringify({
          grants: ["profile:read", "state:read", "state:write", "ai:generate", "agents:run"],
          prompt: "owner build",
        }),
        method: "POST",
        headers: { origin: "https://nanocodex.test" },
      },
    ));
    expect(ownerBuild.status).toBe(202);
  });

  it("completes an account-authorized launch only after binding a runtime browser transaction", async () => {
    const app = appRecord();
    const registry = { getApp: vi.fn(async () => app) };
    const ticketStore = {
      consume: vi.fn(async () => true),
      issue: vi.fn(async () => undefined),
    };
    const env = configuredEnv({
      APP_LAUNCH_TICKETS: { getByName: vi.fn(() => ticketStore) } as unknown as Env["APP_LAUNCH_TICKETS"],
      APP_REGISTRY: {
        get: vi.fn(() => registry),
        idFromName: vi.fn((tenant: string) => tenant),
      } as unknown as Env["APP_REGISTRY"],
    });
    const response = await appGateway(env).request(PERSONAL_ACCESS, new Request(
      `https://nanocodex.test/apps/api/apps/${app.appId}/launch?workspace=personal`,
      { method: "POST", headers: { accept: "application/json", origin: "https://nanocodex.test" } },
    ));
    expect(response.status).toBe(200);
    const location = new URL((await response.json<{ launch_url: string }>()).launch_url);
    expect(location.origin).toBe("https://runtime.example.test");
    expect(location.pathname).toBe("/__auth/begin");
    const intent = location.searchParams.get("intent")!;
    expect(intent).toMatch(/^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]{43}$/);
    expect(ticketStore.issue).toHaveBeenCalledOnce();

    const transaction = "transaction-nonce-12345678";
    const completion = await appGateway(env).completeLaunch(new Request(
      `https://nanocodex.test/apps/api/launch/complete?intent=${intent}&transaction=${transaction}&workspace=personal`,
    ));
    expect(completion.status).toBe(303);
    const launch = new URL(completion.headers.get("location")!);
    expect(launch.origin).toBe("https://runtime.example.test");
    expect(launch.pathname).toBe("/__auth/launch");
    const ticket = launch.searchParams.get("ticket")!;
    expect(await verifyLaunchTicket(ticket, TICKET_SECRET)).toMatchObject({
      actorUserId: USER_ID,
      appId: app.appId,
      tenantId: TENANT_ID,
      transaction,
    });
    expect(ticketStore.consume).toHaveBeenCalledOnce();
    expect(ticketStore.issue).toHaveBeenCalledTimes(2);
    expect(env.NANOCODEX_AGENTS.authorizeTeam).not.toHaveBeenCalled();
  });

  it("reauthorizes team launch completion and rejects selector tampering or membership loss", async () => {
    const app = appRecord(undefined, undefined, TEAM_TENANT);
    const registry = { getApp: vi.fn(async () => app) };
    const ticketStore = {
      consume: vi.fn(async () => true),
      issue: vi.fn(async () => undefined),
    };
    const authorizeTeam = vi.fn<Env["NANOCODEX_AGENTS"]["authorizeTeam"]>(
      async () => teamAuthorization(),
    );
    const env = configuredEnv({
      APP_LAUNCH_TICKETS: { getByName: vi.fn(() => ticketStore) } as unknown as Env["APP_LAUNCH_TICKETS"],
      APP_REGISTRY: {
        get: vi.fn(() => registry),
        idFromName: vi.fn((tenant: string) => tenant),
      } as unknown as Env["APP_REGISTRY"],
      NANOCODEX_AGENTS: { authorizeTeam, createAgent: vi.fn() } as unknown as Env["NANOCODEX_AGENTS"],
    });
    const gateway = appGateway(env);
    const started = await gateway.request(TEAM_OWNER_ACCESS, new Request(
      `https://nanocodex.test/apps/api/apps/${app.appId}/launch?workspace=${TEAM_TENANT}`,
      { method: "POST", headers: { origin: "https://nanocodex.test" } },
    ));
    const intent = new URL(started.headers.get("location")!).searchParams.get("intent")!;
    const transaction = "transaction-nonce-12345678";

    const tampered = await gateway.completeLaunch(new Request(
      `https://nanocodex.test/apps/api/launch/complete?intent=${intent}&transaction=${transaction}&workspace=personal`,
    ));
    expect(tampered.status).toBe(401);
    expect(ticketStore.consume).not.toHaveBeenCalled();

    authorizeTeam.mockResolvedValueOnce({ authorized: false });
    const removed = await gateway.completeLaunch(new Request(
      `https://nanocodex.test/apps/api/launch/complete?intent=${intent}&transaction=${transaction}&workspace=${TEAM_TENANT}`,
    ));
    expect(removed.status).toBe(404);
    expect(ticketStore.consume).not.toHaveBeenCalled();

    authorizeTeam.mockRejectedValueOnce(new Error("authority unavailable"));
    const unavailable = await gateway.completeLaunch(new Request(
      `https://nanocodex.test/apps/api/launch/complete?intent=${intent}&transaction=${transaction}&workspace=${TEAM_TENANT}`,
    ));
    expect(unavailable.status).toBe(404);
    expect(ticketStore.consume).not.toHaveBeenCalled();
  });

  it("reauthorizes team tickets before consumption and fails closed on removal or authority failure", async () => {
    const identity = {
      actorUserId: USER_ID,
      appId: appRecord().appId,
      slug: "tiny-app",
      tenantId: TEAM_TENANT,
    };
    const transaction = "transaction-nonce-12345678";
    const ticket = await issueLaunchTicket(TICKET_SECRET, identity, transaction);
    const ticketStore = { consume: vi.fn(async () => true), issue: vi.fn() };
    const authorizeTeam = vi.fn<Env["NANOCODEX_AGENTS"]["authorizeTeam"]>(
      async () => teamAuthorization("member"),
    );
    const env = configuredEnv({
      APP_LAUNCH_TICKETS: { getByName: vi.fn(() => ticketStore) } as unknown as Env["APP_LAUNCH_TICKETS"],
      NANOCODEX_AGENTS: { authorizeTeam, createAgent: vi.fn() } as unknown as Env["NANOCODEX_AGENTS"],
    });
    expect(await runtimeGateway(env).redeemLaunchTicket(ticket, transaction)).toMatchObject(identity);
    expect(ticketStore.consume).toHaveBeenCalledOnce();

    ticketStore.consume.mockClear();
    authorizeTeam.mockResolvedValueOnce({ authorized: false });
    expect(await runtimeGateway(env).redeemLaunchTicket(ticket, transaction)).toBeNull();
    expect(ticketStore.consume).not.toHaveBeenCalled();

    authorizeTeam.mockRejectedValueOnce(new Error("authority unavailable"));
    expect(await runtimeGateway(env).redeemLaunchTicket(ticket, transaction)).toBeNull();
    expect(ticketStore.consume).not.toHaveBeenCalled();
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
    const claims = frameClaims(app.appId, app.slug);
    const publicPrefix = `/__frame/${"a".repeat(24)}.${"b".repeat(43)}/a/${app.appId}/${app.slug}`;
    const response = await runtimeGateway(env, {
      exports: { NanocodexCapability: () => capability },
    }).invokeApp(claims, new Request(
      `https://runtime.example.test/a/${app.appId}/tiny-app/api/items?q=one`,
      { headers: { authorization: "Bearer forged", cookie: "private=1" } },
    ), publicPrefix);

    expect(response.status).toBe(302);
    expect(response.headers.get("location")).toBe(`${publicPrefix}/next`);
    expect(response.headers.get("set-cookie")).toBeNull();
    expect(loader.get).toHaveBeenCalledWith(
      `${TENANT_ID}:${USER_ID}:${app.appId}:${artifact.revision}:1`,
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
    expect(invoked?.headers.get("x-forwarded-prefix")).toBe(publicPrefix);
    expect(env.NANOCODEX_AGENTS.authorizeTeam).not.toHaveBeenCalled();
  });

  it("revokes an open team runtime session on the next invoke and conceals authority failure", async () => {
    const artifact = await buildProject({
      entryPoint: "src/index.ts",
      files: [{ path: "src/index.ts", content: "export default {fetch(){return new Response('ok')}}" }],
      name: "Team app",
      slug: "tiny-app",
    }, async () => ({
      mainModule: "bundle.js",
      modules: { "bundle.js": "export default {fetch(){return new Response('ok')}}" },
    }));
    const app = appRecord(artifact.revision, serializeArtifact(artifact).length, TEAM_TENANT);
    const registry = { getApp: vi.fn(async () => app) };
    const authorizeTeam = vi.fn<Env["NANOCODEX_AGENTS"]["authorizeTeam"]>(
      async (actorUserId) => teamAuthorization("member", actorUserId),
    );
    const loader = {
      get: vi.fn(() => ({ getEntrypoint: () => ({ fetch: async () => new Response("team ok") }) })),
    };
    const env = configuredEnv({
      APP_ARTIFACTS: {
        get: vi.fn(async () => ({ text: async () => serializeArtifact(artifact) })),
      } as unknown as R2Bucket,
      APP_REGISTRY: {
        get: vi.fn(() => registry),
        idFromName: vi.fn((tenant: string) => tenant),
      } as unknown as Env["APP_REGISTRY"],
      LOADER: loader as unknown as WorkerLoader,
      NANOCODEX_AGENTS: { authorizeTeam, createAgent: vi.fn() } as unknown as Env["NANOCODEX_AGENTS"],
    });
    const claims = frameClaims(app.appId, app.slug, TEAM_TENANT);
    const prefix = `/__frame/${"a".repeat(24)}.${"b".repeat(43)}/a/${app.appId}/${app.slug}`;
    const request = () => new Request(`https://runtime.example.test/a/${app.appId}/${app.slug}/`);
    const gateway = runtimeGateway(env, { exports: { NanocodexCapability: () => ({}) } });

    const allowed = await gateway.invokeApp(claims, request(), prefix);
    expect(allowed.status).toBe(200);
    expect(await allowed.text()).toBe("team ok");
    expect(loader.get).toHaveBeenCalledOnce();
    expect(loader.get).toHaveBeenNthCalledWith(
      1,
      `${TEAM_TENANT}:${USER_ID}:${app.appId}:${artifact.revision}:1`,
      expect.any(Function),
    );

    const secondMember = await gateway.invokeApp(
      frameClaims(app.appId, app.slug, TEAM_TENANT, SECOND_USER_ID),
      request(),
      prefix,
    );
    expect(secondMember.status).toBe(200);
    expect(loader.get).toHaveBeenNthCalledWith(
      2,
      `${TEAM_TENANT}:${SECOND_USER_ID}:${app.appId}:${artifact.revision}:1`,
      expect.any(Function),
    );

    authorizeTeam.mockResolvedValueOnce({ authorized: false });
    const removed = await gateway.invokeApp(claims, request(), prefix);
    expect(removed.status).toBe(404);
    expect(await removed.json()).toEqual({ error: "not_found" });
    expect(loader.get).toHaveBeenCalledTimes(2);

    authorizeTeam.mockRejectedValueOnce(new Error("authority unavailable"));
    const unavailable = await gateway.invokeApp(claims, request(), prefix);
    expect(unavailable.status).toBe(404);
    expect(loader.get).toHaveBeenCalledTimes(2);
  });

  it("rejects a self-validating artifact that does not match the registry revision", async () => {
    const artifact = await buildProject({
      entryPoint: "src/index.ts",
      files: [{ path: "src/index.ts", content: "export default {fetch(){return new Response('ok')}}" }],
      name: "Tiny app",
      slug: "tiny-app",
    }, async () => ({
      mainModule: "bundle.js",
      modules: { "bundle.js": "export default {fetch(){return new Response('ok')}}" },
    }));
    const encoded = serializeArtifact(artifact);
    const app = appRecord("c".repeat(64), new TextEncoder().encode(encoded).byteLength);
    const registry = { getApp: vi.fn(async () => app) };
    const loader = { get: vi.fn() };
    const env = configuredEnv({
      APP_ARTIFACTS: { get: vi.fn(async () => ({ text: async () => encoded })) } as unknown as R2Bucket,
      APP_REGISTRY: {
        get: vi.fn(() => registry),
        idFromName: vi.fn((tenant: string) => tenant),
      } as unknown as Env["APP_REGISTRY"],
      LOADER: loader as unknown as WorkerLoader,
    });

    const response = await runtimeGateway(env).invokeApp(
      frameClaims(app.appId, app.slug),
      new Request(`https://runtime.example.test/a/${app.appId}/tiny-app/`),
      `/__frame/${"a".repeat(24)}.${"b".repeat(43)}/a/${app.appId}/tiny-app`,
    );

    expect(response.status).toBe(503);
    expect(await response.json()).toEqual({ error: "app_artifact_unavailable" });
    expect(loader.get).not.toHaveBeenCalled();
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
      frameClaims("0198e2c4-365e-7a66-a58f-d4e5b46a7dae", "tiny-app"),
      new Request("https://runtime.example.test/a/0198e2c4-365e-7a66-a58f-d4e5b46a7dae/tiny-app/"),
      `/__frame/${"a".repeat(24)}.${"b".repeat(43)}/a/0198e2c4-365e-7a66-a58f-d4e5b46a7dae/tiny-app`,
    );
    expect(response.status).toBe(404);
  });
});

function frameClaims(
  appId: string,
  slug: string,
  tenantId: string = TENANT_ID,
  actorUserId = USER_ID,
): FrameSessionClaims {
  return {
    actorUserId,
    appId,
    audience: FRAME_SESSION_AUDIENCE,
    expiry: Math.floor(Date.now() / 1_000) + 3_600,
    nonce: "abcdefghijklmnopqrstuvwxyz",
    slug,
    tenantId,
    transaction: "transaction-nonce-12345678",
    version: 1,
  };
}

function appRecord(
  revisionId = "a".repeat(64),
  artifactBytes = 123,
  ownerId: string = TENANT_ID,
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
    ownerId,
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
    ownerId,
    revisions: [revision],
    slug: "tiny-app",
    updatedAt: createdAt,
  };
}
