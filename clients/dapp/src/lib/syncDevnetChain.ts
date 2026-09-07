// Canonical: docs/architecture.md §3 — Technology Stack

/**
 * Force the wallet's stored RPC URL for the target chain to match the
 * one this deployment is configured with. Calling `wallet_addEthereumChain`
 * unconditionally is the only reliable way to do this: when the wallet
 * already has the chain but with a different (stale) RPC URL, the
 * standard `wallet_switchEthereumChain` is a no-op for the URL, so the
 * wallet keeps using whatever it had — typically the previous
 * smoke-test session's now-dead tunnel URL.
 *
 * If the URL is unchanged, most wallets dedupe and the user sees no
 * prompt; if it differs, the user sees a single confirmation and the
 * wallet adopts the new URL. Then we switch into the chain.
 *
 * Issue #1356: the chain id, RPC URL, and block-explorer URL all come from
 * the `env` argument — the runtime config record — rather than from a
 * module-scope `import.meta.env` read, so one bundle can target whichever
 * devnet the deployment points it at. Callers obtain the record from
 * `useRuntimeConfig()`.
 */
import { resolveTargetChainId, resolveTargetRpcUrl } from "./wagmi";

interface Eip1193Provider {
  request: (args: { method: string; params?: unknown[] }) => Promise<unknown>;
}

export function getInjectedProvider(): Eip1193Provider | undefined {
  if (typeof window === "undefined") return undefined;
  return (window as unknown as { ethereum?: Eip1193Provider }).ethereum;
}

export async function syncDevnetChain(
  provider: Eip1193Provider,
  env: Record<string, string | undefined>,
): Promise<string | undefined> {
  const targetChainId = resolveTargetChainId(env);
  const targetRpcUrl = resolveTargetRpcUrl(env);
  if (targetChainId === undefined || !targetRpcUrl) {
    return "VITE_DEVNET_RPC_URL is not set in this deployment — auto network add is disabled.";
  }
  const chainIdHex = `0x${targetChainId.toString(16)}`;
  let addError: unknown;
  try {
    await provider.request({
      method: "wallet_addEthereumChain",
      params: [
        {
          chainId: chainIdHex,
          chainName: "Robot Money devnet",
          rpcUrls: [targetRpcUrl],
          nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
          // MetaMask v11+ sometimes refuses the add without a block
          // explorer entry; the explorer-api URL serves as a sensible
          // stand-in even though it isn't a block-explorer UI.
          blockExplorerUrls: [String(env.VITE_EXPLORER_API_URL ?? targetRpcUrl)],
        },
      ],
    });
  } catch (err) {
    addError = err;
    console.error("wallet_addEthereumChain failed:", err);
  }
  try {
    await provider.request({
      method: "wallet_switchEthereumChain",
      params: [{ chainId: chainIdHex }],
    });
    return undefined;
  } catch (err) {
    console.error("wallet_switchEthereumChain failed:", err);
    const addMsg = addError ? `add: ${(addError as { message?: string }).message ?? addError}` : "";
    const switchMsg = `switch: ${(err as { message?: string }).message ?? err}`;
    return [addMsg, switchMsg].filter(Boolean).join(" — ");
  }
}
