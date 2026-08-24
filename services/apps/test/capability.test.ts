import { beforeAll, describe, expect, it, vi } from "vitest";

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
}));

let NanocodexCapability: typeof import("../src/capability").NanocodexCapability;

beforeAll(async () => {
  ({ NanocodexCapability } = await import("../src/capability"));
});

const USER = "0198e2c4-365e-7a66-a58f-d4e5b46a7dad";
const MANAGED_AGENT = "0198e2c4-365e-7a66-a58f-d4e5b46a7dae";
const HANDLE = "123e4567-e89b-42d3-a456-426614174000";

function capability(overrides: Record<string, unknown> = {}) {
  const state = {
    fetch: vi.fn(),
    registerAgent: vi.fn(async () => HANDLE),
    resolveAgent: vi.fn(async (_user: string, handle: string) => handle === HANDLE ? MANAGED_AGENT : null),
  };
  const agents = {
    createAgent: vi.fn(async () => ({ status: 201, body: { agent_id: MANAGED_AGENT } })),
    getTurnStatus: vi.fn(async () => ({ status: 200, body: { id: "turn-1", state: "completed" } })),
    submitTurn: vi.fn(async () => ({ status: 202, body: { id: "turn-1", state: "accepted" } })),
  };
  const env = {
    AI: { run: vi.fn() },
    APP_STATE: { idFromName: vi.fn(() => "state-id"), get: vi.fn(() => state) },
    NANOCODEX_AGENTS: agents,
    ...overrides,
  };
  const instance = new NanocodexCapability({
    props: {
      actorUserId: USER,
      appId: "0198e2c4-365e-7a66-a58f-d4e5b46a7daf",
      basePath: "/a/private-app/",
      displayName: "Private app",
      grants: ["agents:run"],
      revision: "a".repeat(64),
      tenantId: `user:${USER}`,
    },
  } as unknown as ExecutionContext, env as never);
  return { agents, instance, state };
}

describe("tenant-scoped Nanocodex agent binding", () => {
  it("creates an app-scoped handle without exposing managed credentials or identity", async () => {
    const { agents, instance, state } = capability();
    const response = await instance.fetch(new Request("https://agents.internal/v1/agents", { method: "POST" }));
    expect(response.status).toBe(201);
    expect(await response.json()).toEqual({ agent_id: HANDLE, session_id: HANDLE });
    expect(agents.createAgent).toHaveBeenCalledWith(USER, {
      publicOrigin: "https://apps.nanocodex.internal",
    });
    expect(state.registerAgent).toHaveBeenCalledWith(USER, MANAGED_AGENT);
    expect(instance.context()).not.toHaveProperty("actorUserId");
    expect(instance.context()).not.toHaveProperty("tenantId");
  });

  it("translates only an app-owned handle for turn submission and status", async () => {
    const { agents, instance } = capability();
    const submitted = await instance.fetch(new Request(
      `https://agents.internal/v1/agents/${HANDLE}/turns`,
      {
        method: "POST",
        headers: { "content-type": "application/json", "idempotency-key": "stable-1" },
        body: JSON.stringify({ id: "turn-1", input: "Inspect the workspace" }),
      },
    ));
    expect(submitted.status).toBe(202);
    expect(agents.submitTurn).toHaveBeenCalledWith(USER, MANAGED_AGENT, expect.objectContaining({
      id: "turn-1",
      idempotencyKey: "stable-1",
      input: "Inspect the workspace",
    }));

    const status = await instance.fetch(new Request(
      `https://agents.internal/v1/agents/${HANDLE}/turns/turn-1`,
    ));
    expect(status.status).toBe(200);
    expect(agents.getTurnStatus).toHaveBeenCalledWith(
      USER,
      MANAGED_AGENT,
      "turn-1",
      { publicOrigin: "https://apps.nanocodex.internal" },
    );
  });

  it("fails closed for credentials and handles outside the app mapping", async () => {
    const { instance } = capability();
    const credential = await instance.fetch(new Request("https://agents.internal/v1/agents", {
      method: "POST",
      headers: { authorization: "Bearer ncx_live_secret" },
    }));
    expect(credential.status).toBe(400);

    const missing = await instance.fetch(new Request(
      "https://agents.internal/v1/agents/00000000-0000-4000-8000-000000000000/turns/turn-1",
    ));
    expect(missing.status).toBe(404);
  });
});
