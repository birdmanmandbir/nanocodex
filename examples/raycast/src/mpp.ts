import { LocalStorage, open } from "@raycast/api";
import { Provider } from "accounts/cli";
import { createJsonChannelStore, tempo } from "mppx/client";
import { webcrypto } from "node:crypto";
import WebSocket from "ws";

import { prepareTempoWallet } from "./wallet";

const PATH_USD = "0x20c0000000000000000000000000000000000000";
const CHANNEL_KEY_PREFIX = "nanocodex-mpp-channel:";

if (!globalThis.crypto?.subtle) {
  Object.defineProperty(globalThis, "crypto", {
    configurable: true,
    value: webcrypto,
  });
}

export type MppSetup = {
  manager: ReturnType<typeof tempo.session.manager>;
};

type SessionManagerOptions = Parameters<typeof tempo.session.manager>[0];
const MppWebSocket = WebSocket as unknown as NonNullable<
  SessionManagerOptions["webSocket"]
>;

export async function createMppSession(
  setStatus: (status: string) => void,
): Promise<MppSetup> {
  setStatus("Loading Tempo Wallet…");
  const provider = Provider.create({
    mpp: false,
    open(url) {
      setStatus("Authorize the scoped Tempo key in your browser…");
      void open(url);
    },
    timeoutMs: 10 * 60 * 1_000,
  });
  await prepareTempoWallet(provider, setStatus);

  setStatus("Opening the MPP session…");
  const manager = tempo.session.manager({
    ...provider.getMppxParameters(),
    autoSwap: { tokenIn: [PATH_USD], slippage: 1 },
    bootstrap: true,
    channelStore: raycastChannelStore(),
    webSocket: MppWebSocket,
    // maxDeposit caps cumulative vouchers, not an individual top-up.
    maxDeposit: "1",
    topUpAmount: "0.05",
  });

  return { manager };
}

function raycastChannelStore() {
  return createJsonChannelStore({
    async get(key) {
      const value = await LocalStorage.getItem<string>(
        `${CHANNEL_KEY_PREFIX}${key}`,
      );
      return typeof value === "string" ? value : undefined;
    },
    async set(key, value) {
      await LocalStorage.setItem(`${CHANNEL_KEY_PREFIX}${key}`, value);
    },
    async delete(key) {
      await LocalStorage.removeItem(`${CHANNEL_KEY_PREFIX}${key}`);
    },
  });
}
