import { subagentsBrand } from "./tool-configuration.mjs";
import {
  closeSubagent,
  interruptSubagent,
  spawnSubagent,
  waitSubagents,
} from "../internal.mjs";

const DEFAULT_MAX_CONCURRENCY = 32;

export function create(options = {}) {
  if (!options || typeof options !== "object" || Array.isArray(options)) {
    throw new TypeError("Subagents.create options must be an object");
  }
  const maxConcurrency = options.maxConcurrency ?? DEFAULT_MAX_CONCURRENCY;
  if (!Number.isSafeInteger(maxConcurrency) || maxConcurrency < 1) {
    throw new TypeError("subagents maxConcurrency must be a positive safe integer");
  }
  return Object.freeze([Object.freeze({
    [subagentsBrand]: Object.freeze({ maxConcurrency }),
  })]);
}

export function spawn(agent, options) {
  return spawnSubagent(agent, options);
}

export function wait(agent, options) {
  return waitSubagents(agent, options);
}

export function interrupt(agent, agentId) {
  return interruptSubagent(agent, agentId);
}

export function close(agent, agentId) {
  return closeSubagent(agent, agentId);
}
