import type { DefaultAgent, Thinking } from "../types.mjs";

declare const subagentToolBrand: unique symbol;

/** Opaque selector for the Rust-owned subagent tool set. */
export type Tool = Readonly<{
  [subagentToolBrand]: true;
}>;

export type Subagents = readonly [Tool];

export interface Options {
  /** Maximum number of concurrently active subagent turns. Defaults to 32. */
  maxConcurrency?: number | undefined;
}

export type AgentId = number;
export type AgentStatus =
  | Readonly<{ state: "pending" | "running" | "interrupted" | "closing" | "closed" }>
  | Readonly<{ state: "completed"; output: unknown }>
  | Readonly<{ state: "failed"; error: string }>;
export type AgentSummary = Readonly<{
  agent_id: AgentId;
  role: string;
  task: string;
  parent_agent_id: AgentId | null;
  status: AgentStatus;
  last_output?: unknown;
}>;
export type SpawnOptions = Readonly<{
  role: string;
  task: string;
  model?: "sol" | "terra" | "luna" | undefined;
  thinking?: Thinking | undefined;
  outputSchema: Record<string, unknown>;
}>;
export type SpawnReport = Readonly<{
  agent_id: AgentId;
  role: string;
  status: Readonly<{ state: "running" }>;
}>;
export type WaitOptions = Readonly<{
  agentIds: readonly AgentId[];
  timeoutMs?: number | undefined;
}>;
export type WaitReport = Readonly<{
  agents: readonly AgentSummary[];
  timed_out: boolean;
}>;
export type LifecycleReport = Readonly<{ agents: readonly AgentSummary[] }>;

/** Returns a spreadable Rust-backed tool extension for an Agent's tools array. */
export function create(options?: Options): Subagents;
/** Directly invokes the canonical Rust spawn_agent handler. */
export function spawn(agent: DefaultAgent, options: SpawnOptions): Promise<SpawnReport>;
/** Directly invokes the canonical Rust wait_agent handler. */
export function wait(agent: DefaultAgent, options: WaitOptions): Promise<WaitReport>;
/** Directly invokes the canonical Rust interrupt_agent handler. */
export function interrupt(agent: DefaultAgent, agentId: AgentId): Promise<LifecycleReport>;
/** Directly invokes the canonical Rust close_agent handler. */
export function close(agent: DefaultAgent, agentId: AgentId): Promise<LifecycleReport>;
