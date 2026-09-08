/**
 * suite-10: Playwright E2E — multi-vault withdrawal with PositionSelector
 * and live previewRedeem (issue #321).
 *
 * Flow:
 *   1. Boot devnet via globalSetup (shared with vault-deposit-withdraw.spec.ts).
 *   2. Pre-fund the admin EOA with USDC and deposit into the vault so
 *      the user holds a non-zero receipt balance.
 *   3. Stub GET /v1/accounts/:address/positions on the explorer-API mock
 *      to return the vault position. (The smoke-test devnet explorer-api
 *      is a real server; this spec registers the route via Playwright's
 *      network interception so we don't depend on indexer freshness.)
 *   4. Open the dapp as admin, navigate to the Deposit & Withdraw tab.
 *   5. Assert PositionSelector lists the vault position.
 *   6. Click the position — input should pre-fill with the share balance.
 *   7. Assert the withdrawal preview shows net USDC (previewRedeem hint).
 *   8. Click "Sign withdraw with wallet" and assert receipt balance decrements.
 */
import { test, expect } from "./helpers/fixtures";
import { setTimeout as sleep } from "node:timers/promises";
import {
  createPublicClient,
  createWalletClient,
  encodeFunctionData,
  http,
  type Address,
  type Hex,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { loadEndpoints, type DevnetEndpoints } from "./helpers/devnet";
import { sendBufferedTransaction } from "./helpers/gas";
import { openDapp, openTab } from "./helpers/wallet";
import { erc20Abi, vaultAbi } from "../../src/lib/abi";

const DEPOSIT_USDC = 5_000_000n; // 5 USDC
const POLL_INTERVAL_MS = 2_000;
const POLL_TIMEOUT_MS = 180_000;

async function vaultBalanceOf(rpc: string, vault: string, who: string): Promise<bigint> {
  const data = encodeFunctionData({
    abi: vaultAbi,
    functionName: "balanceOf",
    args: [who as Address],
  });
  const res = await fetch(rpc, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "eth_call",
      params: [{ to: vault, data }, "latest"],
    }),
  });
  const j = (await res.json()) as { result?: string };
  return j.result && j.result !== "0x" ? BigInt(j.result) : 0n;
}

/**
 * Sign USDC.approve(vault, amount) + vault.deposit(amount, receiver) and await both.
 *
 * Signed through `sendBufferedTransaction`, never a bare
 * `wallet.sendTransaction()`. This spec signs directly rather than through the
 * injected mock wallet, so it inherits neither that wallet's 1.5x gas buffer
 * nor `Fixture::cast_send`'s. A bare `eth_estimateGas` result is the smallest
 * limit at which the OUTERMOST frame succeeds, so under EIP-150's 63/64 rule
 * the `MetaMorpho.totalAssets()` staticcall three frames inside `_routeDeposit`
 * is left with nothing spare and any movement between estimate and execution
 * runs it out of gas. See `docs/testing/geth-gas-estimation.md` and issue #1388
 * for the measurements. This is robustness only — the deposit's ~1.4M gas cost
 * is issue #1391, and buffering the limit does not reduce it.
 */
async function depositToVault(endpoints: DevnetEndpoints, amount: bigint): Promise<void> {
  const account = privateKeyToAccount(endpoints.admin_private_key as Hex);
  const wallet = createWalletClient({ account, transport: http(endpoints.rpc_url) });
  const publicClient = createPublicClient({ transport: http(endpoints.rpc_url) });

  // Approve
  const approveTx = await sendBufferedTransaction(wallet, publicClient, {
    to: endpoints.usdc_addr as Address,
    data: encodeFunctionData({
      abi: erc20Abi,
      functionName: "approve",
      args: [endpoints.vault_addr as Address, amount],
    }),
  });
  await publicClient.waitForTransactionReceipt({ hash: approveTx, timeout: 60_000 });

  // Deposit
  const depositTx = await sendBufferedTransaction(wallet, publicClient, {
    to: endpoints.vault_addr as Address,
    data: encodeFunctionData({
      abi: vaultAbi,
      functionName: "deposit",
      args: [amount, account.address],
    }),
  });
  await publicClient.waitForTransactionReceipt({ hash: depositTx, timeout: 60_000 });
}

async function waitUntil<T>(predicate: () => Promise<T | null>, description: string): Promise<T> {
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  while (Date.now() < deadline) {
    const v = await predicate();
    if (v !== null) return v;
    await sleep(POLL_INTERVAL_MS);
  }
  throw new Error(`multi-vault-withdrawal: timed out waiting for ${description}`);
}

/** Format a bigint shares value (6dp) as a decimal string for display comparison. */
function formatShares(raw: bigint): string {
  const whole = raw / 1_000_000n;
  const frac = (raw % 1_000_000n).toString().padStart(6, "0");
  return `${whole}.${frac}`;
}

test.describe("Multi-vault withdrawal — PositionSelector and previewRedeem on smoke-test devnet", () => {
  let endpoints: DevnetEndpoints;

  test.beforeAll(() => {
    endpoints = loadEndpoints();
  });

  test("select position from PositionSelector, preview redeem, sign, assert balance decremented", async ({
    page,
  }) => {
    const admin = endpoints.admin_addr;

    // Step 1: deposit so the admin holds a non-zero receipt balance.
    // Admin EOA is pre-funded with USDC via DappStack::boot (issue #603).
    await depositToVault(endpoints, DEPOSIT_USDC);

    const sharesBefore = await vaultBalanceOf(endpoints.rpc_url, endpoints.vault_addr, admin);
    expect(sharesBefore).toBeGreaterThan(0n);

    // Step 2: intercept GET /v1/accounts/:address/positions to return the
    // vault position without needing indexer freshness.
    await page.route(`${endpoints.explorer_api_url}/v1/accounts/${admin}/positions`, (route) => {
      void route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          positions: [
            {
              vault_addr: endpoints.vault_addr,
              vault_name: "RobotMoney USDC Vault",
              shares: formatShares(sharesBefore),
            },
          ],
        }),
      });
    });

    // Step 3: open dapp and navigate to Deposit & Withdraw tab.
    await openDapp(page, endpoints, { role: "admin" });
    await openTab(page, "deposit-withdraw");

    // Step 4: PositionSelector renders the intercepted position.
    const positionSelector = page.getByTestId("position-selector");
    await expect(positionSelector).toBeVisible({ timeout: 15_000 });

    // Step 5: click the vault radio — should pre-fill the shares input.
    // Scope to positionSelector so DestinationSelector radios (rendered above
    // the deposit section) don't shadow this selector.
    const positionRadio = positionSelector.getByRole("radio").first();
    await expect(positionRadio).toBeVisible({ timeout: 5_000 });
    await positionRadio.click();

    // Step 6: the withdraw amount input should be pre-filled with the share balance.
    const withdrawInput = page.getByTestId("withdraw-amount");
    await expect(withdrawInput).toHaveValue(formatShares(sharesBefore), { timeout: 5_000 });

    // Step 7: previewRedeem hint appears (live chain call resolves).
    const previewHint = page.getByTestId("withdraw-preview-redeem");
    await expect(previewHint).toBeVisible({ timeout: 30_000 });
    await expect(previewHint).toContainText("USDC");

    // Step 8: the TxPreview block renders the redeem calldata.
    const redeemPreviewFn = page.getByTestId("withdraw-form").getByTestId("tx-preview-fn");
    await expect(redeemPreviewFn).toContainText("redeem", { timeout: 10_000 });

    // Step 9: sign the withdraw.
    const withdrawSubmit = page.getByTestId("withdraw-submit");
    await expect(withdrawSubmit).toBeEnabled({ timeout: 30_000 });
    await withdrawSubmit.click();

    // Step 10: wait for receipt balance to decrease.
    const sharesAfter = await waitUntil(async () => {
      const cur = await vaultBalanceOf(endpoints.rpc_url, endpoints.vault_addr, admin);
      return cur < sharesBefore ? cur : null;
    }, "vault.balanceOf(admin) to decrease after multi-vault redeem");
    expect(sharesAfter).toBeLessThan(sharesBefore);
  });
});
