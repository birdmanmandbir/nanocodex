import assert from "node:assert/strict";
import { test } from "node:test";

import {
  prepareTempoWallet,
  type CliProvider,
  type WalletOperations,
} from "../src/wallet.ts";

test("a published persisted key skips wallet authorization", async () => {
  const calls: string[] = [];
  const provider = fakeProvider("published");
  await prepareTempoWallet(provider, () => calls.push("status"), {
    async hydrate() {
      calls.push("hydrate");
    },
    async balance() {
      calls.push("balance");
      return 0n;
    },
    async authorize() {
      calls.push("authorize");
    },
  });

  assert.deepEqual(calls, ["hydrate"]);
});

test("a missing key checks the persisted root balance before requesting a deposit", async () => {
  const authorization: boolean[] = [];
  const operations: WalletOperations = {
    async hydrate() {},
    async balance() {
      return 1_000_000n;
    },
    async authorize(_provider, needsFunding) {
      authorization.push(needsFunding);
    },
  };

  await prepareTempoWallet(
    fakeProvider("missing"),
    () => undefined,
    operations,
  );
  assert.deepEqual(authorization, [false]);
});

test("a disconnected wallet requests funding with its authorization", async () => {
  const authorization: boolean[] = [];
  const provider = fakeProvider("missing", true);
  await prepareTempoWallet(provider, () => undefined, {
    async hydrate() {},
    async balance() {
      throw new Error("balance should not be queried without an account");
    },
    async authorize(_provider, needsFunding) {
      authorization.push(needsFunding);
    },
  });

  assert.deepEqual(authorization, [true]);
});

function fakeProvider(
  status: "missing" | "pending" | "published" | "expired",
  disconnected = false,
): CliProvider {
  return {
    async getAccessKeyStatus() {
      return status;
    },
    getAccount() {
      if (disconnected) throw new Error("disconnected");
      return { address: "0x0000000000000000000000000000000000000001" };
    },
  } as unknown as CliProvider;
}
