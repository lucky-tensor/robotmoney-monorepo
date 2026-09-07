// Canonical: docs/architecture.md §5.3 — Human Dapp

/**
 * DebugPanel — advanced observability drawer for chain, contract, wallet,
 * account, and runtime diagnostics.
 */
import { useEffect, useMemo, useState, type ReactNode } from "react";
import type { Address } from "viem";
import { useAccount, useBlockNumber, useChainId, useDisconnect, useReadContract } from "wagmi";
import { gatewayAbi } from "../lib/abi";
import { resolveTargetChainId } from "../lib/wagmi";
import { useRuntimeConfig } from "../lib/RuntimeConfigContext";
import { getInjectedProvider, syncDevnetChain } from "../lib/syncDevnetChain";
import type { VerificationState } from "../lib/useGatewayVerifier";
import { useAgentRegistration } from "../lib/useVaultRegistration";

type EnvClass = "fork" | "devnet" | "testnet" | "mainnet";
type LogLevel = "log" | "warn" | "error";

interface RuntimeLog {
  readonly id: number;
  readonly level: LogLevel;
  readonly message: string;
  readonly timestamp: string;
}

interface DebugPanelProps {
  readonly open: boolean;
  readonly onClose: () => void;
  readonly gatewayAddress: Address;
  readonly vaultAddress: Address;
  readonly registryAddress?: Address;
  readonly routerAddress?: Address;
  readonly envClass: EnvClass;
  readonly explorerApiUrl: string;
  readonly expectedCodeHash?: string;
  readonly forkTimestamp?: string;
  readonly forkBlock?: string;
  readonly verificationState: VerificationState;
}

const MAX_LOGS = 80;

export function DebugPanel(props: DebugPanelProps) {
  const { address, connector, isConnected, status } = useAccount();
  const { disconnect } = useDisconnect();
  const chainId = useChainId();
  const registrationStatus = useAgentRegistration(props.vaultAddress);
  const [networkSyncError, setNetworkSyncError] = useState<string | undefined>(undefined);
  // Issue #1356: expected chain and devnet RPC URL come from the fetched
  // runtime config, not a build-time constant.
  const runtimeConfig = useRuntimeConfig();
  const targetChainId = resolveTargetChainId(runtimeConfig);
  const [logs, setLogs] = useState<RuntimeLog[]>([]);

  const { data: blockNumber, error: blockError } = useBlockNumber({
    query: { enabled: isConnected, refetchInterval: 12_000 },
  });
  const { data: pausedData, error: pausedError } = useReadContract({
    address: props.gatewayAddress,
    abi: gatewayAbi,
    functionName: "paused",
    query: { enabled: isConnected },
  });
  const { data: usdcAddressData, error: usdcError } = useReadContract({
    address: props.gatewayAddress,
    abi: gatewayAbi,
    functionName: "usdc",
    query: { enabled: isConnected },
  });

  useEffect(() => {
    const originals = {
      log: console.log,
      warn: console.warn,
      error: console.error,
    };
    let nextId = 0;
    const push = (level: LogLevel, values: unknown[]) => {
      const timestamp = new Date().toISOString();
      const message = values.map(formatLogValue).join(" ");
      setLogs((current) =>
        [{ id: nextId++, level, message, timestamp }, ...current].slice(0, MAX_LOGS),
      );
    };

    console.log = (...values: unknown[]) => {
      originals.log(...values);
      push("log", values);
    };
    console.warn = (...values: unknown[]) => {
      originals.warn(...values);
      push("warn", values);
    };
    console.error = (...values: unknown[]) => {
      originals.error(...values);
      push("error", values);
    };

    const handleError = (event: ErrorEvent) => {
      push("error", [event.message || event.error || "window error"]);
    };
    const handleRejection = (event: PromiseRejectionEvent) => {
      push("error", ["Unhandled rejection:", event.reason]);
    };
    window.addEventListener("error", handleError);
    window.addEventListener("unhandledrejection", handleRejection);

    return () => {
      console.log = originals.log;
      console.warn = originals.warn;
      console.error = originals.error;
      window.removeEventListener("error", handleError);
      window.removeEventListener("unhandledrejection", handleRejection);
    };
  }, []);

  const paused = Boolean(pausedData);
  const usdcAddress = (usdcAddressData as Address | undefined) ?? "";
  const readErrors = useMemo(
    () =>
      [
        blockError ? `blockNumber: ${blockError.message}` : undefined,
        pausedError ? `gateway.paused: ${pausedError.message}` : undefined,
        usdcError ? `gateway.usdc: ${usdcError.message}` : undefined,
      ].filter((item): item is string => item !== undefined),
    [blockError, pausedError, usdcError],
  );

  const handleSwitchChain = () => {
    const provider = getInjectedProvider();
    if (!provider) {
      setNetworkSyncError("No injected wallet provider (window.ethereum is undefined).");
      return;
    }
    void syncDevnetChain(provider, runtimeConfig).then(setNetworkSyncError);
  };

  return (
    <aside
      id="debug-panel"
      className="debug-panel"
      data-testid="debug-panel"
      data-open={props.open}
      aria-hidden={!props.open}
    >
      <div className="debug-panel-header">
        <div>
          <p className="debug-eyebrow">Observability</p>
          <h2>Debug panel</h2>
        </div>
        <button type="button" data-testid="debug-panel-close" onClick={props.onClose}>
          Close
        </button>
      </div>

      <DebugSection title="Build">
        <DebugRow label="Dapp version" value={__DAPP_VERSION__} testId="debug-dapp-version" />
        <DebugRow label="GitHub commit" value={__GIT_COMMIT__} testId="debug-github-commit" />
      </DebugSection>

      <DebugSection title="Chain">
        <DebugRow label="Environment" value={props.envClass} testId="debug-env-class" />
        <DebugRow
          label="Wallet chain"
          value={isConnected ? chainId : "—"}
          testId="debug-chain-id"
        />
        <DebugRow label="Expected chain" value={targetChainId ?? "—"} />
        <DebugRow
          label="Latest block"
          value={blockNumber?.toString() ?? "—"}
          testId="debug-block-number"
        />
        <DebugRow label="Explorer API" value={props.explorerApiUrl} />
        <DebugRow label="Fork block" value={props.forkBlock ?? "—"} />
        <DebugRow label="Fork time" value={props.forkTimestamp ?? "—"} />
      </DebugSection>

      <DebugSection title="Contracts">
        <DebugRow label="Gateway" value={props.gatewayAddress} testId="debug-gateway-address" />
        <DebugRow label="Vault" value={props.vaultAddress} testId="debug-vault-address" />
        <DebugRow label="Registry" value={props.registryAddress ?? "—"} />
        <DebugRow label="Router" value={props.routerAddress ?? "—"} />
        <DebugRow label="USDC" value={usdcAddress || "—"} testId="debug-usdc-address" />
        <DebugRow
          label="Gateway state"
          value={isConnected ? (paused ? "PAUSED" : "ACTIVE") : "—"}
          testId="debug-public-paused"
        />
        <DebugRow label="Expected code hash" value={props.expectedCodeHash ?? "—"} />
        <DebugRow label="Verification" value={formatVerification(props.verificationState)} />
      </DebugSection>

      <DebugSection title="Wallet">
        <DebugRow label="Status" value={status} testId="debug-wallet-status" />
        <DebugRow label="Connector" value={connector?.name ?? "—"} />
        <DebugRow label="Account" value={address ?? "—"} testId="connected-address" />
        <div className="debug-actions">
          {targetChainId !== undefined && chainId !== targetChainId && (
            <button type="button" data-testid="switch-chain" onClick={handleSwitchChain}>
              Switch chain
            </button>
          )}
          {isConnected && (
            <button type="button" data-testid="disconnect" onClick={() => disconnect()}>
              Disconnect
            </button>
          )}
        </div>
        {networkSyncError && (
          <p data-testid="network-sync-error" className="unsafe-banner">
            <strong>Network setup error:</strong> {networkSyncError}
          </p>
        )}
      </DebugSection>

      <DebugSection title="Account State">
        <DebugRow
          label="Registration"
          value={registrationStatus}
          testId="debug-registration-status"
        />
      </DebugSection>

      <DebugSection title="Read Errors">
        {readErrors.length === 0 ? (
          <p className="debug-empty" data-testid="debug-read-errors-empty">
            No read errors captured.
          </p>
        ) : (
          <ul className="debug-log-list" data-testid="debug-read-errors">
            {readErrors.map((message) => (
              <li key={message} data-level="error">
                {message}
              </li>
            ))}
          </ul>
        )}
      </DebugSection>

      <DebugSection title="App Logs">
        {logs.length === 0 ? (
          <p className="debug-empty" data-testid="debug-logs-empty">
            No app logs captured this session.
          </p>
        ) : (
          <ul className="debug-log-list" data-testid="debug-log-list">
            {logs.map((log) => (
              <li key={log.id} data-level={log.level}>
                <time>{log.timestamp}</time>
                <span>{log.level}</span>
                <code>{log.message}</code>
              </li>
            ))}
          </ul>
        )}
      </DebugSection>
    </aside>
  );
}

function DebugSection(props: { readonly title: string; readonly children: ReactNode }) {
  return (
    <section className="debug-section">
      <h3>{props.title}</h3>
      {props.children}
    </section>
  );
}

function DebugRow(props: {
  readonly label: string;
  readonly value: unknown;
  readonly testId?: string;
}) {
  return (
    <div className="debug-row">
      <dt>{props.label}</dt>
      <dd data-testid={props.testId}>{String(props.value)}</dd>
    </div>
  );
}

function formatVerification(state: VerificationState): string {
  switch (state.status) {
    case "idle":
      return "idle";
    case "pending":
      return "pending";
    case "verified":
      return `verified ${state.computedHash}`;
    case "refused":
      return `refused: ${state.reason}`;
  }
}

function formatLogValue(value: unknown): string {
  if (value instanceof Error) return `${value.name}: ${value.message}`;
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}
