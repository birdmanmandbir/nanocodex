import assert from "node:assert/strict";
import { test } from "node:test";

import { CiRepository, type CiRunRecord } from "./ciRepository.ts";
import type { CiSourcePublication } from "./ciSource.ts";

const headA = "a".repeat(40);
const headB = "b".repeat(40);

test("CI publication CAS commits source and one durable dispatch outbox", async () => {
  const memory = repository();
  const accepted = await publish(memory.durable, null, publication(headA));
  assert.equal(accepted.status, 202);
  assert.ok(memory.alarm > 0);

  const stale = await publish(memory.durable, null, publication(headB));
  assert.equal(stale.status, 409);
  assert.deepEqual(await stale.json(), { error: "publication_conflict", currentHead: headA });

  const replay = await publish(memory.durable, null, publication(headA));
  assert.equal(replay.status, 200);
  assert.equal((await replay.json() as { run: CiRunRecord }).run.workflowId, `ci-${headA}`);

  await memory.durable.alarm();
  assert.equal(memory.created, 1);
  assert.deepEqual(memory.createdParams?.providerData, {
    archiveKey: `sources/${headA}/source.tar.gz`,
    archiveSha256: "1".repeat(64),
    archiveSize: 123,
    treeKey: `sources/${headA}/tree.json`,
    treeSha256: "2".repeat(64),
    cargoLockBlob: "c".repeat(40),
    cargoVendorKey: `cargo-vendor/${"c".repeat(40)}/bundle.tar.gz`,
    cargoVendorSha256: "3".repeat(64),
    cargoVendorSize: 4_000_000,
    rustSecRevision: "d".repeat(40),
    rustSecKey: `rustsec-advisory-db/${"d".repeat(40)}/bundle.tar.gz`,
    rustSecSha256: "4".repeat(64),
    rustSecSize: 1_400_000,
  });
  const state = await memory.durable.fetch(new Request("https://ci.test/state"));
  assert.equal(state.status, 200);
  const body = await state.json() as { publication: CiSourcePublication; run: CiRunRecord };
  assert.equal(body.publication.head, headA);
  assert.equal(body.run.state, "dispatched");
  assert.equal(body.run.attempts, 1);
});

test("a lost Workflow create acknowledgement retries the deterministic ID", async () => {
  const memory = repository({ failAfterCreate: true });
  await publish(memory.durable, null, publication(headA));
  await memory.durable.alarm();
  assert.equal(memory.created, 1);
  const run = await memory.durable.fetch(new Request(`https://ci.test/runs/${headA}`));
  assert.equal((await run.json() as CiRunRecord).state, "dispatched");
});

test("a failing dispatch does not block a later publication", async () => {
  const memory = repository({ failHead: headA });
  await publish(memory.durable, null, publication(headA));
  await publish(memory.durable, headA, publication(headB));

  await memory.durable.alarm();
  const failed = await memory.durable.fetch(new Request(`https://ci.test/runs/${headA}`));
  assert.equal((await failed.json() as CiRunRecord).state, "pending");

  await memory.durable.alarm();
  const dispatched = await memory.durable.fetch(new Request(`https://ci.test/runs/${headB}`));
  assert.equal((await dispatched.json() as CiRunRecord).state, "dispatched");
});

test("retention never deletes active or current publications", async () => {
  const memory = repository();
  let expected: string | null = null;
  for (let index = 1; index <= 105; index++) {
    const next = index.toString(16).padStart(40, "0");
    assert.equal((await publish(memory.durable, expected, publication(next))).status, 202);
    await memory.durable.alarm();
    expected = next;
  }
  const state = await memory.durable.fetch(new Request("https://ci.test/state"));
  assert.equal((await state.json() as { publication: CiSourcePublication }).publication.head, expected);
  const runs = await memory.durable.fetch(new Request("https://ci.test/runs"));
  assert.equal((await runs.json() as { runs: CiRunRecord[] }).runs.length, 50);
  assert.equal(
    (await memory.durable.fetch(new Request(`https://ci.test/runs/${"1".padStart(40, "0")}`))).status,
    200,
  );
});

test("retention removes only the oldest terminal publication", async () => {
  const memory = repository();
  let expected: string | null = null;
  for (let index = 1; index <= 101; index++) {
    const next = index.toString(16).padStart(40, "0");
    assert.equal((await publish(memory.durable, expected, publication(next))).status, 202);
    await memory.durable.alarm();
    if (index === 1) memory.complete(next);
    expected = next;
  }
  const oldest = "1".padStart(40, "0");
  assert.equal(
    (await memory.durable.fetch(new Request(`https://ci.test/runs/${oldest}`))).status,
    404,
  );
  assert.equal(
    (await memory.durable.fetch(new Request(`https://ci.test/publications/${oldest}`))).status,
    404,
  );
  assert.deepEqual(memory.sourceDeletes, [
    `sources/${oldest}/source.tar.gz`,
    `sources/${oldest}/tree.json`,
  ]);
  const state = await memory.durable.fetch(new Request("https://ci.test/state"));
  assert.equal((await state.json() as { publication: CiSourcePublication }).publication.head, expected);
});

function repository(options: { failAfterCreate?: boolean; failHead?: string } = {}) {
  const values = new Map<string, unknown>();
  const workflows = new Map<string, { status: string }>();
  let alarm = -1;
  let created = 0;
  let createdParams: Record<string, unknown> | undefined;
  const sourceDeletes: string[] = [];
  let failAfterCreate = options.failAfterCreate === true;
  const operations = {
    get: async <T>(key: string) => structuredClone(values.get(key)) as T | undefined,
    put: async (keyOrEntries: string | Record<string, unknown>, value?: unknown) => {
      if (typeof keyOrEntries === "string") values.set(keyOrEntries, structuredClone(value));
      else for (const [key, entry] of Object.entries(keyOrEntries)) {
        values.set(key, structuredClone(entry));
      }
    },
    delete: async (key: string | string[]) => {
      if (Array.isArray(key)) return key.reduce((deleted, entry) => values.delete(entry) || deleted, false);
      return values.delete(key);
    },
    list: async <T>({ prefix = "", limit }: { prefix?: string; limit?: number } = {}) => {
      const entries = [...values.entries()].filter(([key]) => key.startsWith(prefix));
      return new Map((limit === undefined ? entries : entries.slice(0, limit))
        .map(([key, value]) => [key, structuredClone(value) as T]));
    },
    setAlarm: async (timestamp: number) => { alarm = timestamp; },
  };
  const state = {
    storage: {
      ...operations,
      transaction: async <T>(callback: (transaction: typeof operations) => Promise<T>) => callback(operations),
    },
  } as unknown as DurableObjectState;
  const env = {
    CI_SOURCE: {
      async delete(keys: string | string[]) {
        sourceDeletes.push(...(Array.isArray(keys) ? keys : [keys]));
      },
    },
    CI_WORKFLOW: {
      async get(id: string) {
        return {
          id,
          status: async () => workflows.get(id) ?? { status: "unknown" },
        };
      },
      async createBatch([{ id, params }]: Array<{ id: string; params: Record<string, unknown> }>) {
        if (id === `ci-${options.failHead}`) throw new Error("permanent dispatch failure");
        if (workflows.has(id)) return [];
        created += 1;
        createdParams = structuredClone(params);
        workflows.set(id, { status: "queued" });
        if (failAfterCreate) {
          failAfterCreate = false;
          throw new Error("lost acknowledgement");
        }
        return [{ id }];
      },
    },
  };
  const durable = new CiRepository(state, env as never);
  return {
    durable,
    complete(head: string) { workflows.set(`ci-${head}`, { status: "complete" }); },
    get sourceDeletes() { return sourceDeletes; },
    get alarm() { return alarm; },
    get created() { return created; },
    get createdParams() { return createdParams; },
  };
}

function publish(
  durable: CiRepository,
  expectedHead: string | null,
  next: CiSourcePublication,
) {
  return durable.fetch(new Request("https://ci.test/publications", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ expectedHead, publication: next }),
  }));
}

function publication(head: string): CiSourcePublication {
  return {
    version: 1,
    head,
    branch: "master",
    ref: "refs/heads/master",
    archive: {
      key: `sources/${head}/source.tar.gz`,
      size: 123,
      sha256: "1".repeat(64),
    },
    tree: {
      key: `sources/${head}/tree.json`,
      size: 456,
      sha256: "2".repeat(64),
    },
    cargoLockBlob: "c".repeat(40),
    cargoVendor: {
      key: `cargo-vendor/${"c".repeat(40)}/bundle.tar.gz`,
      size: 4_000_000,
      sha256: "3".repeat(64),
    },
    rustSecRevision: "d".repeat(40),
    rustSec: {
      key: `rustsec-advisory-db/${"d".repeat(40)}/bundle.tar.gz`,
      size: 1_400_000,
      sha256: "4".repeat(64),
    },
    publishedAt: "2026-08-21T00:00:00.000Z",
  };
}
