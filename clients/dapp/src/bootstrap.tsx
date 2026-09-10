// Canonical: docs/architecture.md §5.3 — Human Dapp (Vite entry point)

/**
 * Application tree and async startup gate.
 *
 * Split out of `main.tsx` in issue #1356. `main.tsx` is the Vite entry and
 * runs on import, which makes it untestable; everything worth asserting on
 * lives here and is exercised by `tests/unit/main-bootstrap.test.tsx`.
 *
 * ## Startup sequence
 *
 * Contract addresses and endpoints used to be `import.meta.env` reads
 * evaluated at module scope, which Vite inlined at build time — so pointing
 * the same image at another environment meant rebuilding it. They now arrive
 * from `/config.json` (see `runtimeConfig.ts`). Because that is a fetch, the
 * app cannot be rendered synchronously on import:
 *
 *   1. render an explicit loading state,
 *   2. await the config,
 *   3. render the real app with it — or a visible error panel if the config
 *      was served but unusable.
 *
 * Step 3's error branch is the point of the whole gate: a deployment with a
 * broken `/config.json` must say so, rather than presenting a blank page or a
 * silently half-configured app pointed at zero addresses.
 *
 * ## What stays build-time
 *
 * The merged config keeps the build-time env as its base layer, so
 * `VITE_FAUCET_HARNESS_PRIVATE_KEY` (faucetClient.ts),
 * `VITE_HISTORY_PANE` (featureFlags.ts),
 * `VITE_GATEWAY_EXPECTED_CODE_HASH` (gatewayVerifier.ts, via
 * `deriveDappConfig`'s `expectedCodeHash`), `VITE_FORCE_ONBOARDING`, and the
 * fork-block annotations still reach their consumers exactly as before —
 * inlined at build time, and unreachable from `/config.json` because
 * `RUNTIME_CONFIG_KEYS` does not list them.
 *
 * The code-hash pin is the load-bearing one: it is the value that decides
 * whether admin writes against the *runtime-supplied* gateway address are
 * enabled, so it has to sit inside whatever attests the bundle rather than
 * beside it. See the security note in `runtimeConfig.ts` (issue #1375).
 *
 * ## Other notes carried over from main.tsx
 *
 * VaultRegistryContext (issue #417) is mounted here as a shared
 * data-fetching seam so all downstream components receive vault metadata
 * from a single batched registry read rather than N independent chain
 * reads. See docs/technical/multi-vault-dapp-decisions.md §4.1. (The
 * router side of that seam, RouterContext, was removed in issue #1281:
 * it had zero consumers and issued a reverting read for a selector no
 * Solidity source defines; RouterDepositTab reads the router directly.)
 *
 * ExplorerProvider (ExplorerContext) is mounted here as the single polling
 * loop for the explorer API (/v1/vaults and /v1/stats). All components that
 * display TVL, stats, or block freshness consume useExplorer() instead of
 * issuing independent fetches — ensuring they always reflect the same
 * indexed block_number with no mixed-block state across components.
 */
import React, { useEffect, useMemo, useState, type ReactElement } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { WagmiProvider, useAccount } from "wagmi";
import type { Address } from "viem";
import { AgentsPanel } from "./components/AgentsPanel";
import { AccountLayerView } from "./components/AccountLayerView";
import { NavBar } from "./components/NavBar";
import { StatusHeader } from "./components/StatusHeader";
import { TestnetBanner } from "./components/TestnetBanner";
import { VerificationBanner } from "./components/VerificationBanner";
import { VaultList } from "./components/VaultList";
import { VaultDetail } from "./components/VaultDetail";
import { RouterView } from "./components/RouterView";
import { ProtocolStats } from "./components/ProtocolStats";
import { AboutModal } from "./components/AboutModal";
import { DebugPage } from "./components/DebugPage";
import { VaultCards } from "./components/VaultCards";
import { LandingPriceStrip } from "./components/LandingPriceStrip";
import { BalancesPanel } from "./components/BalancesPanel";
import { Tabs } from "./components/Tabs";
import { GovernancePanel } from "./components/GovernancePanel";
import { ConsensusReceiptPanel } from "./components/ConsensusReceiptPanel";
import { parseVaultAddressMap } from "./lib/consensusReceiptApi";
import { makeConfig } from "./lib/wagmi";
import { useGatewayVerifier } from "./lib/useGatewayVerifier";
import { resolveExplorerApiUrl } from "./lib/explorerApi";
import { VaultRegistryProvider } from "./lib/VaultRegistryContext";
import { ExplorerProvider } from "./lib/ExplorerContext";
import { RuntimeConfigProvider } from "./lib/RuntimeConfigContext";
import {
  loadRuntimeConfig,
  type ConfigFetchLike,
  type RuntimeConfig,
  type RuntimeConfigSource,
} from "./lib/runtimeConfig";

const ZERO_ADDRESS = "0x0000000000000000000000000000000000000000" as Address;

/** The deployment values the app tree needs, derived from a runtime config. */
export interface DappConfig {
  readonly env: RuntimeConfig;
  readonly gateway: Address;
  readonly vault: Address;
  readonly registry?: Address;
  readonly router?: Address;
  readonly governance?: Address;
  readonly timelock?: Address;
  readonly rmToken?: Address;
  readonly expectedCodeHash?: string;
  readonly envClass: "fork" | "devnet" | "testnet" | "mainnet";
  readonly explorerApiUrl: string;
  readonly vaultAddressBySymbol: ReturnType<typeof parseVaultAddressMap>;
}

/**
 * Project a runtime config record onto the props the app tree consumes.
 * Pure — the same record always yields the same deployment view, which is
 * what makes the fetched-config path assertable in tests.
 */
export function deriveDappConfig(env: RuntimeConfig): DappConfig {
  return {
    env,
    gateway: (env.VITE_GATEWAY_ADDRESS ?? ZERO_ADDRESS) as Address,
    vault: (env.VITE_VAULT_ADDRESS ?? ZERO_ADDRESS) as Address,
    registry: env.VITE_REGISTRY_ADDRESS ? (env.VITE_REGISTRY_ADDRESS as Address) : undefined,
    router: env.VITE_ROUTER_ADDRESS ? (env.VITE_ROUTER_ADDRESS as Address) : undefined,
    governance: env.VITE_GOVERNANCE_ADDRESS ? (env.VITE_GOVERNANCE_ADDRESS as Address) : undefined,
    // Issue #647: TimelockController address for the Timelock admin tab (architecture §4.5).
    timelock: env.VITE_TIMELOCK_ADDRESS ? (env.VITE_TIMELOCK_ADDRESS as Address) : undefined,
    // Issue #365: RM token address for the Faucet tab drip button. Absent means
    // the button is hidden in standalone deployments without the smoke-test harness.
    rmToken: env.VITE_RM_TOKEN_ADDRESS ? (env.VITE_RM_TOKEN_ADDRESS as Address) : undefined,
    expectedCodeHash: env.VITE_GATEWAY_EXPECTED_CODE_HASH,
    envClass: (env.VITE_ENV_CLASS as DappConfig["envClass"]) ?? "fork",
    explorerApiUrl: resolveExplorerApiUrl(env),
    // Issue #1247 task 4.14: the per-deployment vault-symbol map that
    // tests/fixtures/consensus-receipt.bucket-vault-map.json requires in order to
    // say whether a recommendation was applied. When it is absent or incomplete the
    // deployment is not receipt-capable for that comparison, and the surface says
    // "cannot determine" rather than substituting a global or zero address.
    vaultAddressBySymbol: parseVaultAddressMap(env.VITE_VAULT_ADDRESSES),
  };
}

export function App({ cfg }: { readonly cfg: DappConfig }) {
  const { state: verificationState, refresh: verificationRefresh } = useGatewayVerifier(
    cfg.gateway,
    cfg.expectedCodeHash,
  );
  const [selectedVault, setSelectedVault] = useState<string | null>(null);
  const [activeTabId, setActiveTabId] = useState("my-account");
  const [aboutOpen, setAboutOpen] = useState(false);
  const [currentPath, setCurrentPath] = useState(() => window.location.pathname);
  const { address: connectedAddress } = useAccount();

  // Listen for navigation so the /debug route works with browser back/forward.
  useEffect(() => {
    const handlePopState = () => {
      setCurrentPath(window.location.pathname);
    };
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, []);

  const isDebugRoute = currentPath === "/debug";

  if (isDebugRoute) {
    return (
      <>
        <TestnetBanner
          envClass={cfg.envClass}
          forkTimestamp={cfg.env.VITE_FORK_BLOCK_TIMESTAMP}
          forkBlock={cfg.env.VITE_FORK_BLOCK_NUMBER}
        />
        <NavBar aboutOpen={aboutOpen} onToggleAbout={() => setAboutOpen((open) => !open)} />
        <AboutModal open={aboutOpen} onClose={() => setAboutOpen(false)} envClass={cfg.envClass} />
        <DebugPage
          gatewayAddress={cfg.gateway}
          vaultAddress={cfg.vault}
          registryAddress={cfg.registry}
          routerAddress={cfg.router}
          envClass={cfg.envClass}
          explorerApiUrl={cfg.explorerApiUrl}
          expectedCodeHash={cfg.expectedCodeHash}
          forkTimestamp={cfg.env.VITE_FORK_BLOCK_TIMESTAMP}
          forkBlock={cfg.env.VITE_FORK_BLOCK_NUMBER}
          verificationState={verificationState}
        />
      </>
    );
  }

  return (
    <>
      <TestnetBanner
        envClass={cfg.envClass}
        forkTimestamp={cfg.env.VITE_FORK_BLOCK_TIMESTAMP}
        forkBlock={cfg.env.VITE_FORK_BLOCK_NUMBER}
      />
      <NavBar aboutOpen={aboutOpen} onToggleAbout={() => setAboutOpen((open) => !open)} />
      <AboutModal open={aboutOpen} onClose={() => setAboutOpen(false)} envClass={cfg.envClass} />
      <StatusHeader />
      <VerificationBanner state={verificationState} refresh={verificationRefresh} />
      <main className="dapp-shell">
        <div className="landing-overview">
          <ProtocolStats />
          <LandingPriceStrip />
          <VaultCards
            onSelectVault={setSelectedVault}
            onSwitchToExplorer={() => setActiveTabId("portfolio-explorer")}
          />
          <BalancesPanel gatewayAddress={cfg.gateway} rmTokenAddress={cfg.rmToken} />
        </div>

        <Tabs
          testId="dapp-surface-tabs"
          activeTabId={activeTabId}
          onTabChange={setActiveTabId}
          tabs={[
            {
              id: "my-account",
              label: "My Account",
              content: (
                <AgentsPanel
                  gatewayAddress={cfg.gateway}
                  vaultAddress={cfg.vault}
                  gatewayVerificationState={verificationState}
                  envClass={cfg.envClass}
                  flagEnv={cfg.env}
                  // eslint-disable-next-line no-restricted-syntax -- boundary: real clock injected here.
                  now={Date.now()}
                  registryAddress={cfg.registry}
                  routerAddress={cfg.router}
                  rmTokenAddress={cfg.rmToken}
                  timelockAddress={cfg.timelock}
                />
              ),
            },
            {
              id: "router-governance",
              label: "Router Governance",
              content: (
                <div className="tab-section-stack">
                  <RouterView apiUrl={cfg.explorerApiUrl} />
                  {cfg.governance ? (
                    <GovernancePanel
                      governanceAddress={cfg.governance}
                      apiUrl={cfg.explorerApiUrl}
                    />
                  ) : (
                    <section data-testid="governance-config-missing">
                      <h2>Governance — Weight Proposals</h2>
                      <p className="hint">
                        Router governance voting is unavailable until the governance contract
                        address is configured.
                      </p>
                    </section>
                  )}
                </div>
              ),
            },
            {
              id: "consensus-receipts",
              label: "Consensus Receipts",
              content: (
                <div className="tab-section-stack">
                  <ConsensusReceiptPanel
                    explorerApiUrl={cfg.explorerApiUrl}
                    vaultAddressBySymbol={cfg.vaultAddressBySymbol}
                  />
                </div>
              ),
            },
            {
              id: "portfolio-explorer",
              label: "Portfolio Explorer",
              content: (
                <div className="tab-section-stack">
                  {selectedVault != null ? (
                    <VaultDetail
                      apiUrl={cfg.explorerApiUrl}
                      address={selectedVault}
                      onBack={() => setSelectedVault(null)}
                    />
                  ) : (
                    <VaultList onSelectVault={setSelectedVault} />
                  )}
                  <AccountLayerView
                    apiUrl={cfg.explorerApiUrl}
                    connectedAddress={connectedAddress as Address | undefined}
                  />
                </div>
              ),
            },
          ]}
        />
      </main>
    </>
  );
}

/**
 * The full provider stack around `App`, parameterised by a runtime config.
 *
 * VaultRegistryProvider is mounted only when the registry address is
 * configured (single-vault deployments leave it unset).
 */
export function DappRoot({ config }: { readonly config: RuntimeConfig }) {
  const cfg = useMemo(() => deriveDappConfig(config), [config]);
  // wagmi's config opens connector subscriptions, so build it once per
  // runtime config rather than on every render.
  const wagmiConfig = useMemo(() => makeConfig(config), [config]);
  const [queryClient] = useState(() => new QueryClient());

  const appWithChainProviders = cfg.registry ? (
    <VaultRegistryProvider registryAddress={cfg.registry}>
      <App cfg={cfg} />
    </VaultRegistryProvider>
  ) : (
    <App cfg={cfg} />
  );

  return (
    <RuntimeConfigProvider config={config}>
      <WagmiProvider config={wagmiConfig}>
        <QueryClientProvider client={queryClient}>
          <ExplorerProvider apiUrl={cfg.explorerApiUrl}>{appWithChainProviders}</ExplorerProvider>
        </QueryClientProvider>
      </WagmiProvider>
    </RuntimeConfigProvider>
  );
}

/** Shown while `/config.json` is in flight. */
export function RuntimeConfigLoading() {
  return (
    <main className="dapp-shell" data-testid="runtime-config-loading">
      <section>
        <h2>Loading configuration…</h2>
        <p className="hint">Fetching this deployment&rsquo;s contract addresses and endpoints.</p>
      </section>
    </main>
  );
}

/**
 * Shown when `/config.json` was served but unusable. Deliberately a dead end:
 * every address the app would otherwise use is unknown, and rendering the app
 * anyway would point users at whatever the build-time defaults happened to be.
 */
export function RuntimeConfigFailure({ message }: { readonly message: string }) {
  return (
    <main className="dapp-shell" data-testid="runtime-config-error">
      <section>
        <h2>Configuration unavailable</h2>
        <p>
          This deployment&rsquo;s runtime configuration could not be loaded, so the dapp cannot tell
          which contracts to talk to. It will not guess.
        </p>
        <p className="hint" data-testid="runtime-config-error-detail">
          {message}
        </p>
        <p className="hint">Reload once the operator has corrected the deployment.</p>
      </section>
    </main>
  );
}

/** Minimal React root surface used by the bootstrap — matches `ReactDOM.Root`. */
export interface RootLike {
  render: (node: ReactElement) => void;
}

export interface BootstrapDeps {
  /** Build-time env map — the base layer the fetched config overlays. */
  readonly buildEnv: RuntimeConfig;
  /** Injected so tests drive the fetch without patching the global. */
  readonly fetchImpl: ConfigFetchLike;
  readonly configUrl?: string;
  /**
   * Renders the app for a resolved config. Injected rather than hard-wired to
   * `DappRoot` so `main-bootstrap.test.tsx` can observe the config the gate
   * hands downstream without mounting the entire chain-connected tree.
   */
  readonly renderApp: (config: RuntimeConfig) => ReactElement;
}

/**
 * Render the loading state, await the runtime config, then render the app —
 * or the failure panel. Resolves to the config's source on success and
 * `undefined` when the gate ended in the error state.
 */
export async function bootstrapDapp(
  root: RootLike,
  deps: BootstrapDeps,
): Promise<RuntimeConfigSource | undefined> {
  root.render(
    <React.StrictMode>
      <RuntimeConfigLoading />
    </React.StrictMode>,
  );

  let loaded;
  try {
    loaded = await loadRuntimeConfig({
      fetchImpl: deps.fetchImpl,
      buildEnv: deps.buildEnv,
      url: deps.configUrl,
    });
  } catch (err) {
    root.render(
      <React.StrictMode>
        <RuntimeConfigFailure message={(err as { message?: string }).message ?? String(err)} />
      </React.StrictMode>,
    );
    return undefined;
  }

  root.render(<React.StrictMode>{deps.renderApp(loaded.config)}</React.StrictMode>);
  return loaded.source;
}
