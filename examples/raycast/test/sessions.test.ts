import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import {
  appendFile,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  stat,
  utimes,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { SessionSnapshot } from "nanocodex/node";

import {
  CodexRolloutJournal,
  findCompletedTurnMessage,
  listSavedConversations,
  loadSavedConversation,
  type SavedConversation,
} from "../src/sessions.ts";

test("completed JS/WASM turns round-trip through one Codex rollout", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-raycast-"));
  try {
    const threadId = randomUUID();
    const windowId = randomUUID();
    const path = join(directory, `rollout-test-${threadId}.jsonl`);
    const metadata = {
      timestamp: new Date().toISOString(),
      type: "session_meta",
      payload: {
        session_id: threadId,
        id: threadId,
        timestamp: new Date().toISOString(),
        cwd: process.cwd(),
        source: "cli",
        base_instructions: { text: "test instructions" },
        context_window: { window_id: windowId },
      },
    };
    const initial = `${JSON.stringify(metadata)}\n`;
    await writeFile(path, initial, { encoding: "utf8", mode: 0o600 });
    const journal = new CodexRolloutJournal({
      threadId,
      path,
      expectedSize: Buffer.byteLength(initial),
      history: [],
      windowNumber: 0,
      firstWindowId: windowId,
      currentWindowId: windowId,
    });
    const canonicalContext = userMessage("first");
    const firstHistory = [
      canonicalContext,
      assistantMessage("one"),
    ] satisfies Record<string, unknown>[];
    const first = snapshot(threadId, canonicalContext, firstHistory);
    const firstTurnId = randomUUID();
    await journal.appendTurn({
      turnId: firstTurnId,
      prompt: "first",
      finalMessage: "one",
      snapshot: first,
      compacted: false,
      startedAt: Date.now() - 10,
    });
    assert.equal(await findCompletedTurnMessage(path, firstTurnId), "one");
    assert.equal(await findCompletedTurnMessage(path, randomUUID()), undefined);

    const secondHistory = [
      ...firstHistory,
      userMessage("second"),
      assistantMessage("two"),
    ];
    await journal.appendTurn({
      prompt: "second",
      finalMessage: "two",
      snapshot: snapshot(threadId, canonicalContext, secondHistory),
      compacted: false,
      startedAt: Date.now() - 10,
    });

    const file = await stat(path);
    const saved: SavedConversation = {
      id: threadId,
      path,
      title: "test",
      cwd: process.cwd(),
      source: "cli",
      createdAt: file.birthtime,
      updatedAt: file.mtime,
      archived: false,
      size: file.size,
    };
    const loaded = await loadSavedConversation(saved);
    assert.equal(loaded.snapshot.history.length, 4);
    assert.deepEqual(
      loaded.transcript.entries
        .filter((entry) => entry.kind === "user" || entry.kind === "assistant")
        .map((entry) => ("text" in entry ? entry.text : "")),
      ["first", "one", "second", "two"],
    );

    const normalizedHistory = [
      ...secondHistory.slice(0, -1),
      assistantMessage("normalized two"),
      userMessage("third"),
      assistantMessage("three"),
    ];
    await loaded.journal.appendTurn({
      prompt: "third",
      finalMessage: "three",
      snapshot: snapshot(threadId, canonicalContext, normalizedHistory),
      compacted: false,
      startedAt: Date.now() - 10,
    });
    const persisted = (await readFile(path, "utf8"))
      .split("\n")
      .flatMap((line) => (line ? [JSON.parse(line)] : []));
    const compaction = persisted.findLast(
      (record) => record.type === "compacted",
    );
    assert.deepEqual(
      compaction?.payload.replacement_history,
      normalizedHistory,
    );

    await appendFile(path, "{}\n");
    await assert.rejects(
      journal.appendTurn({
        prompt: "third",
        finalMessage: "three",
        snapshot: snapshot(threadId, canonicalContext, secondHistory),
        compacted: false,
        startedAt: Date.now(),
      }),
      /changed in another process/,
    );
  } finally {
    await rm(directory, { force: true, recursive: true });
  }
});

test("conversation discovery keeps only real interactive threads before applying its limit", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-discovery-"));
  const previousCodexHome = process.env.CODEX_HOME;
  process.env.CODEX_HOME = directory;
  try {
    const sessions = join(directory, "sessions", "2026", "07", "25");
    await mkdir(sessions, { recursive: true });
    const cliId = randomUUID();
    const vscodeId = randomUUID();
    const subagentId = randomUUID();
    const emptyId = randomUUID();
    const execId = randomUUID();
    const now = Date.now();

    const subagent = await writeRollout(sessions, subagentId, [
      sessionMetadata(subagentId, process.cwd(), {
        subagent: { type: "review" },
      }),
      eventMessage("user_message", { message: "hidden subagent" }),
    ]);
    const empty = await writeRollout(sessions, emptyId, [
      sessionMetadata(emptyId, process.cwd(), "cli"),
    ]);
    const exec = await writeRollout(sessions, execId, [
      sessionMetadata(execId, process.cwd(), "exec"),
      eventMessage("user_message", { message: "hidden exec" }),
    ]);
    const cli = await writeRollout(sessions, cliId, [
      sessionMetadata(cliId, process.cwd(), "cli"),
      sessionMetadata(randomUUID(), "/tmp/parent", "cli"),
      eventMessage("user_message", { message: "visible cli thread" }),
    ]);
    const vscode = await writeRollout(sessions, vscodeId, [
      sessionMetadata(vscodeId, process.cwd(), "vscode"),
      eventMessage("user_message", { message: "visible vscode thread" }),
    ]);
    for (const [path, offset] of [
      [subagent, 5],
      [empty, 4],
      [exec, 3],
      [cli, 2],
      [vscode, 1],
    ] as const) {
      const modified = new Date(now + offset * 1_000);
      await utimes(path, modified, modified);
    }

    const conversations = await listSavedConversations(2);
    assert.deepEqual(
      conversations.map(({ id }) => id),
      [cliId, vscodeId],
    );
    assert.deepEqual(
      conversations.map(({ title }) => title),
      ["visible cli thread", "visible vscode thread"],
    );
  } finally {
    if (previousCodexHome === undefined) delete process.env.CODEX_HOME;
    else process.env.CODEX_HOME = previousCodexHome;
    await rm(directory, { force: true, recursive: true });
  }
});

test("resume applies Codex rollbacks and falls back from a deleted workspace", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-rollback-"));
  try {
    const threadId = randomUUID();
    const missingWorkspace = join(directory, "deleted-workspace");
    const windowId = randomUUID();
    const path = join(directory, `rollout-test-${threadId}.jsonl`);
    const records = [
      sessionMetadata(threadId, missingWorkspace, "cli", windowId),
      rolloutRecord("response_item", userMessage("first")),
      eventMessage("user_message", { message: "first" }),
      rolloutRecord("response_item", assistantMessage("one")),
      eventMessage("agent_message", { message: "one" }),
      rolloutRecord("response_item", userMessage("rolled back")),
      eventMessage("user_message", { message: "rolled back" }),
      rolloutRecord("response_item", assistantMessage("discard me")),
      eventMessage("agent_message", { message: "discard me" }),
      eventMessage("thread_rolled_back", { num_turns: 1 }),
      rolloutRecord("turn_context", { cwd: missingWorkspace }),
      rolloutRecord("response_item", userMessage("kept")),
      eventMessage("user_message", { message: "kept" }),
      rolloutRecord("response_item", assistantMessage("three")),
      eventMessage("agent_message", { message: "three" }),
    ];
    await writeFile(
      path,
      `${records.map((record) => JSON.stringify(record)).join("\n")}\n`,
    );
    const file = await stat(path);
    const saved: SavedConversation = {
      id: threadId,
      path,
      title: "rollback",
      cwd: missingWorkspace,
      source: "cli",
      createdAt: file.birthtime,
      updatedAt: file.mtime,
      archived: false,
      size: file.size,
    };

    const loaded = await loadSavedConversation(saved, directory);
    assert.equal(loaded.snapshot.workspace, await realpath(directory));
    assert.deepEqual(
      loaded.snapshot.history
        .filter(
          (item) =>
            item.type === "message" &&
            (item.role === "user" || item.role === "assistant"),
        )
        .map((item) => JSON.stringify(item)),
      [
        JSON.stringify(userMessage("first")),
        JSON.stringify(assistantMessage("one")),
        JSON.stringify(userMessage("kept")),
        JSON.stringify(assistantMessage("three")),
      ],
    );
    assert.deepEqual(
      loaded.transcript.entries
        .filter((entry) => entry.kind === "user" || entry.kind === "assistant")
        .map((entry) => ("text" in entry ? entry.text : "")),
      ["first", "one", "kept", "three"],
    );
  } finally {
    await rm(directory, { force: true, recursive: true });
  }
});

function snapshot(
  threadId: string,
  canonicalContext: Record<string, unknown>,
  history: readonly Record<string, unknown>[],
): SessionSnapshot {
  return {
    version: 1,
    model: "gpt-5.6-sol",
    lineage_id: threadId,
    prompt_cache_key: threadId,
    workspace: process.cwd(),
    canonical_context: canonicalContext,
    history,
  };
}

function userMessage(text: string): Record<string, unknown> {
  return {
    type: "message",
    role: "user",
    content: [{ type: "input_text", text }],
  };
}

function assistantMessage(text: string): Record<string, unknown> {
  return {
    type: "message",
    role: "assistant",
    content: [{ type: "output_text", text }],
    status: "completed",
  };
}

async function writeRollout(
  directory: string,
  threadId: string,
  records: readonly Record<string, unknown>[],
): Promise<string> {
  const path = join(directory, `rollout-2026-07-25T00-00-00-${threadId}.jsonl`);
  await writeFile(
    path,
    `${records.map((record) => JSON.stringify(record)).join("\n")}\n`,
  );
  return path;
}

function sessionMetadata(
  threadId: string,
  cwd: string,
  source: unknown,
  windowId = randomUUID(),
): Record<string, unknown> {
  return rolloutRecord("session_meta", {
    id: threadId,
    session_id: threadId,
    cwd,
    source,
    timestamp: new Date().toISOString(),
    base_instructions: { text: "test instructions" },
    context_window: { window_id: windowId },
  });
}

function eventMessage(
  type: string,
  payload: Record<string, unknown>,
): Record<string, unknown> {
  return rolloutRecord("event_msg", { type, ...payload });
}

function rolloutRecord(
  type: string,
  payload: Record<string, unknown>,
): Record<string, unknown> {
  return {
    timestamp: new Date().toISOString(),
    type,
    payload,
  };
}
