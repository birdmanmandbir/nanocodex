import type {
  Agent as BaseAgent,
  AgentActions,
  AgentOptions,
  AgentEvent,
  DefaultAgent,
  ToolConfiguration,
} from "../types.mjs";
import type { CloudflareDurableObjectStorage } from "../runtime/cloudflare-durability-store.mjs";
import type { Tool as SubagentTool } from "../runtime/subagents.mjs";

export type DurableObjectContext = Readonly<{
  storage: CloudflareDurableObjectStorage;
  acceptWebSocket(socket: WebSocket, tags?: string[]): void;
  getWebSockets(tag?: string): WebSocket[];
}>;

/** The owning Cloudflare Durable Object instance. Runtime fields remain adapter-private. */
export type DurableObjectOwner = object;

export type EventFrame = Readonly<{
  cursor: string;
  event: AgentEvent;
}> | Readonly<{
  type: "replay_paused";
  cursor: string;
  latest_cursor: string;
}>;

type CloudflareAgentActions = Omit<AgentActions, "events"> & Readonly<{
  events: AgentActions["events"] & Readonly<{
    /** Accepts a read-only hibernatable event socket; reconnect from the last event or replay pause cursor. */
    connect(request: Request): Response;
  }>;
}>;

/** A durable Agent whose Cloudflare event socket survives typed extensions. */
export type Agent<extended extends object = {}> =
  Omit<BaseAgent<CloudflareAgentActions & extended>, "extend"> & Readonly<{
    extend<const extension extends object>(
      decorator: (agent: Agent<extended>) => extension,
    ): Agent<extended & extension>;
  }>;

/** Removes the package-owned durable history for one Cloudflare Agent. */
export function destroy(owner: DurableObjectOwner): void;

/** Creates one durable Agent from its owning Durable Object instance. */
export function create(owner: create.Owner, options?: create.Options): Promise<create.ReturnType>;
export declare namespace create {
  type Owner = DurableObjectOwner;
  type Options = Readonly<{
    instructions?: string | undefined;
    tools?: ToolConfiguration | undefined;
  }>;
  type ReturnType = Agent;
}

/** Creates one non-durable Rust/WASM Agent in the current Cloudflare isolate. */
export function createEphemeral(
  owner: createEphemeral.Owner,
  options?: createEphemeral.Options,
): Promise<createEphemeral.ReturnType>;
export declare namespace createEphemeral {
  type Owner = DurableObjectOwner;
  type Options = Readonly<AgentOptions & {
    /** Caller-owned tools exposed directly to the model. */
    tools?: ToolConfiguration<SubagentTool> | undefined;
  }>;
  type ReturnType = DefaultAgent;
}
