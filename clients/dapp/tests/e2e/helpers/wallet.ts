/**
 * Playwright wallet injection helper.
 *
 * Installs a minimal EIP-1193 provider as `window.ethereum` *before*
 * any dapp code loads, so the dapp's production `injected()` wagmi
 * connector picks it up exactly as it would pick up MetaMask. No
 * test-only code in the dapp bundle — the only seam is at the browser
 * level.
 *
 * Read methods (eth_call, eth_getBlockByNumber, …) are forwarded to
 * the real RPC URL via fetch().
 * Write methods (eth_sendTransaction, personal_sign, eth_sign,
 * eth_signTypedData*) are forwarded to viem in the Node-side test
 * runner via Playwright `exposeBinding`, then signed and broadcast
 * with the supplied private key.
 */
import type { Page } from "@playwright/test";
import { createPublicClient, createWalletClient, http, type Hex, type Address } from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { estimateGasBuffered } from "./gas";
import type { DevnetEndpoints } from "./devnet";

interface InjectWalletOptions {
  privateKey: Hex;
  rpcUrl: string;
  chainId: number;
}

interface RpcRequest {
  method: string;
  params?: unknown[];
}

const FORWARDED_WRITE_METHODS = new Set([
  "eth_sendTransaction",
  "eth_signTransaction",
  "personal_sign",
  "eth_sign",
  "eth_signTypedData",
  "eth_signTypedData_v3",
  "eth_signTypedData_v4",
]);

/**
 * Inject a browser-side EIP-1193 provider that signs writes with
 * `privateKey` and forwards reads to `rpcUrl`.
 *
 * Must be awaited before the first `page.goto()` so the provider is
 * present at module-evaluation time of the dapp bundle.
 */
export async function injectWallet(page: Page, opts: InjectWalletOptions): Promise<void> {
  const account = privateKeyToAccount(opts.privateKey);
  const walletClient = createWalletClient({
    account,
    transport: http(opts.rpcUrl),
  });
  const publicClient = createPublicClient({
    transport: http(opts.rpcUrl),
  });

  await page.exposeBinding("__rmpcRpc", async (_source, body: unknown) => {
    const res = await fetch(opts.rpcUrl, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return res.json();
  });

  await page.exposeBinding("__rmpcSign", async (_source, req: RpcRequest) => {
    switch (req.method) {
      case "eth_sendTransaction": {
        const [tx] = (req.params ?? []) as Array<{
          to?: Address;
          data?: Hex;
          value?: Hex;
          gas?: Hex;
        }>;
        // Gas handling mirrors a real injected wallet (e.g. MetaMask).
        //
        // The dapp's `writeContract(simulateResult.request)` does NOT set a
        // `gas` field (viem's `simulateContract` returns only abi/address/
        // args/account — see the upstream action), so an injected wallet is
        // responsible for filling gas. A bare `eth_estimateGas` is not safe
        // to use verbatim: Geth under-estimates transactions that trigger
        // gas refunds (e.g. an ERC-4626 `redeem` that burns shares and
        // clears storage slots), so a tx sent with exactly the estimate
        // mines but reverts out-of-gas — the balance never changes and the
        // failure is silent. Real wallets pad the estimate; we do the same
        // with a 1.5x buffer so the harness behaves like production.
        //
        // The buffer itself lives in `helpers/gas.ts`, shared with the specs
        // that sign directly rather than through this provider, so the two
        // paths cannot drift apart (issue #1388).
        let gas: bigint | undefined = tx.gas ? BigInt(tx.gas) : undefined;
        if (gas === undefined) {
          gas = await estimateGasBuffered(publicClient, account.address, {
            to: tx.to,
            data: tx.data,
            value: tx.value ? BigInt(tx.value) : undefined,
          });
        }
        return walletClient.sendTransaction({
          chain: null,
          to: tx.to,
          data: tx.data,
          value: tx.value ? BigInt(tx.value) : undefined,
          gas,
        });
      }
      case "personal_sign":
      case "eth_sign": {
        const params = (req.params ?? []) as Hex[];
        // personal_sign: [data, address]; eth_sign: [address, data]
        const message = req.method === "personal_sign" ? params[0] : params[1];
        return walletClient.signMessage({ message: { raw: message } });
      }
      case "eth_signTypedData":
      case "eth_signTypedData_v3":
      case "eth_signTypedData_v4": {
        const params = (req.params ?? []) as [Address, string];
        const typedData = JSON.parse(params[1]);
        return walletClient.signTypedData(typedData);
      }
      default:
        throw new Error(`Unhandled signing method: ${req.method}`);
    }
  });

  await page.addInitScript(
    ({
      address,
      chainIdHex,
      writeMethods,
    }: {
      address: string;
      chainIdHex: string;
      writeMethods: string[];
    }) => {
      const writeSet = new Set(writeMethods);
      let nextId = 1;
      const listeners: Record<string, Array<(...args: unknown[]) => void>> = {};

      const provider = {
        isMetaMask: false,
        isRobotMoneyTestWallet: true,
        chainId: chainIdHex,
        selectedAddress: address,
        async request(req: { method: string; params?: unknown[] }) {
          switch (req.method) {
            case "eth_accounts":
            case "eth_requestAccounts":
              return [address];
            case "eth_chainId":
            case "net_version":
              return chainIdHex;
            case "wallet_switchEthereumChain":
            case "wallet_addEthereumChain":
              return null;
            default:
              break;
          }
          if (writeSet.has(req.method)) {
            return (
              window as unknown as {
                __rmpcSign: (r: { method: string; params?: unknown[] }) => Promise<unknown>;
              }
            ).__rmpcSign({ method: req.method, params: req.params });
          }
          const result = (await (
            window as unknown as {
              __rmpcRpc: (
                body: unknown,
              ) => Promise<{ result?: unknown; error?: { message: string } }>;
            }
          ).__rmpcRpc({
            jsonrpc: "2.0",
            id: nextId++,
            method: req.method,
            params: req.params ?? [],
          })) as { result?: unknown; error?: { message: string } };
          if (result.error) throw new Error(result.error.message);
          return result.result;
        },
        on(event: string, handler: (...args: unknown[]) => void) {
          (listeners[event] ??= []).push(handler);
        },
        removeListener(event: string, handler: (...args: unknown[]) => void) {
          const list = listeners[event];
          if (!list) return;
          const i = list.indexOf(handler);
          if (i >= 0) list.splice(i, 1);
        },
      };
      Object.defineProperty(window, "ethereum", {
        value: provider,
        writable: false,
        configurable: false,
      });
    },
    {
      address: account.address,
      chainIdHex: `0x${opts.chainId.toString(16)}`,
      writeMethods: Array.from(FORWARDED_WRITE_METHODS),
    },
  );
}

/**
 * Click the unified "Connect wallet" button and wait until AgentsPanel
 * has flipped from the connect gate to the post-connect agent surface
 * (the onboarding wizard or the admin tabs). Use after `injectWallet`
 * and `page.goto()`.
 *
 * Connecting flips AgentsPanel in place — no navigation, no page reload,
 * no wagmi reconnect-on-mount. Waiting for the agent surface is itself
 * proof the wallet connected, so there is nothing to verify on a separate
 * page. (An earlier implementation round-tripped through `/debug` to read
 * `connected-address`; that added two full reloads per connect and relied
 * on reconnect-on-mount winning a first-paint race twice — the source of
 * the intermittent suite-10 failures where neither `onboarding-wizard`
 * nor `admin-tabs` ever appeared. The `/debug` route itself is covered by
 * `tests/unit/debug-page.test.tsx`.)
 */
export async function connectInjectedWallet(page: Page): Promise<void> {
  const { expect } = await import("@playwright/test");
  const button = page.getByTestId("connect-wallet").first();
  await expect(button).toBeVisible();
  await button.click();
  await ensureWalletConnected(page);
}

/**
 * Wait until the post-connect agent surface (onboarding wizard or admin
 * tabs) is on screen, clicking Connect again if the gate is still shown.
 *
 * Covers two cases with one helper:
 *   - the initial click's connect mutation hasn't rendered yet (normal), and
 *   - a connect/reconnect attempt that silently didn't take — e.g. wagmi's
 *     reconnect-on-mount losing the first-paint race after a full reload,
 *     which leaves AgentsPanel parked on the gate with no auto-recovery.
 *
 * The injected provider answers `eth_requestAccounts` synchronously, so a
 * re-click re-establishes the session immediately. A no-op re-click never
 * happens: we only click when the surface failed to appear AND the gate is
 * still visible.
 */
export async function ensureWalletConnected(page: Page): Promise<void> {
  const { expect } = await import("@playwright/test");
  const gate = page.getByTestId("connect-wallet").first();
  const surface = page.getByTestId("onboarding-wizard").or(page.getByTestId("admin-tabs"));
  for (let attempt = 0; attempt < 3; attempt++) {
    // Give the in-flight connect a chance to render the surface.
    const settled = await surface
      .waitFor({ state: "visible", timeout: 10_000 })
      .then(() => true)
      .catch(() => false);
    if (settled) return;
    // Surface never came up; if the gate is still showing the connect did
    // not take — click it and retry.
    if (await gate.isVisible().catch(() => false)) {
      await gate.click();
    }
  }
  // Final settle — surface a clear error if Connect never took.
  await expect(surface).toBeVisible({ timeout: 30_000 });
}

/**
 * Combined "open the dapp as <role>" setup: inject the role's wallet
 * provider, navigate to the smoke-test dapp URL, and (optionally)
 * connect. The default role is `admin` (the gateway deployer).
 */
export async function openDapp(
  page: Page,
  endpoints: DevnetEndpoints,
  opts: { role?: "admin" | "pauser" | "agent"; connect?: boolean } = {},
): Promise<void> {
  const role = opts.role ?? "admin";
  const privateKey = (
    role === "pauser"
      ? endpoints.pauser_private_key
      : role === "agent"
        ? endpoints.agent_private_key
        : endpoints.admin_private_key
  ) as Hex;
  await injectWallet(page, {
    privateKey,
    rpcUrl: endpoints.rpc_url,
    chainId: endpoints.chain_id,
  });
  await page.goto(endpoints.dapp_url);
  if (opts.connect !== false) {
    await connectInjectedWallet(page);
    await dismissOnboardingIfPresent(page);
  }
}

/**
 * Wait for AgentsPanel to settle on either the OnboardingWizard or
 * AdminFlow after wallet connect, and click the wizard's Skip → Admin
 * button (data-testid `wizard-dismiss`) if the wizard wins the race.
 *
 * A fresh wallet on smoke-test devnet has neither authorized an agent
 * nor holds vault shares, so without this helper AgentsPanel parks on
 * the wizard and any spec that interacts with AdminFlow times out.
 *
 * Specs that intentionally exercise the wizard (e.g. `?force-onboarding=1`)
 * must not call this helper.
 */
export async function dismissOnboardingIfPresent(page: Page): Promise<void> {
  const { expect } = await import("@playwright/test");
  const wizard = page.getByTestId("onboarding-wizard");
  const adminTabs = page.getByTestId("admin-tabs");
  await expect(wizard.or(adminTabs)).toBeVisible({ timeout: 30_000 });
  if (await wizard.isVisible().catch(() => false)) {
    await page.getByTestId("wizard-dismiss").click();
    await expect(adminTabs).toBeVisible({ timeout: 30_000 });
  }
}

/**
 * Activate a named tab in the AdminFlow. Tabs render only the active
 * panel into the DOM, so specs must call this before interacting with
 * any form testid that lives inside a tab.
 */
export type AdminTabId =
  | "authorize"
  | "deposit-withdraw"
  | "pause"
  | "revoke"
  | "rotation"
  | "admin-role"
  | "pauser-role"
  | "faucet"
  | "history"
  | "export";

export async function openTab(page: Page, tabId: AdminTabId): Promise<void> {
  await page.getByTestId(`tab-${tabId}`).click();
  await page.getByTestId(`tabpanel-${tabId}`).waitFor({ state: "visible" });
}
