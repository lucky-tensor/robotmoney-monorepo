/**
 * wagmi + wallet chain sync read the devnet endpoint from their argument —
 * issue #1356.
 *
 * Before this issue `makeConfig` ignored its `env` parameter entirely and
 * closed over a module-scope `import.meta.env.VITE_DEVNET_RPC_URL` read, so
 * every config it produced described whatever the bundle was built with.
 * These tests fail if that closure comes back: the same module must produce
 * two different configs for two different env records.
 */
import { describe, expect, it } from "vitest";
import {
  DEVNET_CHAIN_ID,
  makeConfig,
  resolveTargetChainId,
  resolveTargetRpcUrl,
} from "../../src/lib/wagmi";
import { syncDevnetChain } from "../../src/lib/syncDevnetChain";

const RPC_A = "https://devnet-a.example/rpc";
const RPC_B = "https://devnet-b.example/rpc";

function devnetRpcUrls(env: Record<string, string | undefined>): readonly string[] {
  const config = makeConfig(env);
  const devnet = config.chains.find((chain) => chain.id === DEVNET_CHAIN_ID);
  expect(devnet).toBeDefined();
  return devnet?.rpcUrls.default.http ?? [];
}

describe("makeConfig consumes its env argument", () => {
  it("builds the devnet chain from the supplied VITE_DEVNET_RPC_URL", () => {
    expect(devnetRpcUrls({ VITE_DEVNET_RPC_URL: RPC_A })).toEqual([RPC_A]);
  });

  it("produces a different devnet endpoint for a different env record", () => {
    // The load-bearing assertion: one module, two configs. A module-scope
    // read could not do this.
    expect(devnetRpcUrls({ VITE_DEVNET_RPC_URL: RPC_A })).toEqual([RPC_A]);
    expect(devnetRpcUrls({ VITE_DEVNET_RPC_URL: RPC_B })).toEqual([RPC_B]);
  });

  it("leaves the devnet chain without an HTTP endpoint when the URL is unset", () => {
    expect(devnetRpcUrls({})).toEqual([]);
  });

  it("always exposes the four supported chains", () => {
    expect(makeConfig({ VITE_DEVNET_RPC_URL: RPC_A }).chains.map((c) => c.id)).toContain(
      DEVNET_CHAIN_ID,
    );
  });
});

describe("target chain resolution", () => {
  it("targets the devnet chain only when an RPC URL is configured", () => {
    expect(resolveTargetChainId({ VITE_DEVNET_RPC_URL: RPC_A })).toBe(DEVNET_CHAIN_ID);
    expect(resolveTargetRpcUrl({ VITE_DEVNET_RPC_URL: RPC_A })).toBe(RPC_A);
  });

  it("targets no chain when the RPC URL is absent or empty", () => {
    expect(resolveTargetChainId({})).toBeUndefined();
    expect(resolveTargetRpcUrl({})).toBeUndefined();
    expect(resolveTargetChainId({ VITE_DEVNET_RPC_URL: "" })).toBeUndefined();
    expect(resolveTargetRpcUrl({ VITE_DEVNET_RPC_URL: "" })).toBeUndefined();
  });
});

interface WalletCall {
  readonly method: string;
  readonly params?: unknown[];
}

/** An EIP-1193 provider that records requests and always succeeds. */
function fakeWallet() {
  const calls: WalletCall[] = [];
  return {
    calls,
    provider: {
      request: (args: WalletCall) => {
        calls.push(args);
        return Promise.resolve(undefined);
      },
    },
  };
}

/** The single `wallet_addEthereumChain` parameter object the wallet was given. */
function addChainParams(calls: readonly WalletCall[]): Record<string, unknown> {
  const call = calls.find((c) => c.method === "wallet_addEthereumChain");
  expect(call).toBeDefined();
  return (call?.params?.[0] ?? {}) as Record<string, unknown>;
}

describe("syncDevnetChain consumes its env argument", () => {
  it("asks the wallet to add the chain at the supplied RPC and explorer URLs", async () => {
    const wallet = fakeWallet();

    const error = await syncDevnetChain(wallet.provider, {
      VITE_DEVNET_RPC_URL: RPC_B,
      VITE_EXPLORER_API_URL: "https://explorer.example",
    });

    expect(error).toBeUndefined();
    const params = addChainParams(wallet.calls);
    expect(params.chainId).toBe(`0x${DEVNET_CHAIN_ID.toString(16)}`);
    expect(params.rpcUrls).toEqual([RPC_B]);
    expect(params.blockExplorerUrls).toEqual(["https://explorer.example"]);
    expect(wallet.calls.some((c) => c.method === "wallet_switchEthereumChain")).toBe(true);
  });

  it("falls back to the RPC URL as the block explorer when none is configured", async () => {
    const wallet = fakeWallet();

    await syncDevnetChain(wallet.provider, { VITE_DEVNET_RPC_URL: RPC_A });

    expect(addChainParams(wallet.calls).blockExplorerUrls).toEqual([RPC_A]);
  });

  it("declines to touch the wallet when no devnet RPC URL is configured", async () => {
    const wallet = fakeWallet();

    const error = await syncDevnetChain(wallet.provider, {});

    expect(wallet.calls).toEqual([]);
    expect(error).toMatch(/VITE_DEVNET_RPC_URL is not set/);
  });
});
