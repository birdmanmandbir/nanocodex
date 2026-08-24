import { DurableObject, WorkerEntrypoint } from "cloudflare:workers";

const MAX_KEY_CHARS = 128;
const MAX_VALUE_BYTES = 32 * 1024;
const MAX_KEYS = 256;
const MAX_AI_PROMPT_CHARS = 4 * 1024;
const MAX_AI_OUTPUT_CHARS = 8 * 1024;
const MAX_AGENT_REQUEST_BYTES = 64 * 1024;
const MAX_APP_AGENTS = 32;
const KEY = /^[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}$/;
const USER_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const AGENT_HANDLE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const TURN_ID = /^[A-Za-z0-9._:-]{1,128}$/;

export type CapabilityProps = Readonly<{
  appId: string;
  actorUserId: string;
  basePath: string;
  displayName: string;
  grants: readonly string[];
  revision: string;
  tenantId: string;
}>;

export type ManagedAgentRpcResult = Readonly<{
  status: number;
  body: Record<string, unknown>;
}>;

export interface ManagedAgentService {
  authorizeTeam(actorUserId: string, teamId: string): Promise<Readonly<
    | { authorized: false }
    | {
      authorized: true;
      team: Readonly<{ id: string; name: string; created_at: number }>;
      membership: Readonly<{ user_id: string; role: "owner" | "member"; joined_at: number }>;
    }
  >>;
  createAgent(userId: string, input: { publicOrigin: string }): Promise<ManagedAgentRpcResult>;
  submitTurn(userId: string, agentId: string, input: {
    id: string;
    input: unknown;
    idempotencyKey?: string;
    publicOrigin: string;
  }): Promise<ManagedAgentRpcResult>;
  getTurnStatus(userId: string, agentId: string, turnId: string, input: {
    publicOrigin: string;
  }): Promise<ManagedAgentRpcResult>;
}

export interface CapabilityEnv {
  AI: Ai;
  NANOCODEX_AGENTS: ManagedAgentService;
  APP_STATE: DurableObjectNamespace<AppState>;
}

type StoredValue = Readonly<{ value: unknown }>;
type StoredAgent = Readonly<{ actorUserId: string; managedAgentId: string | null }>;

export class AppState extends DurableObject<CapabilityEnv> {
  async reserveAgent(actorUserId: string): Promise<string> {
    if (!USER_ID.test(actorUserId)) throw new Error("invalid agent identity");
    const handle = crypto.randomUUID();
    await this.ctx.storage.transaction(async (storage) => {
      const existing = await storage.list({ prefix: "agent:", limit: MAX_APP_AGENTS });
      if (existing.size >= MAX_APP_AGENTS) throw new Error("app agent limit reached");
      await storage.put(
        `agent:${handle}`,
        { actorUserId, managedAgentId: null } satisfies StoredAgent,
      );
    });
    return handle;
  }

  async registerAgent(actorUserId: string, handle: string, managedAgentId: string): Promise<string> {
    if (!USER_ID.test(actorUserId) || !AGENT_HANDLE.test(handle) || !USER_ID.test(managedAgentId)) {
      throw new Error("invalid agent identity");
    }
    await this.ctx.storage.transaction(async (storage) => {
      const key = `agent:${handle}`;
      const reserved = await storage.get<StoredAgent>(key);
      if (!reserved || reserved.actorUserId !== actorUserId) throw new Error("agent reservation not found");
      if (reserved.managedAgentId !== null && reserved.managedAgentId !== managedAgentId) {
        throw new Error("agent reservation already registered");
      }
      if (reserved.managedAgentId === null) {
        await storage.put(key, { actorUserId, managedAgentId } satisfies StoredAgent);
      }
    });
    return handle;
  }

  async releaseAgent(actorUserId: string, handle: string): Promise<void> {
    if (!USER_ID.test(actorUserId) || !AGENT_HANDLE.test(handle)) return;
    await this.ctx.storage.transaction(async (storage) => {
      const key = `agent:${handle}`;
      const reserved = await storage.get<StoredAgent>(key);
      if (reserved?.actorUserId === actorUserId && reserved.managedAgentId === null) {
        await storage.delete(key);
      }
    });
  }

  async resolveAgent(actorUserId: string, handle: string): Promise<string | null> {
    if (!USER_ID.test(actorUserId) || !AGENT_HANDLE.test(handle)) return null;
    const stored = await this.ctx.storage.get<StoredAgent>(`agent:${handle}`);
    return stored?.actorUserId === actorUserId ? stored.managedAgentId : null;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/counter") return this.#counter(request);
    if (url.pathname === "/kv") return this.#list(request, url);
    if (url.pathname.startsWith("/kv/")) {
      const key = decodeURIComponent(url.pathname.slice("/kv/".length));
      return this.#value(request, key);
    }
    return Response.json({ error: "not_found" }, { status: 404 });
  }

  async #counter(request: Request): Promise<Response> {
    if (request.method === "GET") {
      return Response.json({ value: await this.ctx.storage.get<number>("counter") ?? 0 });
    }
    if (request.method !== "POST") return methodNotAllowed();
    const value = await this.ctx.storage.transaction(async (storage) => {
      const next = (await storage.get<number>("counter") ?? 0) + 1;
      await storage.put("counter", next);
      return next;
    });
    return Response.json({ value });
  }

  async #list(request: Request, url: URL): Promise<Response> {
    if (request.method !== "GET") return methodNotAllowed();
    const prefix = url.searchParams.get("prefix") ?? "";
    if (prefix.length > MAX_KEY_CHARS || (prefix && !KEY.test(prefix))) {
      return Response.json({ error: "invalid_prefix" }, { status: 400 });
    }
    const entries = await this.ctx.storage.list<StoredValue>({
      limit: 100,
      prefix: `kv:${prefix}`,
    });
    return Response.json({
      entries: [...entries].map(([key, stored]) => ({
        key: key.slice("kv:".length),
        value: stored.value,
      })),
    });
  }

  async #value(request: Request, key: string): Promise<Response> {
    if (!validKey(key)) return Response.json({ error: "invalid_key" }, { status: 400 });
    const storageKey = `kv:${key}`;
    if (request.method === "GET") {
      const stored = await this.ctx.storage.get<StoredValue>(storageKey);
      return stored
        ? Response.json({ found: true, value: stored.value })
        : Response.json({ found: false });
    }
    if (request.method === "DELETE") {
      return Response.json({ deleted: await this.ctx.storage.delete(storageKey) });
    }
    if (request.method !== "PUT") return methodNotAllowed();
    const bytes = await readBoundedBody(request, MAX_VALUE_BYTES);
    if (bytes === null) {
      return Response.json({ error: "value_too_large" }, { status: 413 });
    }
    let stored: StoredValue;
    try {
      stored = JSON.parse(new TextDecoder().decode(bytes)) as StoredValue;
    } catch {
      return Response.json({ error: "invalid_json" }, { status: 400 });
    }
    if (!stored || typeof stored !== "object" || !("value" in stored)) {
      return Response.json({ error: "invalid_value" }, { status: 400 });
    }
    const existing = await this.ctx.storage.get(storageKey);
    if (existing === undefined) {
      const entries = await this.ctx.storage.list({ prefix: "kv:", limit: MAX_KEYS });
      if (entries.size >= MAX_KEYS) {
        return Response.json({ error: "key_limit" }, { status: 409 });
      }
    }
    await this.ctx.storage.put(storageKey, stored);
    return Response.json({ stored: true });
  }
}

export class NanocodexCapability extends WorkerEntrypoint<CapabilityEnv, CapabilityProps> {
  context(): Omit<CapabilityProps, "actorUserId" | "grants" | "tenantId"> & { grants: readonly string[] } {
    const { appId, basePath, displayName, grants, revision } = this.ctx.props;
    return { appId, basePath, displayName, grants, revision };
  }

  async get(key: string): Promise<unknown | null> {
    this.#require("state:read");
    const response = await this.#state(`/kv/${encodeURIComponent(assertKey(key))}`, "GET");
    const body = await response.json<{ found: boolean; value?: unknown }>();
    return body.found ? body.value : null;
  }

  async put(key: string, value: unknown): Promise<void> {
    this.#require("state:write");
    const body = JSON.stringify({ value });
    if (new TextEncoder().encode(body).byteLength > MAX_VALUE_BYTES) throw new Error("value too large");
    await this.#state(`/kv/${encodeURIComponent(assertKey(key))}`, "PUT", body);
  }

  async delete(key: string): Promise<boolean> {
    this.#require("state:write");
    const response = await this.#state(`/kv/${encodeURIComponent(assertKey(key))}`, "DELETE");
    return (await response.json<{ deleted: boolean }>()).deleted;
  }

  async list(prefix = ""): Promise<Array<{ key: string; value: unknown }>> {
    this.#require("state:read");
    if (prefix && !validKey(prefix)) throw new Error("invalid prefix");
    const response = await this.#state(`/kv?prefix=${encodeURIComponent(prefix)}`, "GET");
    return (await response.json<{ entries: Array<{ key: string; value: unknown }> }>()).entries;
  }

  async counter(): Promise<number> {
    this.#require("state:read");
    const response = await this.#state("/counter", "GET");
    return (await response.json<{ value: number }>()).value;
  }

  async incrementCounter(): Promise<number> {
    this.#require("state:write");
    const response = await this.#state("/counter", "POST");
    return (await response.json<{ value: number }>()).value;
  }

  async generateText(prompt: string): Promise<string> {
    this.#require("ai:generate");
    if (typeof prompt !== "string" || prompt.length === 0 || prompt.length > MAX_AI_PROMPT_CHARS) {
      throw new Error("invalid prompt");
    }
    const run = this.env.AI.run as unknown as (
      model: string,
      input: Record<string, unknown>,
      options?: Record<string, unknown>,
    ) => Promise<unknown>;
    const result = await run("@cf/zai-org/glm-5.2", {
      max_completion_tokens: 1_200,
      messages: [
        { role: "system", content: "Answer directly and concisely." },
        { role: "user", content: prompt },
      ],
    }, { gateway: { id: "default" }, tags: ["nanocodex-app-runtime"] });
    const text = modelText(result);
    if (!text) throw new Error("model returned no text");
    return text.slice(0, MAX_AI_OUTPUT_CHARS);
  }

  async fetch(request: Request): Promise<Response> {
    this.#require("agents:run");
    const url = new URL(request.url);
    if (url.origin !== "https://agents.internal" || url.search || url.hash) {
      return capabilityJson({ error: "not_found" }, 404);
    }
    if (request.headers.has("authorization") || request.headers.has("cookie")) {
      return capabilityJson({ error: "credentials_not_allowed" }, 400);
    }
    if (url.pathname === "/v1/agents") {
      if (request.method !== "POST") return capabilityJson({ error: "method_not_allowed" }, 405);
      if (request.body) {
        await request.body.cancel().catch(() => undefined);
        return capabilityJson({ error: "invalid_request" }, 400);
      }
      const actor = this.#actor();
      const state = this.#stateStub();
      let handle: string;
      try {
        handle = await state.reserveAgent(actor);
      } catch (error) {
        if (error instanceof Error && error.message === "app agent limit reached") {
          return capabilityJson({ error: "app_agent_limit" }, 409);
        }
        throw error;
      }
      let result: ManagedAgentRpcResult;
      try {
        result = await this.env.NANOCODEX_AGENTS.createAgent(actor, {
          publicOrigin: "https://apps.nanocodex.internal",
        });
      } catch (error) {
        await state.releaseAgent(actor, handle);
        throw error;
      }
      if (result.status !== 201 || typeof result.body.agent_id !== "string") {
        await state.releaseAgent(actor, handle);
        return capabilityJson(result.body, result.status);
      }
      await state.registerAgent(actor, handle, result.body.agent_id);
      return capabilityJson({ agent_id: handle, session_id: handle }, 201);
    }

    const turn = url.pathname.match(/^\/v1\/agents\/([^/]+)\/turns$/);
    if (turn) {
      if (request.method !== "POST") return capabilityJson({ error: "method_not_allowed" }, 405);
      const managedAgentId = await this.#managedAgent(turn[1]);
      if (!managedAgentId) return capabilityJson({ error: "not_found" }, 404);
      const body = await readAgentBody(request);
      if (body instanceof Response) return body;
      if (typeof body.id !== "string" || !TURN_ID.test(body.id) || !("input" in body)) {
        return capabilityJson({ error: "invalid_request" }, 400);
      }
      const idempotencyKey = request.headers.get("idempotency-key") ?? undefined;
      if (idempotencyKey && (idempotencyKey.length > 256 || !/^[\x21-\x7e]+$/.test(idempotencyKey))) {
        return capabilityJson({ error: "invalid_idempotency_key" }, 400);
      }
      const result = await this.env.NANOCODEX_AGENTS.submitTurn(this.#actor(), managedAgentId, {
        id: body.id,
        input: body.input,
        ...(idempotencyKey ? { idempotencyKey } : {}),
        publicOrigin: "https://apps.nanocodex.internal",
      });
      return capabilityJson(result.body, result.status);
    }

    const status = url.pathname.match(/^\/v1\/agents\/([^/]+)\/turns\/([^/]+)$/);
    if (status) {
      if (request.method !== "GET") return capabilityJson({ error: "method_not_allowed" }, 405);
      const managedAgentId = await this.#managedAgent(status[1]);
      if (!managedAgentId || !TURN_ID.test(status[2])) return capabilityJson({ error: "not_found" }, 404);
      const result = await this.env.NANOCODEX_AGENTS.getTurnStatus(
        this.#actor(),
        managedAgentId,
        status[2],
        { publicOrigin: "https://apps.nanocodex.internal" },
      );
      return capabilityJson(result.body, result.status);
    }
    return capabilityJson({ error: "not_found" }, 404);
  }

  async #state(path: string, method: string, body?: string): Promise<Response> {
    const response = await this.#stateStub().fetch(`https://app-state.internal${path}`, {
      body,
      headers: body ? { "content-type": "application/json" } : undefined,
      method,
    });
    if (!response.ok) {
      const error: { error?: string } = await response.json<{ error?: string }>().catch(() => ({}));
      throw new Error(error.error ?? "app state unavailable");
    }
    return response;
  }

  #stateStub(): DurableObjectStub<AppState> {
    const id = this.env.APP_STATE.idFromName(`${this.ctx.props.tenantId}:${this.ctx.props.appId}`);
    return this.env.APP_STATE.get(id);
  }

  async #managedAgent(handle: string): Promise<string | null> {
    if (!AGENT_HANDLE.test(handle)) return null;
    return this.#stateStub().resolveAgent(this.#actor(), handle);
  }

  #actor(): string {
    if (!USER_ID.test(this.ctx.props.actorUserId)) throw new Error("invalid capability actor");
    return this.ctx.props.actorUserId;
  }

  #require(grant: string): void {
    if (!this.ctx.props.grants.includes(grant)) throw new Error("capability denied");
  }
}

async function readAgentBody(request: Request): Promise<Record<string, unknown> | Response> {
  const bytes = await readBoundedBody(request, MAX_AGENT_REQUEST_BYTES);
  if (bytes === null) return capabilityJson({ error: "request_too_large" }, 413);
  try {
    const value = JSON.parse(new TextDecoder().decode(bytes)) as unknown;
    return value && typeof value === "object" && !Array.isArray(value)
      ? value as Record<string, unknown>
      : capabilityJson({ error: "invalid_json" }, 400);
  } catch {
    return capabilityJson({ error: "invalid_json" }, 400);
  }
}

async function readBoundedBody(request: Request, maxBytes: number): Promise<Uint8Array | null> {
  const declaredHeader = request.headers.get("content-length");
  if (declaredHeader !== null) {
    const declared = Number(declaredHeader);
    if (!Number.isSafeInteger(declared) || declared < 0 || declared > maxBytes) {
      await request.body?.cancel().catch(() => undefined);
      return null;
    }
  }
  if (!request.body) return new Uint8Array();

  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > maxBytes) {
        await reader.cancel().catch(() => undefined);
        return null;
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }

  const body = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}

function capabilityJson(body: unknown, status: number): Response {
  return Response.json(body, {
    status,
    headers: {
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
    },
  });
}

function assertKey(key: string): string {
  if (!validKey(key)) throw new Error("invalid key");
  return key;
}

function validKey(key: string): boolean {
  return typeof key === "string" && key.length <= MAX_KEY_CHARS && KEY.test(key);
}

function methodNotAllowed(): Response {
  return Response.json({ error: "method_not_allowed" }, { status: 405 });
}

function modelText(value: unknown): string | undefined {
  if (!value || typeof value !== "object") return undefined;
  const record = value as Record<string, unknown>;
  if (typeof record.response === "string") return record.response;
  if (Array.isArray(record.choices)) {
    const first = record.choices[0];
    if (first && typeof first === "object") {
      const message = (first as Record<string, unknown>).message;
      if (message && typeof message === "object") {
        const content = (message as Record<string, unknown>).content;
        if (typeof content === "string") return content;
      }
    }
  }
  return undefined;
}
