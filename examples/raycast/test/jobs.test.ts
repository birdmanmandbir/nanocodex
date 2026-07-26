import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import {
  BackgroundJobStore,
  deserializeConversation,
  isTerminalJob,
  newBackgroundJobSubmission,
  serializeConversation,
} from "../src/jobs.ts";

test("background jobs durably retain prompt and transcript state", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-raycast-jobs-"));
  try {
    const store = new BackgroundJobStore(directory);
    const created = await store.create({
      prompt: "  keep running after the view closes  ",
      workspace: "/tmp/workspace",
    });

    assert.equal(created.prompt, "keep running after the view closes");
    assert.equal(created.status, "queued");
    assert.equal(created.terminal.pendingTurns, 1);
    assert.deepEqual(
      created.terminal.entries.map((entry) =>
        entry.kind === "user" ? entry.text : undefined,
      ),
      ["keep running after the view closes"],
    );

    const running = await store.save({
      ...created,
      status: "running",
      statusDetail: "Thinking...",
      terminal: {
        ...created.terminal,
        running: true,
        status: "Thinking...",
      },
    });
    const loaded = await store.read(created.id);
    assert.deepEqual(loaded, running);
    assert.equal(isTerminalJob(running), false);

    const completed = await store.save({
      ...running,
      status: "completed",
      statusDetail: "Completed",
      completedAt: new Date().toISOString(),
    });
    assert.equal(isTerminalJob(completed), true);
    assert.deepEqual(
      (await store.list()).map((job) => job.id),
      [created.id],
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("background cancellation is a separate cross-command signal", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-raycast-jobs-"));
  try {
    const store = new BackgroundJobStore(directory);
    const job = await store.create({
      prompt: "cancel me",
      workspace: "/tmp/workspace",
    });
    assert.equal(await store.cancellationRequested(job.id), false);
    await store.requestCancellation(job.id);
    assert.equal(await store.cancellationRequested(job.id), true);
    await store.clearCancellation(job.id);
    assert.equal(await store.cancellationRequested(job.id), false);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("native launch retries enqueue one job idempotently", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nanocodex-raycast-jobs-"));
  try {
    const store = new BackgroundJobStore(directory);
    const submission = newBackgroundJobSubmission({
      prompt: "one native handoff",
      workspace: "/tmp/workspace",
    });
    const [left, right] = await Promise.all([
      store.enqueue(submission),
      store.enqueue(submission),
    ]);
    assert.equal(left.id, submission.id);
    assert.equal(right.id, submission.id);
    assert.deepEqual(
      (await store.list()).map((job) => job.id),
      [submission.id],
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("conversation references survive the JSON job boundary", () => {
  const createdAt = new Date("2026-07-25T12:00:00.000Z");
  const updatedAt = new Date("2026-07-25T12:01:00.000Z");
  const saved = {
    id: "01983f23-6f2f-7d98-8b1d-15c0ef80536c",
    path: "/tmp/rollout.jsonl",
    title: "Background test",
    cwd: "/tmp/workspace",
    source: "cli",
    createdAt,
    updatedAt,
    archived: false,
    size: 42,
  };
  assert.deepEqual(
    deserializeConversation(serializeConversation(saved)),
    saved,
  );
});
