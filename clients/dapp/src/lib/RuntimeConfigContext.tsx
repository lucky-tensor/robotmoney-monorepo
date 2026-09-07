// Canonical: docs/architecture.md §5.3 — Human Dapp (runtime configuration)

/**
 * React seam for the fetched runtime config (issue #1356).
 *
 * Before this issue, `wagmi.ts` computed `targetChainId` / `targetRpcUrl`
 * once at module scope from a build-time `import.meta.env` read, and any
 * component could import those constants directly. With the devnet RPC URL
 * arriving from a fetch, those values are no longer knowable at import time,
 * so the config is published through a context instead — the same shape
 * `ExplorerContext` and `VaultRegistryContext` already use.
 *
 * The default value is an empty config, which resolves to "no devnet chain
 * configured" — identical to a bundle built without `VITE_DEVNET_RPC_URL`.
 * That makes the hook safe in component tests that render without a
 * provider, with no test-only branch in the source.
 */
import { createContext, useContext, type ReactNode } from "react";
import type { RuntimeConfig } from "./runtimeConfig";

const EMPTY_CONFIG: RuntimeConfig = {};

const RuntimeConfigContext = createContext<RuntimeConfig>(EMPTY_CONFIG);

export function RuntimeConfigProvider(props: {
  readonly config: RuntimeConfig;
  readonly children: ReactNode;
}) {
  return (
    <RuntimeConfigContext.Provider value={props.config}>
      {props.children}
    </RuntimeConfigContext.Provider>
  );
}

/**
 * The active runtime config. Returns an empty config when no provider is
 * mounted, which every consumer already treats as "unconfigured".
 */
export function useRuntimeConfig(): RuntimeConfig {
  return useContext(RuntimeConfigContext);
}
