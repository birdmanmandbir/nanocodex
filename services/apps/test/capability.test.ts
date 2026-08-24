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
let AppState: typeof import("../src/capability").AppState;

beforeAll(async () => {
  ({ AppState, NanocodexCapability } = await import("../src/capability"));
});

const USER = "0198e2c4-365e-7a66-a58f-d4e5b46a7dad";
const MANAGED_AGENT = "0198e2c4-365e-7a66-a58f-d4e5b46a7dae";
const HANDLE = "123e4567-e89b-42d3-a456-426614174000";

function capability(overrides: {
  agents?: Record<string, unknown>;
  state?: Record<string, unknown>;
} = {}) {
  const state = {
    fetch: vi.fn(),
    registerAgent: vi.fn(async () => HANDLE),
    releaseAgent: vi.fn(async () => undefined),
    reserveAgent: vi.fn(async () => HANDLE),
    resolveAgent: vi.fn(async (_user: string, handle: string) => handle === HANDLE ? MANAGED_AGENT : null),
    ...overrides.state,
  };
  const agents = {
    createAgent: vi.fn(async () => ({ status: 201, body: { agent_id: MANAGED_AGENT } })),
    getTurnStatus: vi.fn(async () => ({ status: 200, body: { id: "turn-1", state: "completed" } })),
    submitTurn: vi.fn(async () => ({ status: 202, body: { id: "turn-1", state: "accepted" } })),
    ...overrides.agents,
  };
  const env = {
    AI: { run: vi.fn() },
    APP_STATE: { idFromName: vi.fn(() => "state-id"), get: vi.fn(() => state) },
    NANOCODEX_AGENTS: agents,
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

function appState() {
  const values = new Map<string, unknown>();
  const operations = {
    delete: vi.fn(async (key: string) => values.delete(key)),
    get: vi.fn(async (key: string) => values.get(key)),
    list: vi.fn(async (options: { limit?: number; prefix?: string } = {}) => {
      const entries = [...values]
        .filter(([key]) => key.startsWith(options.prefix ?? ""))
        .slice(0, options.limit);
      return new Map(entries);
    }),
    put: vi.fn(async (key: string, value: unknown) => { values.set(key, value); }),
  };
  let transactionTail = Promise.resolve();
  const transaction = vi.fn(async <T>(callback: (storage: typeof operations) => Promise<T>) => {
    const previous = transactionTail;
    let release: () => void = () => {};
    transactionTail = new Promise<void>((resolve) => { release = resolve; });
    await previous;
    try {
      return await callback(operations);
    } finally {
      release();
    }
  });
  const instance = new AppState({
    storage: { ...operations, transaction },
  } as unknown as DurableObjectState, {} as never);
  return { instance, values };
}

function streamedRequest(url: string, chunks: Uint8Array[], cancel: () => void, headers?: HeadersInit) {
  let index = 0;
  const body = new ReadableStream<Uint8Array>({
    cancel,
    pull(controller) {
      const chunk = chunks[index++];
      if (chunk) controller.enqueue(chunk);
    },
  });
  return new Request(url, {
    body,
    headers,
    method: "POST",
    duplex: "half",
  } as RequestInit & { duplex: "half" });
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
    expect(state.reserveAgent).toHaveBeenCalledWith(USER);
    expect(state.registerAgent).toHaveBeenCalledWith(USER, HANDLE, MANAGED_AGENT);
    expect(instance.context()).not.toHaveProperty("actorUserId");
    expect(instance.context()).not.toHaveProperty("tenantId");
  });

  it("atomically caps concurrent reservations and keeps registration idempotent", async () => {
    const { instance } = appState();
    const reservations = await Promise.allSettled(
      Array.from({ length: 64 }, () => instance.reserveAgent(USER)),
    );
    const handles = reservations.flatMap((result) => result.status === "fulfilled" ? [result.value] : []);
    expect(handles).toHaveLength(32);
    expect(reservations.filter((result) => result.status === "rejected")).toHaveLength(32);

    const handle = handles[0]!;
    await expect(instance.registerAgent(USER, handle, MANAGED_AGENT)).resolves.toBe(handle);
    await expect(instance.registerAgent(USER, handle, MANAGED_AGENT)).resolves.toBe(handle);
    await expect(instance.resolveAgent(USER, handle)).resolves.toBe(MANAGED_AGENT);
    await instance.releaseAgent(USER, handle);
    await expect(instance.resolveAgent(USER, handle)).resolves.toBe(MANAGED_AGENT);
  });

  it("enforces a full reservation cap before provisioning upstream", async () => {
    const { agents, instance } = capability({
      state: { reserveAgent: vi.fn(async () => { throw new Error("app agent limit reached"); }) },
    });
    const response = await instance.fetch(new Request("https://agents.internal/v1/agents", { method: "POST" }));
    expect(response.status).toBe(409);
    expect(await response.json()).toEqual({ error: "app_agent_limit" });
    expect(agents.createAgent).not.toHaveBeenCalled();
  });

  it("releases failed creates but retains ambiguous successful registrations", async () => {
    const failed = capability({
      agents: { createAgent: vi.fn(async () => ({ status: 503, body: { error: "unavailable" } })) },
    });
    const response = await failed.instance.fetch(
      new Request("https://agents.internal/v1/agents", { method: "POST" }),
    );
    expect(response.status).toBe(503);
    expect(failed.state.releaseAgent).toHaveBeenCalledWith(USER, HANDLE);

    const ambiguous = capability({
      state: { registerAgent: vi.fn(async () => { throw new Error("storage unavailable"); }) },
    });
    await expect(ambiguous.instance.fetch(
      new Request("https://agents.internal/v1/agents", { method: "POST" }),
    )).rejects.toThrow("storage unavailable");
    expect(ambiguous.agents.createAgent).toHaveBeenCalledOnce();
    expect(ambiguous.state.releaseAgent).not.toHaveBeenCalled();
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

  it("rejects and cancels every create-agent request body", async () => {
    const cancelled = vi.fn();
    const { agents, instance, state } = capability();
    const response = await instance.fetch(streamedRequest(
      "https://agents.internal/v1/agents",
      [new Uint8Array()],
      cancelled,
    ));
    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ error: "invalid_request" });
    expect(cancelled).toHaveBeenCalledOnce();
    expect(state.reserveAgent).not.toHaveBeenCalled();
    expect(agents.createAgent).not.toHaveBeenCalled();
  });

  it("accepts exactly 64 KiB turn bodies and cancels byte overflow", async () => {
    const { agents, instance } = capability();
    const prefix = JSON.stringify({ id: "turn-1", input: "" }).slice(0, -2);
    const exact = `${prefix}${"a".repeat(64 * 1024 - prefix.length - 2)}"}`;
    expect(new TextEncoder().encode(exact)).toHaveLength(64 * 1024);
    const accepted = await instance.fetch(new Request(
      `https://agents.internal/v1/agents/${HANDLE}/turns`,
      { method: "POST", body: exact },
    ));
    expect(accepted.status).toBe(202);
    expect(agents.submitTurn).toHaveBeenCalledOnce();

    const cancelled = vi.fn();
    const overflow = await instance.fetch(streamedRequest(
      `https://agents.internal/v1/agents/${HANDLE}/turns`,
      [new Uint8Array(64 * 1024), new Uint8Array([1])],
      cancelled,
    ));
    expect(overflow.status).toBe(413);
    expect(await overflow.json()).toEqual({ error: "request_too_large" });
    expect(cancelled).toHaveBeenCalledOnce();
    expect(agents.submitTurn).toHaveBeenCalledOnce();
  });

  it("measures streamed turn limits in bytes instead of UTF-16 characters", async () => {
    const cancelled = vi.fn();
    const { agents, instance } = capability();
    const body = new TextEncoder().encode(JSON.stringify({
      id: "turn-1",
      input: "😀".repeat(20_000),
    }));
    expect(body.byteLength).toBeGreaterThan(64 * 1024);
    expect(new TextDecoder().decode(body).length).toBeLessThan(64 * 1024);
    const response = await instance.fetch(streamedRequest(
      `https://agents.internal/v1/agents/${HANDLE}/turns`,
      [body],
      cancelled,
    ));
    expect(response.status).toBe(413);
    expect(cancelled).toHaveBeenCalledOnce();
    expect(agents.submitTurn).not.toHaveBeenCalled();
  });
});
