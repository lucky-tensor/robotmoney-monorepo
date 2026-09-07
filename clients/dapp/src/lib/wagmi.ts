// Canonical: docs/architecture.md §3 — Technology Stack

/**
 * wagmi/viem client setup. The dapp uses the browser-injected EIP-1193
 * provider (MetaMask, hardware bridges, etc.) as its only wallet
 * connector AND as its read transport for every chain —
 * see `docs/technical/dapp-topology.md` §2 ("No dapp-owned RPC"). All
 * traffic for the chain the user is interacting with traverses an
 * endpoint the user chose, not one this bundle was built with.
 *
 * Exception — price-strip reads on the devnet chain: when `VITE_DEVNET_RPC_URL`
 * is configured, an HTTP transport at that URL is added as a fallback
 * (after `unstable_connector(injected)`) for the devnet chain only. This
 * allows `useReadContract` calls (e.g. Uniswap V3 slot0 reads for the price
 * strip) to succeed even when no MetaMask is configured for the devnet — which
 * is always the case in CI Playwright runs and for first-time visitors to the
 * demo URL. Wallet signing and non-price-strip writes are not affected: wagmi
 * falls back to the HTTP transport only for read calls, never for signing.
 *
 * Issue #1356: the devnet RPC URL is no longer read from `import.meta.env` at
 * module scope. It arrives in the config record this module is *given* — from
 * `/config.json` at runtime, or from the build-time env when no such document
 * is deployed (see `runtimeConfig.ts`). Nothing here is knowable at import
 * time any more, so the former `targetChainId` / `targetRpcUrl` constants are
 * now `resolveTargetChainId(env)` / `resolveTargetRpcUrl(env)`; components
 * reach the record through `useRuntimeConfig()`.
 *
 * Test harnesses inject `window.ethereum` themselves before the page
 * loads — no test-only branches in this file.
 */
import { unstable_connector, http, createConfig, fallback } from "wagmi";
import { foundry, mainnet, sepolia } from "wagmi/chains";
import { injected } from "wagmi/connectors";
import { defineChain } from "viem";

/**
 * Robot Money devnet (Geth+Lighthouse fork). Real prod-shaped chain id;
 * not the same as foundry/anvil (31337). The id is a fixed property of the
 * chain, so unlike the RPC URL it stays a module constant.
 */
export const DEVNET_CHAIN_ID = 918453;

/**
 * Build the devnet chain definition for a given RPC URL. The URL serves two
 * purposes:
 *   1. `wallet_addEthereumChain` prefills the RPC URL in the user's wallet so
 *      Connect Wallet can rotate the stored endpoint when the tunnel URL changes.
 *   2. HTTP fallback transport for read calls (price strip, block numbers) when
 *      MetaMask is not configured for the devnet — see file-level doc above.
 */
function makeDevnetChain(rpcUrl: string) {
  return defineChain({
    id: DEVNET_CHAIN_ID,
    name: "Robot Money devnet",
    nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
    rpcUrls: { default: { http: rpcUrl ? [rpcUrl] : [] } },
  });
}

function readDevnetRpcUrl(env: Record<string, string | undefined>): string {
  return env.VITE_DEVNET_RPC_URL ?? "";
}

/**
 * Chain ID the dapp will ask the user's wallet to switch to after
 * Connect. `undefined` when no `VITE_DEVNET_RPC_URL` is configured
 * (i.e. mainnet / Base operator builds) — in those, the user
 * is expected to already be on the right chain and Connect does no
 * automatic switching.
 */
export function resolveTargetChainId(env: Record<string, string | undefined>): number | undefined {
  return readDevnetRpcUrl(env) ? DEVNET_CHAIN_ID : undefined;
}

/**
 * RPC URL the dapp asks the user's wallet to associate with
 * `resolveTargetChainId(env)`. Used by the Connect Wallet flow to call
 * `wallet_addEthereumChain` every time, which is the only way to keep
 * the wallet's stored RPC URL in sync with ephemeral tunnel URLs that
 * rotate across smoke-test sessions. The dapp never fetches from this
 * URL itself — the wallet does, after the user accepts the prompt.
 */
export function resolveTargetRpcUrl(env: Record<string, string | undefined>): string | undefined {
  return readDevnetRpcUrl(env) || undefined;
}

export function makeConfig(env: Record<string, string | undefined>) {
  const devnetRpcUrl = readDevnetRpcUrl(env);
  const devnet = makeDevnetChain(devnetRpcUrl);

  // For the devnet chain: use the injected connector as the primary transport
  // so wallet signing continues to work when MetaMask is configured. Fall back
  // to an HTTP transport at `devnetRpcUrl` for read calls (price strip, block
  // numbers) when the injected connector is absent or reports the wrong chain.
  // The fallback is a no-op when `devnetRpcUrl` is empty (unconfigured devnet).
  const devnetTransport = devnetRpcUrl
    ? fallback([unstable_connector(injected), http(devnetRpcUrl)])
    : unstable_connector(injected);

  return createConfig({
    chains: [devnet, foundry, sepolia, mainnet],
    connectors: [injected()],
    transports: {
      [devnet.id]: devnetTransport,
      [foundry.id]: unstable_connector(injected),
      [sepolia.id]: unstable_connector(injected),
      [mainnet.id]: unstable_connector(injected),
    },
  });
}
