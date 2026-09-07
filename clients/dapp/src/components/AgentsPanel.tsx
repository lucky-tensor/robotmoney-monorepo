// Canonical: docs/architecture.md §5.2 — Agent Permissions Gateway

/**
 * AgentsPanel — gates the full per-user agent management surface.
 *
 *   no wallet                     → connect prompt
 *   wallet, no agent + no shares  → OnboardingWizard
 *   wallet, has agent or shares   → AdminFlow (all management tabs)
 *
 * Setting `VITE_FORCE_ONBOARDING=1` makes the wizard the post-connect view
 * regardless of registration status — for local layout review without infra.
 * The wallet-connect gate is unaffected.
 *
 * Registration is decided by useAgentRegistration — see that file for the
 * agent-authorized localStorage flag and the vault-share-balance check.
 */
import { useEffect, useState } from "react";
import { useAccount, useConnect } from "wagmi";
import type { Address } from "viem";
import { AdminFlow } from "./AdminFlow";
import { OnboardingWizard } from "./OnboardingWizard";
import {
  useAgentRegistration,
  markOnboardingDismissed,
  isOnboardingDismissed,
} from "../lib/useVaultRegistration";
import type { VerificationState } from "../lib/useGatewayVerifier";
import type { PreviewContext } from "../lib/preview";
import { getInjectedProvider, syncDevnetChain } from "../lib/syncDevnetChain";
import { useRuntimeConfig } from "../lib/RuntimeConfigContext";

type Props = Readonly<{
  gatewayAddress: Address;
  vaultAddress: Address;
  gatewayVerificationState: VerificationState;
  envClass: "fork" | "devnet" | "testnet" | "mainnet";
  flagEnv: Record<string, string | undefined>;
  now: number;
  /** VaultRegistry address (issue #320). Optional. */
  registryAddress?: Address;
  /** PortfolioRouter address (issue #320). Optional. */
  routerAddress?: Address;
  /** RM token address for the Faucet tab drip button (issue #365). Optional. */
  rmTokenAddress?: Address;
  /**
   * TimelockController address (issue #647 / architecture §4.5).
   * Forwarded to AdminFlow → buildAdminTabs → TimelockPanel.
   */
  timelockAddress?: Address;
}>;

export function AgentsPanel(props: Props) {
  const { isConnected, address } = useAccount();
  const { connect, connectors } = useConnect();
  const status = useAgentRegistration(props.vaultAddress);
  const [networkSyncError, setNetworkSyncError] = useState<string | undefined>(undefined);
  // Issue #1356: the devnet RPC URL wallet_addEthereumChain suggests comes
  // from the fetched runtime config rather than a build-time constant.
  const runtimeConfig = useRuntimeConfig();

  // localStorage-backed dismiss flag, keyed per wallet address.
  // Initialised synchronously from localStorage so there is no wizard flash on
  // subsequent page loads. Updated via storage events for cross-tab consistency.
  const [onboardingDismissed, setOnboardingDismissed] = useState<boolean>(() => {
    if (!address) return false;
    return isOnboardingDismissed(address);
  });
  useEffect(() => {
    if (!address) {
      setOnboardingDismissed(false);
      return;
    }
    const read = () => setOnboardingDismissed(isOnboardingDismissed(address));
    read();
    window.addEventListener("rm:onboarding-dismissed-changed", read);
    window.addEventListener("storage", read);
    return () => {
      window.removeEventListener("rm:onboarding-dismissed-changed", read);
      window.removeEventListener("storage", read);
    };
  }, [address]);
  // `VITE_FORCE_ONBOARDING=1` (build-time) is the documented operator
  // override; `?force-onboarding=1` (URL query) is the dapp-side hook
  // the issue-#261 Playwright e2e suite uses to drive the onboarding
  // wizard against the smoke-test devnet without rebuilding the
  // container with a different env class. Both forms are dev-only —
  // mainnet operator builds never set the env var, and the URL flag
  // only activates onboarding mounting (it never reaches the faucet
  // seed path, which still requires `classifyChain === "testnet"`).
  const urlForceOnboarding =
    typeof window !== "undefined" &&
    new URLSearchParams(window.location.search).get("force-onboarding") === "1";
  const forceOnboarding = props.flagEnv.VITE_FORCE_ONBOARDING === "1" || urlForceOnboarding;

  const handleConnect = (connector: (typeof connectors)[number]) => {
    connect(
      { connector },
      {
        onSuccess: () => {
          const provider = getInjectedProvider();
          if (!provider) {
            setNetworkSyncError("No injected wallet provider (window.ethereum is undefined).");
            return;
          }
          void syncDevnetChain(provider, runtimeConfig).then(setNetworkSyncError);
        },
      },
    );
  };

  if (!isConnected) {
    return (
      <main className="agents-gate" data-testid="agents-gate-connect">
        <section>
          <h2>Connect wallet</h2>
          <p>Connect a wallet to authorize your first agent and manage your policies.</p>
          {connectors[0] ? (
            <button
              type="button"
              data-testid="connect-wallet"
              onClick={() => handleConnect(connectors[0])}
            >
              Connect wallet
            </button>
          ) : (
            <p data-testid="no-connectors" className="hint">
              No browser wallet detected. Install a wallet extension to continue.
            </p>
          )}
          {networkSyncError && (
            <p data-testid="network-sync-error" className="unsafe-banner">
              <strong>Network setup error:</strong> {networkSyncError}
            </p>
          )}
        </section>
      </main>
    );
  }

  if ((status === "unregistered" || forceOnboarding) && !onboardingDismissed) {
    const ctx: PreviewContext = {
      gateway: props.gatewayAddress,
      gatewayCodeHashVerified: props.gatewayVerificationState.status === "verified",
      envClass: props.envClass,
    };
    return (
      <OnboardingWizard
        gatewayAddress={props.gatewayAddress}
        ctx={ctx}
        env={props.flagEnv}
        now={props.now}
        onDismiss={() => {
          if (address) markOnboardingDismissed(address);
          setOnboardingDismissed(true);
        }}
        rmTokenAddress={props.rmTokenAddress}
      />
    );
  }

  return (
    <AdminFlow
      gatewayAddress={props.gatewayAddress}
      vaultAddress={props.vaultAddress}
      gatewayVerificationState={props.gatewayVerificationState}
      envClass={props.envClass}
      flagEnv={props.flagEnv}
      now={props.now}
      registryAddress={props.registryAddress}
      routerAddress={props.routerAddress}
      rmTokenAddress={props.rmTokenAddress}
      timelockAddress={props.timelockAddress}
    />
  );
}
