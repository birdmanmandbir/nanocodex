import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";

import type { MemoryRecord, MemoryResult } from "../src/durable-memory";
import type { Env } from "../src/index";

const testEnv = env as unknown as Env;

describe("account-owned durable memory scope", () => {
  it("enforces Tact-compatible scan/read/put/delete semantics", async () => {
    const ownerId = crypto.randomUUID();
    const scope = testEnv.NANOCODEX_MEMORY.getByName(ownerId);
    const ownerHeaders = {
      "content-type": "application/json",
      "x-nanocodex-owner-id": ownerId,
    };
    expect((await scope.fetch("https://memory.internal/initialize", {
      method: "PUT",
      headers: { "x-nanocodex-owner-id": ownerId },
    })).status).toBe(204);
    expect((await scope.fetch("https://memory.internal/memory")).status).toBe(404);

    const first = await operation(scope, ownerHeaders, {
      operation: "put",
      content: "Prefer rust sqlite invariant reviews.",
    });
    const second = await operation(scope, ownerHeaders, {
      operation: "put",
      content: "Rust sqlite notes with unrelated padding words.",
    });
    expect(first).toMatchObject({ operation: "put", replaced: false });
    expect(second).toMatchObject({ operation: "put", replaced: false });
    const firstRecord = (first as Extract<MemoryResult, { operation: "put" }>).memory;

    const duplicate = await rawOperation(scope, ownerHeaders, {
      operation: "put",
      content: "  prefer RUST sqlite invariant reviews.  ",
    });
    expect(duplicate.status).toBe(409);
    expect(await duplicate.json()).toMatchObject({ error: "memory_duplicate" });

    const scan = await operation(scope, ownerHeaders, {
      operation: "scan",
      query: "rust sqlite invariant",
      limit: 1,
    });
    expect(scan).toMatchObject({
      operation: "scan",
      abstained: false,
      candidates: [{ key: firstRecord.key }],
    });

    const read = await operation(scope, ownerHeaders, {
      operation: "read",
      keys: [firstRecord.key, firstRecord.key, { ...firstRecord.key, version: 99 }],
    });
    expect(read).toMatchObject({
      operation: "read",
      memories: [{
        key: firstRecord.key,
        use_count: 1,
        probation_until_ms: null,
      }],
    });

    const replaced = await operation(scope, ownerHeaders, {
      operation: "put",
      content: "Prefer rust sqlite transaction reviews.",
      replace: firstRecord.key,
    });
    const replacement = (replaced as Extract<MemoryResult, { operation: "put" }>).memory;
    expect(replacement).toMatchObject({
      key: { id: firstRecord.key.id, version: firstRecord.key.version + 1 },
      created_at_ms: firstRecord.created_at_ms,
      scan_count: 0,
      use_count: 0,
      last_scanned_at_ms: null,
      last_used_at_ms: null,
    });
    const staleReplacement = await rawOperation(scope, ownerHeaders, {
      operation: "put",
      content: "stale replacement",
      replace: firstRecord.key,
    });
    expect(staleReplacement.status).toBe(409);
    expect(await staleReplacement.json()).toMatchObject({ error: "memory_conflict" });

    const staleDelete = await rawOperation(scope, ownerHeaders, {
      operation: "delete",
      key: firstRecord.key,
    });
    expect(staleDelete.status).toBe(409);
    expect(await staleDelete.json()).toMatchObject({ error: "memory_conflict" });
    expect(await operation(scope, ownerHeaders, {
      operation: "delete",
      key: replacement.key,
    })).toEqual({ operation: "delete", key: replacement.key });
    expect(await operation(scope, ownerHeaders, {
      operation: "delete",
      key: replacement.key,
    })).toEqual({ operation: "delete", key: replacement.key });

    const rejectedSecret = await rawOperation(scope, ownerHeaders, {
      operation: "put",
      content: "Authorization: Bearer abcdefghijk1234",
    });
    expect(rejectedSecret.status).toBe(422);
    expect(await rejectedSecret.json()).toMatchObject({ error: "memory_secret_rejected" });

    const listed = await scope.fetch("https://memory.internal/memory", { headers: ownerHeaders });
    const body = await listed.json<{ memories: MemoryRecord[] }>();
    expect(body.memories).toHaveLength(1);
    expect(body.memories[0]?.key).toEqual(
      (second as Extract<MemoryResult, { operation: "put" }>).memory.key,
    );
  });
});

function rawOperation(
  scope: DurableObjectStub,
  headers: HeadersInit,
  body: unknown,
): Promise<Response> {
  return scope.fetch("https://memory.internal/memory", {
    method: "POST",
    headers,
    body: JSON.stringify(body),
  });
}

async function operation(
  scope: DurableObjectStub,
  headers: HeadersInit,
  body: unknown,
): Promise<MemoryResult> {
  const response = await rawOperation(scope, headers, body);
  expect(response.status).toBe(200);
  return response.json<MemoryResult>();
}
