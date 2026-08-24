import { DurableObject } from "cloudflare:workers";

import {
  DurableMemoryError,
  MAX_MEMORY_RECORDS,
  MAX_MEMORY_TOTAL_CONTENT_BYTES,
  MEMORY_PROBATION_DURATION_MS,
  normalizeMemoryIdentity,
  parseMemoryOperation,
  rankMemories,
  type MemoryDeleteResult,
  type MemoryKey,
  type MemoryOperation,
  type MemoryPutResult,
  type MemoryReadResult,
  type MemoryRecord,
  type MemoryResult,
  type MemoryScanResult,
} from "./durable-memory";
import {
  HistorySearchError,
  HISTORY_VECTOR_MATCH_THRESHOLD,
  MAX_HISTORY_SEARCH_LIMIT,
  groupHistoryCitations,
  historyVectorRetrieval,
  historyFtsQuery,
  historySearchTerms,
  isAcceptedHistoryLexicalMatch,
  isExactHistoryIdentifierQuery,
  isRecord,
  parseHistoryFindSessionsInput,
  parseHistoryReadSessionInput,
  promptInputText,
  type HistoryFindSessionsInput,
  type HistoryFindSessionsResponse,
  type HistoryProjection,
  type HistoryReadSessionResponse,
  type SessionSearchHit,
} from "./history-search";

const OWNER_ASSERTION = "x-nanocodex-owner-id";
const SESSION_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const TURN_ID = /^[A-Za-z0-9._:-]{1,128}$/;
const MAX_BODY_BYTES = 2 * 1024 * 1024;
const MAX_AI_RETRY_DELAY_MS = 60_000;

export interface MemoryScopeEnv {
  HISTORY_AI_SEARCH?: AiSearchInstance;
}

type MemoryTurnRow = {
  segment_id: string;
  thread_id: string;
  title: string;
  turn_id: string;
  source_cursor: string;
  user_text: string;
  assistant_text: string;
  content: string;
  created_at: number;
  ai_item_id: string | null;
};

type DurableMemoryRow = {
  id: number;
  version: number;
  content: string;
  identity: string;
  created_at_ms: number;
  updated_at_ms: number;
  last_scanned_at_ms: number | null;
  scan_count: number;
  last_used_at_ms: number | null;
  use_count: number;
  probation_until_ms: number | null;
};

type RankedMemoryTurnRow = MemoryTurnRow & { rank: number; semantic_score?: number };

type AiOutboxRow = {
  operation_id: string;
  operation: "upsert" | "delete";
  segment_id: string;
  payload_json: string | null;
  ai_item_id: string | null;
  attempt_count: number;
  retry_at: number;
};

const json = (body: unknown, init: ResponseInit = {}) => Response.json(body, {
  ...init,
  headers: { "cache-control": "no-store", ...init.headers },
});

export class MemoryScope extends DurableObject<MemoryScopeEnv> {
  #aiTask?: Promise<void>;
  #aiWarmup?: Promise<void>;

  constructor(ctx: DurableObjectState, env: MemoryScopeEnv) {
    super(ctx, env);
    this.ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS memory_scope_state (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        owner_id TEXT NOT NULL UNIQUE,
        created_at INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS memory_threads (
        thread_id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS memory_turns (
        segment_id TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL,
        turn_id TEXT NOT NULL,
        source_cursor INTEGER NOT NULL,
        user_text TEXT NOT NULL,
        assistant_text TEXT NOT NULL,
        content TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        ai_item_id TEXT,
        FOREIGN KEY (thread_id) REFERENCES memory_threads(thread_id) ON DELETE CASCADE,
        UNIQUE (thread_id, turn_id)
      );
      CREATE INDEX IF NOT EXISTS memory_turns_thread_created
        ON memory_turns(thread_id, created_at);
      CREATE TABLE IF NOT EXISTS memory_tombstones (
        thread_id TEXT PRIMARY KEY,
        deleted_at INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS memory_ai_outbox (
        operation_id TEXT PRIMARY KEY,
        operation TEXT NOT NULL CHECK (operation IN ('upsert', 'delete')),
        segment_id TEXT NOT NULL,
        payload_json TEXT,
        ai_item_id TEXT,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        retry_at INTEGER NOT NULL DEFAULT 0
      );
      CREATE TABLE IF NOT EXISTS durable_memories (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        version INTEGER NOT NULL CHECK (version > 0),
        content TEXT NOT NULL,
        identity TEXT NOT NULL UNIQUE,
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        last_scanned_at_ms INTEGER,
        scan_count INTEGER NOT NULL DEFAULT 0,
        last_used_at_ms INTEGER,
        use_count INTEGER NOT NULL DEFAULT 0,
        probation_until_ms INTEGER
      );
      CREATE VIRTUAL TABLE IF NOT EXISTS memory_turns_fts USING fts5(
        content,
        content='memory_turns',
        content_rowid='rowid',
        tokenize='unicode61'
      );
      CREATE TRIGGER IF NOT EXISTS memory_turns_ai AFTER INSERT ON memory_turns BEGIN
        INSERT INTO memory_turns_fts(rowid, content) VALUES (new.rowid, new.content);
      END;
      CREATE TRIGGER IF NOT EXISTS memory_turns_ad AFTER DELETE ON memory_turns BEGIN
        INSERT INTO memory_turns_fts(memory_turns_fts, rowid, content)
          VALUES ('delete', old.rowid, old.content);
      END;
      CREATE TRIGGER IF NOT EXISTS memory_turns_au AFTER UPDATE OF content ON memory_turns BEGIN
        INSERT INTO memory_turns_fts(memory_turns_fts, rowid, content)
          VALUES ('delete', old.rowid, old.content);
        INSERT INTO memory_turns_fts(rowid, content) VALUES (new.rowid, new.content);
      END;
    `);
    this.ctx.blockConcurrencyWhile(async () => {
      this.#scheduleAiOutbox();
      this.#startAiWarmup();
    });
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const assertedOwner = request.headers.get(OWNER_ASSERTION);
    if (request.method === "PUT" && url.pathname === "/initialize") {
      if (assertedOwner === null) return json({ error: "not_found" }, { status: 404 });
      return this.#initialize(assertedOwner);
    }
    if (!this.#authorized(assertedOwner)) return json({ error: "not_found" }, { status: 404 });
    try {
      if (request.method === "POST" && url.pathname === "/project") {
        const projection = await parseJsonBody<HistoryProjection>(request);
        this.#project(projection);
        this.#scheduleAiOutbox();
        return new Response(null, { status: 204 });
      }
      const threadMatch = url.pathname.match(/^\/threads\/([0-9a-f-]+)$/);
      if (request.method === "DELETE" && threadMatch) {
        if (!SESSION_ID.test(threadMatch[1]!)) {
          throw new HistorySearchError(400, "invalid_thread_id", "invalid thread id");
        }
        this.#deleteThread(threadMatch[1]!);
        this.#scheduleAiOutbox();
        return new Response(null, { status: 204 });
      }
      if (request.method === "POST" && url.pathname === "/search") {
        const input = parseHistoryFindSessionsInput(await parseJsonBody<unknown>(request));
        return json(await this.#findSessions(input));
      }
      if (request.method === "POST" && url.pathname === "/read") {
        const input = parseHistoryReadSessionInput(await parseJsonBody<unknown>(request));
        const rows = this.#readThread(
          input.session_id,
          input.turn_ids,
          MAX_HISTORY_SEARCH_LIMIT,
        );
        const turns = rows.map((row) => ({
          session_id: row.thread_id,
          title: row.title,
          turn_id: row.turn_id,
          cursor: row.source_cursor,
          user: row.user_text,
          assistant: row.assistant_text,
        }));
        const citations = groupHistoryCitations(rows.map((row) => ({
          session_id: row.thread_id,
          title: row.title,
          turn_id: row.turn_id,
          cursor: row.source_cursor,
        })));
        return json({ turns, citations } satisfies HistoryReadSessionResponse);
      }
      if (request.method === "GET" && url.pathname === "/memory") {
        return json({ memories: this.#listMemories() });
      }
      if (request.method === "POST" && url.pathname === "/memory") {
        const operation = parseMemoryOperation(await parseJsonBody<unknown>(request));
        return json(this.#memory(operation));
      }
      return json({ error: "not_found" }, { status: 404 });
    } catch (error) {
      if (error instanceof HistorySearchError) {
        return json({ error: error.code, message: error.message }, { status: error.status });
      }
      if (error instanceof DurableMemoryError) {
        return json({ error: error.code, message: error.message }, {
          status: error.code === "memory_conflict" || error.code === "memory_duplicate" ? 409
            : error.code === "memory_not_found" ? 404
              : error.code === "memory_capacity" || error.code === "memory_secret_rejected" ? 422
                : 400,
        });
      }
      console.error("memory scope request failed", errorMessage(error));
      return json({ error: "memory_scope_failed", message: errorMessage(error) }, { status: 500 });
    }
  }

  async alarm(): Promise<void> {
    if (this.#aiTask) await this.#aiTask.catch(() => {});
    else await this.#drainAiOutbox();
    await this.#scheduleNextAlarm();
  }

  #initialize(ownerId: string): Response {
    const current = this.#ownerId();
    if (current !== undefined && current !== ownerId) return json({ error: "not_found" }, { status: 404 });
    if (current === undefined) {
      this.ctx.storage.sql.exec(
        "INSERT INTO memory_scope_state (singleton, owner_id, created_at) VALUES (1, ?, ?)",
        ownerId,
        Date.now(),
      );
    }
    this.#startAiWarmup();
    return new Response(null, { status: 204 });
  }

  #authorized(assertedOwner: string | null): boolean {
    return assertedOwner !== null && assertedOwner === this.#ownerId();
  }

  #ownerId(): string | undefined {
    return this.ctx.storage.sql.exec<{ owner_id: string }>(
      "SELECT owner_id FROM memory_scope_state WHERE singleton = 1",
    ).toArray()[0]?.owner_id;
  }

  #project(projection: HistoryProjection): void {
    if (!isRecord(projection)
      || typeof projection.thread_id !== "string" || !SESSION_ID.test(projection.thread_id)
      || typeof projection.turn_id !== "string" || !TURN_ID.test(projection.turn_id)
      || typeof projection.cursor !== "string" || !/^\d+$/.test(projection.cursor)
      || typeof projection.title !== "string"
      || typeof projection.final_message !== "string"
      || !Number.isSafeInteger(projection.created_at)) {
      throw new HistorySearchError(400, "invalid_projection", "invalid history projection");
    }
    const userText = promptInputText(projection.input);
    const content = [`User: ${userText}`, `Assistant: ${projection.final_message}`].join("\n\n");
    const segmentId = `${projection.thread_id}:${projection.turn_id}`;
    this.ctx.storage.transactionSync(() => {
      if (this.ctx.storage.sql.exec(
        "SELECT 1 AS present FROM memory_tombstones WHERE thread_id = ?",
        projection.thread_id,
      ).toArray().length > 0) return;
      this.ctx.storage.sql.exec(
        `INSERT INTO memory_threads (thread_id, title, created_at, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(thread_id) DO UPDATE SET
           title = excluded.title,
           updated_at = MAX(memory_threads.updated_at, excluded.updated_at)`,
        projection.thread_id,
        projection.title,
        projection.created_at,
        projection.created_at,
      );
      this.ctx.storage.sql.exec(
        `INSERT INTO memory_turns (
           segment_id, thread_id, turn_id, source_cursor,
           user_text, assistant_text, content, created_at
         ) VALUES (?, ?, ?, CAST(? AS INTEGER), ?, ?, ?, ?)
         ON CONFLICT(segment_id) DO UPDATE SET
           source_cursor = excluded.source_cursor,
           user_text = excluded.user_text,
           assistant_text = excluded.assistant_text,
           content = excluded.content,
           created_at = excluded.created_at
         WHERE excluded.source_cursor >= memory_turns.source_cursor`,
        segmentId,
        projection.thread_id,
        projection.turn_id,
        projection.cursor,
        userText,
        projection.final_message,
        content,
        projection.created_at,
      );
      if (this.env.HISTORY_AI_SEARCH !== undefined) {
        this.ctx.storage.sql.exec(
          `INSERT INTO memory_ai_outbox (
             operation_id, operation, segment_id, payload_json, attempt_count, retry_at
           ) VALUES (?, 'upsert', ?, ?, 0, 0)
           ON CONFLICT(operation_id) DO UPDATE SET
             payload_json = excluded.payload_json,
             attempt_count = 0,
             retry_at = 0`,
          `upsert:${segmentId}`,
          segmentId,
          JSON.stringify({
            name: `${segmentId}.md`,
            content,
            metadata: {
              scope_id: this.#ownerId(),
              segment_id: segmentId,
            },
          }),
        );
      }
    });
  }

  #deleteThread(threadId: string): void {
    this.ctx.storage.transactionSync(() => {
      const indexed = this.ctx.storage.sql.exec<{ segment_id: string; ai_item_id: string | null }>(
        "SELECT segment_id, ai_item_id FROM memory_turns WHERE thread_id = ?",
        threadId,
      ).toArray();
      this.ctx.storage.sql.exec(
        "INSERT OR REPLACE INTO memory_tombstones (thread_id, deleted_at) VALUES (?, ?)",
        threadId,
        Date.now(),
      );
      if (this.env.HISTORY_AI_SEARCH !== undefined) {
        for (const item of indexed) {
          this.ctx.storage.sql.exec(
            "DELETE FROM memory_ai_outbox WHERE segment_id = ?",
            item.segment_id,
          );
          this.ctx.storage.sql.exec(
            `INSERT OR REPLACE INTO memory_ai_outbox (
               operation_id, operation, segment_id, payload_json, ai_item_id, attempt_count, retry_at
             ) VALUES (?, 'delete', ?, NULL, ?, 0, 0)`,
            `delete:${item.segment_id}`,
            item.segment_id,
            item.ai_item_id,
          );
        }
      }
      this.ctx.storage.sql.exec("DELETE FROM memory_threads WHERE thread_id = ?", threadId);
    });
  }

  async #findSessions(input: HistoryFindSessionsInput): Promise<HistoryFindSessionsResponse> {
    const local = this.#localSearch(input.query, input.limit);
    let rows = local;
    if (this.env.HISTORY_AI_SEARCH !== undefined && !isExactHistoryIdentifierQuery(input.query)) {
      try {
        const vector = await this.#autoVectorSearch(input.query, input.limit);
        // Prose queries are routed here for semantic retrieval; exact
        // identifiers already take the FTS-only path above. Keep the semantic
        // winner first and use lexical rows to broaden the remainder.
        rows = interleaveRankedRows(vector, local, input.limit);
      } catch (error) {
        // SQLite remains authoritative while uploads are pending or the
        // external index is unavailable.
        console.error("memory AI Search query failed; using local FTS", errorMessage(error));
      }
    }
    const results = rows.map((row) => memoryHit(row, input.query));
    return {
      query: input.query,
      results,
      citations: groupHistoryCitations(results),
    };
  }

  #localSearch(query: string, limit: number): RankedMemoryTurnRow[] {
    const match = historyFtsQuery(query);
    if (!match) return [];
    const candidateLimit = Math.min(50, Math.max(limit, limit * 3));
    return this.ctx.storage.sql.exec<RankedMemoryTurnRow>(
      `SELECT m.segment_id, m.thread_id, t.title, m.turn_id,
              CAST(m.source_cursor AS TEXT) AS source_cursor,
              m.user_text, m.assistant_text, m.content, m.created_at, m.ai_item_id,
              bm25(memory_turns_fts) AS rank
       FROM memory_turns_fts
       JOIN memory_turns m ON m.rowid = memory_turns_fts.rowid
       JOIN memory_threads t ON t.thread_id = m.thread_id
       WHERE memory_turns_fts MATCH ?
       ORDER BY rank, m.created_at DESC
       LIMIT ?`,
      match,
      candidateLimit,
    ).toArray()
      .filter((row) => isAcceptedHistoryLexicalMatch(query, row.content))
      .slice(0, limit);
  }

  async #autoVectorSearch(query: string, limit: number): Promise<RankedMemoryTurnRow[]> {
    const ownerId = this.#ownerId();
    if (ownerId === undefined) return [];
    const searched = await this.env.HISTORY_AI_SEARCH!.search({
      query,
      ai_search_options: {
        retrieval: historyVectorRetrieval(ownerId, limit),
        query_rewrite: { enabled: false },
        reranking: { enabled: false },
        // Memory is mutable and AI Search may accept an item before its
        // filtered vector view is complete. Caching that early result makes a
        // newly projected turn invisible for subsequent identical queries.
        cache: { enabled: false },
      },
    });
    const candidates = searched.chunks.flatMap((chunk) => {
      const segmentId = chunk.item.metadata?.segment_id;
      const score = chunk.scoring_details?.vector_score ?? chunk.score;
      return typeof segmentId === "string"
        && Number.isFinite(score)
        && score >= HISTORY_VECTOR_MATCH_THRESHOLD
        ? [{ segmentId, score }]
        : [];
    });
    const bySegment = new Map<string, { segmentId: string; score: number }>();
    for (const candidate of candidates) {
      const current = bySegment.get(candidate.segmentId);
      if (current === undefined || candidate.score > current.score) {
        bySegment.set(candidate.segmentId, candidate);
      }
    }
    const unique = [...bySegment.values()];
    if (unique.length === 0) return [];
    const placeholders = unique.map(() => "?").join(", ");
    const rows = this.ctx.storage.sql.exec<MemoryTurnRow>(
      `SELECT m.segment_id, m.thread_id, t.title, m.turn_id,
              CAST(m.source_cursor AS TEXT) AS source_cursor,
              m.user_text, m.assistant_text, m.content, m.created_at, m.ai_item_id
       FROM memory_turns m
       JOIN memory_threads t ON t.thread_id = m.thread_id
       WHERE m.segment_id IN (${placeholders})`,
      ...unique.map(({ segmentId }) => segmentId),
    ).toArray();
    const byId = new Map(rows.map((row) => [row.segment_id, row]));
    return unique.flatMap(({ segmentId, score }) => {
      const row = byId.get(segmentId);
      return row === undefined ? [] : [{ ...row, rank: -score, semantic_score: score }];
    }).slice(0, limit);
  }

  #readThread(threadId: string, turnIds: readonly string[] | undefined, limit: number): MemoryTurnRow[] {
    if (turnIds !== undefined && turnIds.length > 0) {
      const selected = [...new Set(turnIds)].slice(0, MAX_HISTORY_SEARCH_LIMIT);
      const placeholders = selected.map(() => "?").join(", ");
      return this.ctx.storage.sql.exec<MemoryTurnRow>(
        `SELECT m.segment_id, m.thread_id, t.title, m.turn_id,
                CAST(m.source_cursor AS TEXT) AS source_cursor,
                m.user_text, m.assistant_text, m.content, m.created_at, m.ai_item_id
         FROM memory_turns m
         JOIN memory_threads t ON t.thread_id = m.thread_id
         WHERE m.thread_id = ? AND m.turn_id IN (${placeholders})
         ORDER BY m.created_at, m.rowid
         LIMIT ?`,
        threadId,
        ...selected,
        limit,
      ).toArray();
    }
    return this.ctx.storage.sql.exec<MemoryTurnRow>(
      `SELECT * FROM (
         SELECT m.segment_id, m.thread_id, t.title, m.turn_id,
                CAST(m.source_cursor AS TEXT) AS source_cursor,
                m.user_text, m.assistant_text, m.content, m.created_at, m.ai_item_id
         FROM memory_turns m
         JOIN memory_threads t ON t.thread_id = m.thread_id
         WHERE m.thread_id = ?
         ORDER BY m.created_at DESC, m.rowid DESC
         LIMIT ?
       ) ORDER BY created_at`,
      threadId,
      limit,
    ).toArray();
  }

  #memory(operation: MemoryOperation): MemoryResult {
    switch (operation.operation) {
      case "scan": return this.#scanMemories(operation.query, operation.limit);
      case "read": return this.#readMemories(operation.keys);
      case "put": return this.#putMemory(operation.content, operation.replace);
      case "delete": return this.#deleteMemory(operation.key);
    }
  }

  #listMemories(): MemoryRecord[] {
    const now = Date.now();
    return this.ctx.storage.transactionSync(() => {
      this.#pruneMemories(now);
      return this.ctx.storage.sql.exec<DurableMemoryRow>(
        "SELECT * FROM durable_memories ORDER BY id",
      ).toArray().map(memoryRecord);
    });
  }

  #scanMemories(query: string, limit: number): MemoryScanResult {
    const now = Date.now();
    return this.ctx.storage.transactionSync(() => {
      this.#pruneMemories(now);
      const rows = this.ctx.storage.sql.exec<DurableMemoryRow>(
        "SELECT * FROM durable_memories ORDER BY id",
      ).toArray();
      const scan = rankMemories(query, rows.map(memoryRecord), limit);
      for (const candidate of scan.candidates) {
        this.ctx.storage.sql.exec(
          `UPDATE durable_memories
           SET last_scanned_at_ms = ?,
               scan_count = MIN(scan_count + 1, ?)
           WHERE id = ? AND version = ?`,
          now,
          Number.MAX_SAFE_INTEGER,
          candidate.key.id,
          candidate.key.version,
        );
      }
      return { operation: "scan", ...scan };
    });
  }

  #readMemories(keys: readonly MemoryKey[]): MemoryReadResult {
    const now = Date.now();
    return this.ctx.storage.transactionSync(() => {
      this.#pruneMemories(now);
      const memories: MemoryRecord[] = [];
      const seen = new Set<number>();
      for (const key of keys) {
        if (seen.has(key.id)) continue;
        seen.add(key.id);
        const row = this.ctx.storage.sql.exec<DurableMemoryRow>(
          "SELECT * FROM durable_memories WHERE id = ? AND version = ?",
          key.id,
          key.version,
        ).toArray()[0];
        if (!row) continue;
        const useCount = Math.min(Number.MAX_SAFE_INTEGER, row.use_count + 1);
        this.ctx.storage.sql.exec(
          `UPDATE durable_memories
           SET last_used_at_ms = ?, probation_until_ms = NULL, use_count = ?
           WHERE id = ? AND version = ?`,
          now,
          useCount,
          key.id,
          key.version,
        );
        memories.push(memoryRecord({
          ...row,
          last_used_at_ms: now,
          use_count: useCount,
          probation_until_ms: null,
        }));
      }
      return { operation: "read", memories };
    });
  }

  #putMemory(content: string, replace: MemoryKey | undefined): MemoryPutResult {
    if (containsLikelySecret(content)) {
      throw new DurableMemoryError(
        "memory_secret_rejected",
        "memory content was rejected as a likely secret",
      );
    }
    const identity = normalizeMemoryIdentity(content);
    const now = Date.now();
    const probationUntil = now + MEMORY_PROBATION_DURATION_MS;
    const contentBytes = new TextEncoder().encode(content).byteLength;
    return this.ctx.storage.transactionSync(() => {
      this.#pruneMemories(now);
      const duplicate = this.ctx.storage.sql.exec<{ id: number }>(
        "SELECT id FROM durable_memories WHERE identity = ? AND (? IS NULL OR id != ?)",
        identity,
        replace?.id ?? null,
        replace?.id ?? null,
      ).toArray()[0];
      if (duplicate) {
        throw new DurableMemoryError("memory_duplicate", "an equivalent memory already exists");
      }
      const capacity = this.ctx.storage.sql.exec<{ count: number; content_bytes: number }>(
        `SELECT COUNT(*) AS count,
                COALESCE(SUM(length(CAST(content AS BLOB))), 0) AS content_bytes
         FROM durable_memories`,
      ).toArray()[0] ?? { count: 0, content_bytes: 0 };

      if (replace) {
        const current = this.ctx.storage.sql.exec<DurableMemoryRow>(
          "SELECT * FROM durable_memories WHERE id = ?",
          replace.id,
        ).toArray()[0];
        if (!current) throw new DurableMemoryError("memory_not_found", "memory was not found");
        if (current.version !== replace.version || current.version >= Number.MAX_SAFE_INTEGER) {
          throw new DurableMemoryError("memory_conflict", "memory changed since it was read");
        }
        const currentBytes = new TextEncoder().encode(current.content).byteLength;
        if (capacity.content_bytes - currentBytes + contentBytes > MAX_MEMORY_TOTAL_CONTENT_BYTES) {
          throw new DurableMemoryError("memory_capacity", "memory content capacity was reached");
        }
        this.ctx.storage.sql.exec(
          `UPDATE durable_memories
           SET version = version + 1, content = ?, identity = ?, updated_at_ms = ?,
               last_scanned_at_ms = NULL, scan_count = 0,
               last_used_at_ms = NULL, use_count = 0, probation_until_ms = ?
           WHERE id = ? AND version = ?`,
          content,
          identity,
          now,
          probationUntil,
          replace.id,
          replace.version,
        );
        const updated = this.ctx.storage.sql.exec<DurableMemoryRow>(
          "SELECT * FROM durable_memories WHERE id = ?",
          replace.id,
        ).toArray()[0];
        if (!updated) throw new DurableMemoryError("memory_not_found", "memory was not found");
        return { operation: "put", memory: memoryRecord(updated), replaced: true };
      }

      if (capacity.count >= MAX_MEMORY_RECORDS
        || capacity.content_bytes + contentBytes > MAX_MEMORY_TOTAL_CONTENT_BYTES) {
        throw new DurableMemoryError("memory_capacity", "memory storage capacity was reached");
      }
      this.ctx.storage.sql.exec(
        `INSERT INTO durable_memories (
           version, content, identity, created_at_ms, updated_at_ms, probation_until_ms
         ) VALUES (1, ?, ?, ?, ?, ?)`,
        content,
        identity,
        now,
        now,
        probationUntil,
      );
      const inserted = this.ctx.storage.sql.exec<DurableMemoryRow>(
        "SELECT * FROM durable_memories WHERE identity = ?",
        identity,
      ).toArray()[0];
      if (!inserted) throw new DurableMemoryError("memory_not_found", "memory was not found");
      return { operation: "put", memory: memoryRecord(inserted), replaced: false };
    });
  }

  #deleteMemory(key: MemoryKey): MemoryDeleteResult {
    return this.ctx.storage.transactionSync(() => {
      const current = this.ctx.storage.sql.exec<Pick<DurableMemoryRow, "version">>(
        "SELECT version FROM durable_memories WHERE id = ?",
        key.id,
      ).toArray()[0];
      if (!current) return { operation: "delete", key };
      if (current.version !== key.version) {
        throw new DurableMemoryError("memory_conflict", "memory changed since it was read");
      }
      this.ctx.storage.sql.exec(
        "DELETE FROM durable_memories WHERE id = ? AND version = ?",
        key.id,
        key.version,
      );
      return { operation: "delete", key };
    });
  }

  #pruneMemories(now: number): void {
    this.ctx.storage.sql.exec(
      `DELETE FROM durable_memories
       WHERE probation_until_ms IS NOT NULL AND probation_until_ms <= ? AND use_count = 0`,
      now,
    );
  }

  #scheduleAiOutbox(): void {
    if (this.env.HISTORY_AI_SEARCH === undefined) return;
    this.ctx.waitUntil(this.ctx.storage.setAlarm(Date.now() + 1).catch((error) => {
      console.error("failed to schedule memory AI Search outbox", errorMessage(error));
    }));
    if (this.#aiTask) return;
    const task = this.#drainAiOutbox();
    this.#aiTask = task;
    void task.finally(() => {
      if (this.#aiTask === task) this.#aiTask = undefined;
    }).catch(() => {});
    this.ctx.waitUntil(task.catch(async (error) => {
      console.error("memory AI Search projection failed", errorMessage(error));
      await this.#scheduleNextAlarm();
    }));
  }

  #startAiWarmup(): void {
    const ownerId = this.#ownerId();
    if (this.env.HISTORY_AI_SEARCH === undefined || ownerId === undefined || this.#aiWarmup) return;
    const task = this.env.HISTORY_AI_SEARCH.search({
      query: "nanocodex memory search",
      ai_search_options: {
        retrieval: {
          retrieval_type: "vector",
          max_num_results: 1,
          filters: { scope_id: { $eq: ownerId } },
          return_on_failure: false,
        },
        query_rewrite: { enabled: false },
        reranking: { enabled: false },
        cache: { enabled: false },
      },
    }).then(() => {});
    this.#aiWarmup = task;
    this.ctx.waitUntil(task.catch((error) => {
      console.error("memory AI Search warmup failed", errorMessage(error));
    }));
  }

  async #drainAiOutbox(): Promise<void> {
    if (this.env.HISTORY_AI_SEARCH === undefined) return;
    while (true) {
      const rows = this.ctx.storage.sql.exec<AiOutboxRow>(
        `SELECT operation_id, operation, segment_id, payload_json, ai_item_id,
                attempt_count, retry_at
         FROM memory_ai_outbox
         WHERE retry_at <= ?
         ORDER BY rowid
         LIMIT 16`,
        Date.now(),
      ).toArray();
      if (rows.length === 0) break;
      for (const row of rows) {
        try {
          if (row.operation === "delete") {
            if (row.ai_item_id !== null) {
              await this.env.HISTORY_AI_SEARCH.items.delete(row.ai_item_id);
            }
          } else {
            const payload = JSON.parse(row.payload_json ?? "null") as {
              name: string;
              content: string;
              metadata: Record<string, unknown>;
            };
            const item = await this.env.HISTORY_AI_SEARCH.items.upload(
              payload.name,
              payload.content,
              { metadata: payload.metadata },
            );
            const current = this.ctx.storage.sql.exec<{ present: number }>(
              "SELECT 1 AS present FROM memory_turns WHERE segment_id = ?",
              row.segment_id,
            ).toArray()[0];
            if (!current) await this.env.HISTORY_AI_SEARCH.items.delete(item.id);
            else {
              this.ctx.storage.sql.exec(
                "UPDATE memory_turns SET ai_item_id = ? WHERE segment_id = ?",
                item.id,
                row.segment_id,
              );
            }
          }
          this.ctx.storage.sql.exec(
            "DELETE FROM memory_ai_outbox WHERE operation_id = ?",
            row.operation_id,
          );
        } catch (error) {
          const attempt = row.attempt_count + 1;
          this.ctx.storage.sql.exec(
            `UPDATE memory_ai_outbox SET attempt_count = ?, retry_at = ?
             WHERE operation_id = ?`,
            attempt,
            Date.now() + retryDelayMs(attempt),
            row.operation_id,
          );
          console.error("memory AI Search outbox operation failed", errorMessage(error));
        }
      }
    }
    await this.#scheduleNextAlarm();
  }

  async #scheduleNextAlarm(): Promise<void> {
    const row = this.ctx.storage.sql.exec<{ retry_at: number }>(
      "SELECT retry_at FROM memory_ai_outbox ORDER BY retry_at LIMIT 1",
    ).toArray()[0];
    if (!row) {
      await this.ctx.storage.deleteAlarm();
      return;
    }
    await this.ctx.storage.setAlarm(Math.max(Date.now() + 1, row.retry_at));
  }
}

function memoryHit(row: RankedMemoryTurnRow, query: string): SessionSearchHit {
  return {
    session_id: row.thread_id,
    title: row.title,
    turn_id: row.turn_id,
    cursor: row.source_cursor,
    score: row.semantic_score ?? normalizedScore(row.rank),
    snippet: snippet(row.content, query),
  };
}

function memoryRecord(row: DurableMemoryRow): MemoryRecord {
  return {
    key: { id: row.id, version: row.version },
    content: row.content,
    created_at_ms: row.created_at_ms,
    updated_at_ms: row.updated_at_ms,
    last_scanned_at_ms: row.last_scanned_at_ms,
    scan_count: row.scan_count,
    last_used_at_ms: row.last_used_at_ms,
    use_count: row.use_count,
    probation_until_ms: row.probation_until_ms,
  };
}

const SECRET_PREFIXES = [
  "sk-", "sk_", "ghp_", "gho_", "ghu_", "ghs_", "github_pat_",
  "xoxb-", "xoxp-", "xoxa-", "xoxr-", "akia",
] as const;
const SECRET_ASSIGNMENT_NAMES = [
  "password", "passwd", "secret", "token", "private_key", "private-key",
  "api_key", "api-key", "apikey",
] as const;

/** Conservative parity with Tact's persistence-boundary secret detector. */
function containsLikelySecret(content: string): boolean {
  const lower = content.toLowerCase();
  if (lower.split(/\r?\n/u).some((line) => {
    const trimmed = line.trim();
    return (trimmed.startsWith("-----begin ") && trimmed.endsWith("private key-----"))
      || trimmed.startsWith("authorization:")
      || trimmed.startsWith("authorization=")
      || [...trimmed.matchAll(/bearer ([a-z0-9._~+/-]{12,128})/giu)]
        .some((match) => /[^a-z]/iu.test(match[1]!));
  })) return true;
  if (content.split(/\s+/u).some((word) => {
    const match = word.match(/^[a-z][a-z0-9+.-]*:\/\/([^/?#]+)(?:[/?#]|$)/iu);
    if (!match) return false;
    const at = match[1]!.lastIndexOf("@");
    if (at < 0) return false;
    const credentials = match[1]!.slice(0, at);
    const separator = credentials.indexOf(":");
    return separator > 0 && separator < credentials.length - 1;
  })) return true;
  for (const prefix of SECRET_PREFIXES) {
    let offset = 0;
    while ((offset = lower.indexOf(prefix, offset)) >= 0) {
      const previous = offset === 0 ? "" : lower[offset - 1]!;
      const suffix = lower.slice(offset + prefix.length).match(/^[a-z0-9_-]{12,16}/u)?.[0];
      if ((!previous || !/[a-z0-9_]/u.test(previous)) && (suffix?.length ?? 0) >= 12) return true;
      offset += prefix.length;
    }
  }
  if (content.split(/\s+/u).some((word) => {
    const candidate = word.replace(/^[^A-Za-z0-9_.-]+|[^A-Za-z0-9_.-]+$/gu, "");
    const segments = candidate.split(".");
    return segments.length === 3 && segments[0]!.startsWith("eyJ")
      && segments.every((segment) => segment.length >= 8 && /^[A-Za-z0-9_-]+$/u.test(segment));
  })) return true;
  return SECRET_ASSIGNMENT_NAMES.some((name) => {
    let offset = 0;
    while ((offset = lower.indexOf(name, offset)) >= 0) {
      const remainder = lower.slice(offset + name.length).trimStart();
      if ((remainder.startsWith("=") || remainder.startsWith(":"))
        && remainder.slice(1).trimStart().length > 0) return true;
      offset += name.length;
    }
    return false;
  });
}

function interleaveRankedRows(
  primary: readonly RankedMemoryTurnRow[],
  secondary: readonly RankedMemoryTurnRow[],
  limit: number,
): RankedMemoryTurnRow[] {
  const rows: RankedMemoryTurnRow[] = [];
  const seen = new Set<string>();
  const append = (row: RankedMemoryTurnRow | undefined) => {
    if (row === undefined || seen.has(row.segment_id) || rows.length >= limit) return;
    seen.add(row.segment_id);
    rows.push(row);
  };
  for (let index = 0; rows.length < limit
    && (index < primary.length || index < secondary.length); index += 1) {
    append(primary[index]);
    append(secondary[index]);
  }
  return rows;
}

function normalizedScore(rank: number): number {
  if (rank <= -1) return Math.min(1, Math.max(0, -rank));
  if (rank < 0) return Math.min(1, Math.max(0, -rank / (1 - rank)));
  return 0;
}

function snippet(content: string, query: string): string {
  const compact = content.replace(/\s+/g, " ").trim();
  if (compact.length <= 360) return compact;
  const lowered = compact.toLocaleLowerCase();
  const match = historySearchTerms(query).reduce((earliest, term) => {
    const index = lowered.indexOf(term);
    return index < 0 ? earliest : Math.min(earliest, index);
  }, Number.POSITIVE_INFINITY);
  const start = Number.isFinite(match) ? Math.max(0, match - 120) : 0;
  const end = Math.min(compact.length, start + 358);
  return `${start > 0 ? "…" : ""}${compact.slice(start, end).trim()}${end < compact.length ? "…" : ""}`;
}

function retryDelayMs(attempt: number): number {
  return Math.min(MAX_AI_RETRY_DELAY_MS, 1_000 * (2 ** Math.max(0, attempt - 1)));
}

async function parseJsonBody<Value>(request: Request): Promise<Value> {
  const encoded = await request.text();
  if (new TextEncoder().encode(encoded).byteLength > MAX_BODY_BYTES) {
    throw new HistorySearchError(413, "request_too_large", "memory request exceeds 2 MiB");
  }
  try {
    return JSON.parse(encoded) as Value;
  } catch {
    throw new HistorySearchError(400, "invalid_json", "request body must be JSON");
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
