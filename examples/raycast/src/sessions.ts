import { randomUUID } from "node:crypto";
import { createReadStream, type Dirent, type Stats } from "node:fs";
import {
  mkdir,
  open,
  readFile,
  readdir,
  realpath,
  stat,
} from "node:fs/promises";
import { homedir } from "node:os";
import { basename, join } from "node:path";
import { createInterface } from "node:readline";
import type { SessionSnapshot } from "nanocodex/node";
import type { TerminalEntry, ToolActivity } from "nanocodex-tui";

const SESSION_ID =
  /([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/i;
const MODEL = "gpt-5.6-sol";
const SNAPSHOT_VERSION = 1;
const SUMMARY_SCAN_BYTES = 2 * 1024 * 1024;
const TRANSCRIPT_TAIL_BYTES = 16 * 1024 * 1024;
const MAX_TEXT_BYTES = 256 * 1024;
const MAX_TOOL_TEXT_BYTES = 16 * 1024;

type JsonObject = Record<string, unknown>;

export type SavedConversation = {
  id: string;
  path: string;
  title: string;
  cwd: string;
  source: string;
  createdAt: Date;
  updatedAt: Date;
  archived: boolean;
  size: number;
};

export type SavedTranscript = {
  entries: TerminalEntry[];
  truncated: boolean;
};

export type LoadedConversation = {
  snapshot: SessionSnapshot;
  transcript: SavedTranscript;
  instructions?: string;
  journal: CodexRolloutJournal;
};

export type PersistedTurn = {
  turnId?: string;
  prompt: string;
  finalMessage: string;
  snapshot: SessionSnapshot;
  compacted: boolean;
  startedAt: number;
};

type RolloutFile = {
  id: string;
  path: string;
  archived: boolean;
  size: number;
  createdAt: Date;
  updatedAt: Date;
};

type SessionMetadata = {
  id?: string;
  cwd?: string;
  source?: string;
  timestamp?: string;
  instructions?: string;
  firstWindowId?: string;
};

type MaterializedRollout = {
  metadata: SessionMetadata;
  history: JsonObject[];
  windowNumber: number;
  currentWindowId?: string;
};

export class CodexRolloutJournal {
  readonly threadId: string;
  readonly path: string;

  private expectedSize: number;
  private history: readonly unknown[];
  private windowNumber: number;
  private readonly firstWindowId: string;
  private currentWindowId: string;
  private writes = Promise.resolve();

  constructor(options: {
    threadId: string;
    path: string;
    expectedSize: number;
    history: readonly unknown[];
    windowNumber: number;
    firstWindowId: string;
    currentWindowId: string;
  }) {
    this.threadId = options.threadId;
    this.path = options.path;
    this.expectedSize = options.expectedSize;
    this.history = options.history;
    this.windowNumber = options.windowNumber;
    this.firstWindowId = options.firstWindowId;
    this.currentWindowId = options.currentWindowId;
  }

  appendTurn(turn: PersistedTurn): Promise<void> {
    const write = this.writes.then(() => this.writeTurn(turn));
    this.writes = write.catch(() => undefined);
    return write;
  }

  private async writeTurn(turn: PersistedTurn): Promise<void> {
    if (turn.snapshot.lineage_id !== this.threadId) {
      throw new Error(
        `snapshot lineage ${turn.snapshot.lineage_id} does not match Codex thread ${this.threadId}`,
      );
    }
    const metadata = await stat(this.path);
    if (metadata.size !== this.expectedSize) {
      throw new Error(
        "the Codex rollout changed in another process; refusing to append a competing writer",
      );
    }

    const completedAt = Date.now();
    const turnId = turn.turnId ?? randomUUID();
    const records: JsonObject[] = [
      rolloutLine({
        type: "event_msg",
        payload: {
          type: "task_started",
          turn_id: turnId,
          started_at: Math.floor(turn.startedAt / 1_000),
          model_context_window: null,
          collaboration_mode_kind: "default",
        },
      }),
      rolloutLine({
        type: "event_msg",
        payload: {
          type: "user_message",
          message: turn.prompt,
          images: [],
          local_images: [],
          text_elements: [],
        },
      }),
    ];

    const history = turn.snapshot.history;
    const replaceHistory =
      turn.compacted || !hasJsonPrefix(history, this.history);
    let nextWindowNumber = this.windowNumber;
    let nextCurrentWindowId = this.currentWindowId;
    if (replaceHistory) {
      const previousWindowId = this.currentWindowId;
      const windowId = randomUUID();
      nextWindowNumber += 1;
      nextCurrentWindowId = windowId;
      records.push(
        rolloutLine({
          type: "compacted",
          payload: {
            message: "",
            replacement_history: history,
            window_number: nextWindowNumber,
            first_window_id: this.firstWindowId,
            previous_window_id: previousWindowId,
            window_id: windowId,
          },
        }),
      );
    } else {
      for (const item of history.slice(this.history.length)) {
        records.push(
          rolloutLine({
            type: "response_item",
            payload: item,
          }),
        );
      }
    }

    records.push(
      rolloutLine({
        type: "event_msg",
        payload: {
          type: "agent_message",
          message: turn.finalMessage,
          phase: "final_answer",
          memory_citation: null,
        },
      }),
      rolloutLine({
        type: "event_msg",
        payload: {
          type: "task_complete",
          turn_id: turnId,
          last_agent_message: turn.finalMessage,
          started_at: Math.floor(turn.startedAt / 1_000),
          completed_at: Math.floor(completedAt / 1_000),
          duration_ms: Math.max(0, completedAt - turn.startedAt),
          time_to_first_token_ms: null,
        },
      }),
    );

    const encoded = `${records.map((record) => JSON.stringify(record)).join("\n")}\n`;
    const file = await open(this.path, "a");
    try {
      await file.writeFile(encoded, "utf8");
      await file.sync();
    } finally {
      await file.close();
    }
    this.expectedSize += Buffer.byteLength(encoded);
    this.history = history;
    this.windowNumber = nextWindowNumber;
    this.currentWindowId = nextCurrentWindowId;
  }
}

export async function listSavedConversations(
  limit = 100,
): Promise<SavedConversation[]> {
  if (limit <= 0) return [];
  const codexHome = codexHomePath();
  const [active, archived, names] = await Promise.all([
    collectRolloutFiles(join(codexHome, "sessions"), false),
    collectRolloutFiles(join(codexHome, "archived_sessions"), true),
    readThreadNames(join(codexHome, "session_index.jsonl")),
  ]);
  const deduplicated = new Map<string, RolloutFile>();
  for (const file of [...active, ...archived]) {
    const current = deduplicated.get(file.id);
    if (!current || current.updatedAt < file.updatedAt) {
      deduplicated.set(file.id, file);
    }
  }
  const recent = [...deduplicated.values()].sort(
    (left, right) => right.updatedAt.getTime() - left.updatedAt.getTime(),
  );
  const scanned = await mapLimit(
    recent,
    8,
    async (file): Promise<SavedConversation | undefined> => {
      let summary: Awaited<ReturnType<typeof scanConversationSummary>>;
      try {
        summary = await scanConversationSummary(file.path);
      } catch {
        return undefined;
      }
      const source = summary.metadata.source?.toLowerCase();
      if (
        summary.metadata.id !== file.id ||
        (source !== "cli" && source !== "vscode") ||
        !summary.firstUserMessage
      ) {
        return undefined;
      }
      const named = names.get(file.id);
      return {
        ...file,
        title:
          named ??
          summary.firstUserMessage ??
          `Conversation ${file.id.slice(0, 8)}`,
        cwd: summary.metadata.cwd ?? homedir(),
        source,
        createdAt: parseDate(summary.metadata.timestamp) ?? file.createdAt,
      };
    },
  );
  return scanned
    .filter((conversation) => conversation !== undefined)
    .slice(0, limit);
}

export async function loadSavedConversation(
  conversation: SavedConversation,
  fallbackWorkspace = homedir(),
): Promise<LoadedConversation> {
  const before = await stat(conversation.path);
  const outcome = await Promise.all([
    materializeRollout(conversation, before.size),
    loadSavedTranscript(conversation, before.size),
  ]).then(
    ([materialized, transcript]) => ({ materialized, transcript }),
    (error: unknown) => ({ error }),
  );
  const after = await stat(conversation.path);
  if (!sameFileVersion(before, after)) {
    throw new Error(
      "the Codex rollout changed while it was loading; reopen it to use one consistent version",
    );
  }
  if ("error" in outcome) throw outcome.error;
  const { materialized, transcript } = outcome;
  const workspace = await canonicalWorkspace(
    materialized.metadata.cwd ?? conversation.cwd,
    fallbackWorkspace,
  );
  const canonicalContext = materialized.history.find(isUserMessage);
  if (!canonicalContext) {
    throw new Error("the Codex rollout does not contain a user message");
  }
  const firstWindowId = materialized.metadata.firstWindowId ?? randomUUID();
  const snapshot = {
    version: SNAPSHOT_VERSION,
    model: MODEL,
    lineage_id: conversation.id,
    prompt_cache_key: conversation.id,
    workspace,
    canonical_context: canonicalContext,
    history: materialized.history,
  } satisfies SessionSnapshot;

  return {
    snapshot,
    transcript,
    ...(materialized.metadata.instructions
      ? { instructions: materialized.metadata.instructions }
      : {}),
    journal: new CodexRolloutJournal({
      threadId: conversation.id,
      path: conversation.path,
      expectedSize: after.size,
      history: materialized.history,
      windowNumber: materialized.windowNumber,
      firstWindowId,
      currentWindowId: materialized.currentWindowId ?? firstWindowId,
    }),
  };
}

export async function createSavedConversation(options: {
  workspace: string;
  instructions: string;
}): Promise<{
  sessionId: string;
  workspace: string;
  journal: CodexRolloutJournal;
}> {
  const workspace = await canonicalWorkspace(options.workspace);
  const threadId = randomUUID();
  const now = new Date();
  const directory = join(
    codexHomePath(),
    "sessions",
    String(now.getFullYear()),
    twoDigits(now.getMonth() + 1),
    twoDigits(now.getDate()),
  );
  await mkdir(directory, { recursive: true });
  const filenameTimestamp = [
    `${now.getFullYear()}-${twoDigits(now.getMonth() + 1)}-${twoDigits(now.getDate())}`,
    `${twoDigits(now.getHours())}-${twoDigits(now.getMinutes())}-${twoDigits(now.getSeconds())}`,
  ].join("T");
  const path = join(
    directory,
    `rollout-${filenameTimestamp}-${threadId}.jsonl`,
  );
  const windowId = randomUUID();
  const record = rolloutLine({
    type: "session_meta",
    payload: {
      session_id: threadId,
      id: threadId,
      timestamp: now.toISOString(),
      cwd: workspace,
      originator: "nanocodex-raycast",
      cli_version: "0.1.1",
      source: "cli",
      thread_source: "user",
      model_provider: "openai",
      base_instructions: { text: options.instructions },
      history_mode: "legacy",
      context_window: { window_id: windowId },
    },
  });
  const encoded = `${JSON.stringify(record)}\n`;
  const file = await open(path, "wx", 0o600);
  try {
    await file.writeFile(encoded, "utf8");
    await file.sync();
  } finally {
    await file.close();
  }

  return {
    sessionId: threadId,
    workspace,
    journal: new CodexRolloutJournal({
      threadId,
      path,
      expectedSize: Buffer.byteLength(encoded),
      history: [],
      windowNumber: 0,
      firstWindowId: windowId,
      currentWindowId: windowId,
    }),
  };
}

export async function findCompletedTurnMessage(
  path: string,
  turnId: string,
): Promise<string | undefined> {
  const input = createReadStream(path, { encoding: "utf8" });
  const lines = createInterface({ input, crlfDelay: Infinity });
  let message: string | undefined;
  try {
    for await (const line of lines) {
      const record = decodeRecord(line);
      const payload = object(record?.payload);
      if (
        record?.type === "event_msg" &&
        payload?.type === "task_complete" &&
        payload.turn_id === turnId
      ) {
        message = string(payload.last_agent_message);
      }
    }
  } finally {
    lines.close();
    input.destroy();
  }
  return message;
}

export async function loadSavedTranscript(
  conversation: SavedConversation,
  fileSize?: number,
): Promise<SavedTranscript> {
  const size = fileSize ?? (await stat(conversation.path)).size;
  if (!size) return { entries: [], truncated: false };
  const start = Math.max(0, size - TRANSCRIPT_TAIL_BYTES);
  const entries: TerminalEntry[] = [];
  const toolCallIds = new Set<string>();
  const input = createReadStream(conversation.path, {
    encoding: "utf8",
    ...(start ? { start } : {}),
    ...(size ? { end: size - 1 } : {}),
  });
  const lines = createInterface({ input, crlfDelay: Infinity });
  let skipPartialLine = start > 0;
  let lineNumber = 0;

  try {
    for await (const line of lines) {
      lineNumber += 1;
      if (skipPartialLine) {
        skipPartialLine = false;
        continue;
      }
      const record = decodeRecord(line);
      if (!record) continue;
      const payload = object(record.payload);
      if (
        record.type === "event_msg" &&
        payload?.type === "thread_rolled_back"
      ) {
        rollbackTranscript(entries, number(payload.num_turns) ?? 0, start > 0);
        toolCallIds.clear();
        for (const retained of entries) {
          if (retained.kind === "tool") {
            toolCallIds.add(retained.tool.callId);
          }
        }
        continue;
      }
      const entry = visibleEntry(record, `saved-${lineNumber}`);
      if (!entry) continue;
      if (entry.kind === "tool") {
        if (toolCallIds.has(entry.tool.callId)) continue;
        toolCallIds.add(entry.tool.callId);
      }
      pushDistinct(entries, entry);
    }
  } finally {
    lines.close();
    input.destroy();
  }

  return { entries, truncated: start > 0 };
}

async function materializeRollout(
  conversation: SavedConversation,
  fileSize: number,
): Promise<MaterializedRollout> {
  if (!fileSize) {
    throw new Error("Codex rollout is empty");
  }
  const metadata: SessionMetadata = {};
  let history: JsonObject[] = [];
  let windowNumber = 0;
  let currentWindowId: string | undefined;
  let sawSessionMetadata = false;
  const input = createReadStream(conversation.path, {
    encoding: "utf8",
    ...(fileSize ? { end: fileSize - 1 } : {}),
  });
  const lines = createInterface({ input, crlfDelay: Infinity });
  let lineNumber = 0;

  try {
    for await (const line of lines) {
      lineNumber += 1;
      if (!line.trim()) continue;
      let record: JsonObject | undefined;
      try {
        record = object(JSON.parse(line));
      } catch (cause) {
        throw new Error(
          `failed to decode ${conversation.path} line ${lineNumber}: ${errorMessage(cause)}`,
        );
      }
      if (!record) {
        throw new Error(
          `failed to decode ${conversation.path} line ${lineNumber}: expected a JSON object`,
        );
      }
      const payload = object(record.payload);
      switch (record.type) {
        case "session_meta": {
          if (sawSessionMetadata) break;
          sawSessionMetadata = true;
          if (string(payload?.id) !== conversation.id) {
            throw new Error(
              "Codex rollout thread ID does not match its filename",
            );
          }
          metadata.id = string(payload?.id);
          metadata.cwd = string(payload?.cwd);
          metadata.source = sourceName(payload?.source);
          metadata.timestamp = string(payload?.timestamp);
          metadata.instructions = string(
            object(payload?.base_instructions)?.text,
          );
          metadata.firstWindowId = string(
            object(payload?.context_window)?.window_id,
          );
          currentWindowId = metadata.firstWindowId;
          break;
        }
        case "turn_context": {
          const cwd = string(payload?.cwd);
          if (cwd) metadata.cwd = cwd;
          break;
        }
        case "response_item":
          if (!payload) {
            throw new Error(
              `Codex response item at line ${lineNumber} is not an object`,
            );
          }
          history.push(payload);
          break;
        case "compacted": {
          const replacement = payload?.replacement_history;
          if (!Array.isArray(replacement) || !replacement.every(isObject)) {
            throw new Error(
              `Codex compaction at line ${lineNumber} has invalid replacement history`,
            );
          }
          history = replacement;
          windowNumber = number(payload?.window_number) ?? windowNumber + 1;
          currentWindowId = string(payload?.window_id) ?? currentWindowId;
          break;
        }
        case "event_msg":
          if (payload?.type === "thread_rolled_back") {
            rollbackHistory(history, number(payload.num_turns) ?? 0);
          }
          break;
      }
    }
  } finally {
    lines.close();
    input.destroy();
  }
  if (!sawSessionMetadata) {
    throw new Error("Codex rollout is missing session metadata");
  }
  return { metadata, history, windowNumber, currentWindowId };
}

async function collectRolloutFiles(
  root: string,
  archived: boolean,
): Promise<RolloutFile[]> {
  const paths: string[] = [];
  const directories = [root];
  while (directories.length) {
    const directory = directories.pop();
    if (!directory) break;
    let entries: Dirent[];
    try {
      entries = await readdir(directory, { withFileTypes: true });
    } catch (cause) {
      if (isNodeError(cause, "ENOENT")) continue;
      throw cause;
    }
    for (const entry of entries) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) directories.push(path);
      else if (entry.isFile() && SESSION_ID.test(entry.name)) paths.push(path);
    }
  }

  return mapLimit(paths, 32, async (path) => {
    const metadata = await stat(path);
    const match = SESSION_ID.exec(path);
    if (!match?.[1]) throw new Error(`invalid Codex rollout filename: ${path}`);
    return {
      id: match[1],
      path,
      archived,
      size: metadata.size,
      createdAt: metadata.birthtime,
      updatedAt: metadata.mtime,
    };
  });
}

async function scanConversationSummary(path: string): Promise<{
  metadata: SessionMetadata;
  firstUserMessage?: string;
}> {
  const metadata: SessionMetadata = {};
  let sawSessionMetadata = false;
  const input = createReadStream(path, {
    encoding: "utf8",
    start: 0,
    end: SUMMARY_SCAN_BYTES - 1,
  });
  const lines = createInterface({ input, crlfDelay: Infinity });
  let firstUserMessage: string | undefined;

  try {
    for await (const line of lines) {
      const record = decodeRecord(line);
      if (!record) continue;
      if (record.type === "session_meta" && !sawSessionMetadata) {
        sawSessionMetadata = true;
        const payload = object(record.payload);
        metadata.id = string(payload?.id);
        metadata.cwd = string(payload?.cwd);
        metadata.source = sourceName(payload?.source);
        metadata.timestamp = string(payload?.timestamp);
      } else if (record.type === "turn_context") {
        const cwd = string(object(record.payload)?.cwd);
        if (cwd) metadata.cwd = cwd;
      } else if (
        record.type === "event_msg" &&
        object(record.payload)?.type === "user_message"
      ) {
        firstUserMessage = titleText(string(object(record.payload)?.message));
        if (firstUserMessage) break;
      } else if (record.type === "response_item") {
        firstUserMessage = titleText(
          responseUserMessageText(object(record.payload)),
        );
        if (firstUserMessage) break;
      }
    }
  } finally {
    lines.close();
    input.destroy();
  }
  return { metadata, firstUserMessage };
}

async function readThreadNames(path: string): Promise<Map<string, string>> {
  let content: string;
  try {
    content = await readFile(path, "utf8");
  } catch (cause) {
    if (isNodeError(cause, "ENOENT")) return new Map();
    throw cause;
  }
  const names = new Map<string, string>();
  for (const line of content.split("\n")) {
    const record = decodeRecord(line);
    const id = string(record?.id);
    const name = titleText(string(record?.thread_name));
    if (id && name) names.set(id, name);
  }
  return names;
}

function visibleEntry(
  record: JsonObject,
  id: string,
): TerminalEntry | undefined {
  const payload = object(record.payload);
  if (!payload) return undefined;

  if (record.type === "event_msg") {
    switch (payload.type) {
      case "user_message": {
        const text = bounded(string(payload.message), MAX_TEXT_BYTES);
        return text ? { id, kind: "user", text } : undefined;
      }
      case "agent_reasoning": {
        const text = bounded(string(payload.text), MAX_TEXT_BYTES);
        return text
          ? { id, kind: "reasoning", text, streaming: false }
          : undefined;
      }
      case "agent_message": {
        const text = bounded(string(payload.message), MAX_TEXT_BYTES);
        return text
          ? { id, kind: "assistant", text, streaming: false }
          : undefined;
      }
      case "mcp_tool_call_end": {
        const invocation = object(payload.invocation);
        const server = string(invocation?.server);
        const tool = string(invocation?.tool);
        const callId = string(payload.call_id);
        if (!server || !tool || !callId) return undefined;
        return toolEntry(
          id,
          callId,
          `${server}.${tool}`,
          invocation?.arguments,
        );
      }
      case "web_search_end": {
        const callId = string(payload.call_id);
        return callId
          ? toolEntry(id, callId, "web_search", payload.action)
          : undefined;
      }
    }
  }

  if (record.type === "response_item") {
    if (
      payload.type === "custom_tool_call" ||
      payload.type === "function_call"
    ) {
      const callId = string(payload.call_id);
      const name = string(payload.name);
      const argumentsValue =
        payload.type === "custom_tool_call" ? payload.input : payload.arguments;
      return callId && name
        ? toolEntry(id, callId, name, argumentsValue)
        : undefined;
    }
    if (payload.type === "reasoning" && Array.isArray(payload.summary)) {
      const text = payload.summary
        .flatMap((item) => {
          const summary = object(item);
          const text = string(summary?.text);
          return text ? [text] : [];
        })
        .join("\n");
      return text
        ? {
            id,
            kind: "reasoning",
            text: bounded(text, MAX_TEXT_BYTES)!,
            streaming: false,
          }
        : undefined;
    }
  }
  return undefined;
}

function toolEntry(
  id: string,
  callId: string,
  name: string,
  argumentsValue: unknown,
): TerminalEntry {
  const tool: ToolActivity = {
    callId,
    name,
    arguments: bounded(formatValue(argumentsValue), MAX_TOOL_TEXT_BYTES) ?? "",
    status: "completed",
    children: [],
  };
  return { id, kind: "tool", tool };
}

function pushDistinct(entries: TerminalEntry[], entry: TerminalEntry): void {
  const tail = entries.at(-1);
  if (
    tail &&
    tail.kind === entry.kind &&
    "text" in tail &&
    "text" in entry &&
    tail.text === entry.text
  ) {
    return;
  }
  entries.push(entry);
}

function rolloutLine(item: JsonObject): JsonObject {
  return { timestamp: new Date().toISOString(), ...item };
}

function decodeRecord(line: string): JsonObject | undefined {
  if (!line.trim()) return undefined;
  try {
    return object(JSON.parse(line));
  } catch {
    return undefined;
  }
}

function object(value: unknown): JsonObject | undefined {
  return isObject(value) ? value : undefined;
}

function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function string(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function number(value: unknown): number | undefined {
  return typeof value === "number" ? value : undefined;
}

function sourceName(value: unknown): string | undefined {
  if (typeof value === "string") return value;
  const source = object(value);
  return (
    string(source?.type) ??
    string(source?.kind) ??
    (source ? Object.keys(source)[0] : undefined)
  );
}

function isUserMessage(item: JsonObject): boolean {
  return item.type === "message" && item.role === "user";
}

function responseUserMessageText(
  item: JsonObject | undefined,
): string | undefined {
  if (!item || !isUserMessage(item) || !Array.isArray(item.content)) {
    return undefined;
  }
  const parts = item.content.flatMap((value) => {
    const content = object(value);
    const text = string(content?.text);
    return text ? [text] : [];
  });
  return parts.length ? parts.join("\n") : undefined;
}

function titleText(value: string | undefined): string | undefined {
  if (!value) return undefined;
  const normalized = value.split(/\s+/).filter(Boolean).join(" ");
  if (!normalized) return undefined;
  const characters = [...normalized];
  return characters.length <= 120
    ? normalized
    : `${characters.slice(0, 120).join("")}...`;
}

function bounded(
  value: string | undefined,
  maximum: number,
): string | undefined {
  if (!value) return undefined;
  return value.length <= maximum
    ? value
    : `${value.slice(0, maximum)}\n\n... truncated ...`;
}

function formatValue(value: unknown): string {
  if (typeof value === "string") return value;
  if (value === undefined) return "";
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function parseDate(value: string | undefined): Date | undefined {
  if (!value) return undefined;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? undefined : date;
}

function isNodeError(cause: unknown, code: string): boolean {
  return (
    cause instanceof Error &&
    "code" in cause &&
    (cause as NodeJS.ErrnoException).code === code
  );
}

async function canonicalWorkspace(
  path: string,
  fallback?: string,
): Promise<string> {
  try {
    return await realpath(path);
  } catch (cause) {
    if (fallback && fallback !== path) {
      try {
        return await realpath(fallback);
      } catch {
        // Report the recorded workspace below; it is the one the user chose.
      }
    }
    throw new Error(
      `the conversation workspace is unavailable at ${path}: ${errorMessage(cause)}`,
    );
  }
}

function rollbackHistory(history: JsonObject[], count: number): void {
  if (!Number.isSafeInteger(count) || count <= 0) return;
  const userPositions = history.flatMap((item, index) =>
    isUserMessage(item) ? [index] : [],
  );
  if (!userPositions.length) return;
  const offset = Math.max(0, userPositions.length - count);
  history.splice(userPositions[offset] ?? userPositions[0] ?? history.length);
}

function rollbackTranscript(
  entries: TerminalEntry[],
  count: number,
  truncated: boolean,
): void {
  if (!Number.isSafeInteger(count) || count <= 0) return;
  const userPositions = entries.flatMap((entry, index) =>
    entry.kind === "user" ? [index] : [],
  );
  if (!userPositions.length) {
    if (truncated) entries.length = 0;
    return;
  }
  if (truncated && count >= userPositions.length) {
    entries.length = 0;
    return;
  }
  const offset = Math.max(0, userPositions.length - count);
  entries.splice(userPositions[offset] ?? userPositions[0] ?? entries.length);
}

function hasJsonPrefix(
  history: readonly unknown[],
  prefix: readonly unknown[],
): boolean {
  if (history.length < prefix.length) return false;
  for (let index = 0; index < prefix.length; index += 1) {
    if (JSON.stringify(history[index]) !== JSON.stringify(prefix[index])) {
      return false;
    }
  }
  return true;
}

function sameFileVersion(left: Stats, right: Stats): boolean {
  return (
    left.dev === right.dev &&
    left.ino === right.ino &&
    left.size === right.size &&
    left.mtimeMs === right.mtimeMs
  );
}

function codexHomePath(): string {
  return process.env.CODEX_HOME || join(homedir(), ".codex");
}

function twoDigits(value: number): string {
  return String(value).padStart(2, "0");
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

async function mapLimit<input, output>(
  values: readonly input[],
  concurrency: number,
  operation: (value: input) => Promise<output>,
): Promise<output[]> {
  const output = new Array<output>(values.length);
  let next = 0;
  await Promise.all(
    Array.from({ length: Math.min(concurrency, values.length) }, async () => {
      while (true) {
        const index = next;
        next += 1;
        const value = values[index];
        if (value === undefined) return;
        output[index] = await operation(value);
      }
    }),
  );
  return output;
}

export function workspaceName(conversation: SavedConversation): string {
  return basename(conversation.cwd) || conversation.cwd;
}
