import { describe, expect, it, vi } from "vitest";

vi.mock("cloudflare:workers", () => ({
  DurableObject: class {
    ctx: unknown;
    constructor(ctx: unknown) { this.ctx = ctx; }
  },
}));

import type { LaunchTicketClaims } from "../src/auth";
import { LaunchTicketStore } from "../src/launch-tickets";

const claims = {
  actorUserId: "0198e2c4-365e-7a66-a58f-d4e5b46a7dad",
  appId: "0198e2c4-365e-7a66-a58f-d4e5b46a7dae",
  audience: "nanocodex-app-launch",
  expiry: Math.floor(Date.now() / 1_000) + 60,
  nonce: "abcdefghijklmnopqrstuvwxyz",
  slug: "private-app",
  tenantId: "user:0198e2c4-365e-7a66-a58f-d4e5b46a7dad",
  transaction: "transaction-nonce-12345678",
  version: 1,
} satisfies LaunchTicketClaims;

describe("launch ticket store", () => {
  it("consumes an exact ticket only once", async () => {
    const values = new Map<string, unknown>();
    const storage: Record<string, unknown> & {
      transaction<T>(operation: (storage: DurableObjectStorage) => Promise<T>): Promise<T>;
    } = {
      deleteAlarm: vi.fn(),
      deleteAll: vi.fn(async () => values.clear()),
      get: vi.fn(async (key: string) => values.get(key)),
      put: vi.fn(async (key: string, value: unknown) => values.set(key, value)),
      setAlarm: vi.fn(),
      transaction: async <T>(operation: (storage: DurableObjectStorage) => Promise<T>) => {
        return operation(storage as unknown as DurableObjectStorage);
      },
    };
    const store = new LaunchTicketStore({ storage } as unknown as DurableObjectState, {});

    await store.issue(claims);
    expect(await store.consume(claims)).toBe(true);
    expect(await store.consume(claims)).toBe(false);
  });
});
