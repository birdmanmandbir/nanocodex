import { Expiry, Store } from "accounts";
import { Provider } from "accounts/cli";
import { parseUnits } from "viem";
import { connect } from "viem/experimental/erc7846";
import { Actions } from "viem/tempo";

const PATH_USD = "0x20c0000000000000000000000000000000000000";
const USDC_E = "0x20c000000000000000000000b9537d11c60e8b50";
const MINIMUM_PATH_USD = parseUnits("0.1", 6);
const ACCESS_KEY_LIMIT = parseUnits("5", 6);

export type CliProvider = ReturnType<typeof Provider.create>;

export type WalletOperations = {
  hydrate(provider: CliProvider): Promise<void>;
  balance(provider: CliProvider): Promise<bigint>;
  authorize(provider: CliProvider, needsFunding: boolean): Promise<void>;
};

const walletOperations: WalletOperations = {
  async hydrate(provider) {
    await Store.waitForHydration(provider.store);
  },
  async balance(provider) {
    const balance = await Actions.token.getBalance(provider.getClient(), {
      account: provider.getAccount(),
      token: PATH_USD,
    });
    return balance.amount;
  },
  async authorize(provider, needsFunding) {
    await connect(provider.getClient(), {
      capabilities: {
        authorizeAccessKey: {
          expiry: Expiry.days(1),
          limits: [
            { token: PATH_USD, limit: ACCESS_KEY_LIMIT },
            { token: USDC_E, limit: ACCESS_KEY_LIMIT },
          ],
        },
        ...(needsFunding
          ? { showDeposit: { amount: "0.25", token: "pathUSD" } }
          : {}),
      },
    });
  },
};

export async function prepareTempoWallet(
  provider: CliProvider,
  setStatus: (status: string) => void,
  operations: WalletOperations = walletOperations,
): Promise<void> {
  await operations.hydrate(provider);
  const accessKeyStatus = await provider.getAccessKeyStatus();
  if (accessKeyStatus !== "missing" && accessKeyStatus !== "expired") return;

  let hasAccount = true;
  try {
    provider.getAccount();
  } catch {
    hasAccount = false;
  }
  const balance = hasAccount ? await operations.balance(provider) : undefined;
  const needsFunding = balance === undefined || balance < MINIMUM_PATH_USD;

  setStatus("Waiting for Tempo Wallet authorization…");
  await operations.authorize(provider, needsFunding);
}
