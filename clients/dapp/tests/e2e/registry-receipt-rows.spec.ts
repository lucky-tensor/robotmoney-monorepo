// Canonical: docs/architecture.md §5.3 — Human Dapp

/**
 * Live registry-decode coverage — BalancesPanel receipt rows (issue #1348).
 *
 * This is the only assertion in the suite that depends on the dapp
 * successfully decoding `VaultRegistry.getVault` against a real chain.
 *
 * Why it is needed: `getVault` returns TWO top-level outputs
 * (`VaultMetadata`, `status`). `abi.ts` previously declared a single 9-field
 * tuple, so every decode failed. `VaultRegistryContext` swallows per-call
 * failures (`if (result.status !== "success") continue`), so a broken decode
 * degrades SILENTLY to an empty vault list — nothing throws, nothing logs.
 * `BalancesPanel` (mounted app-wide, `main.tsx:160`) is the only mounted
 * consumer of the resulting `VaultRecord[]`, and it renders one receipt row
 * per registered vault the wallet holds shares in. So:
 *
 *   decode broken -> vaults = [] -> no receipt rows -> this spec is RED.
 *
 * Every other dapp-e2e assertion about vaults reads the explorer API
 * (`GET /v1/vaults`, `GET /v1/accounts/:address/positions`) rather than the
 * registry, so none of them can catch a registry decode regression.
 * Deliberately assert the receipt ROW, not merely that the panel renders —
 * the panel renders fine with an empty vault list, which is exactly the
 * false-green this spec exists to prevent.
 *
 * The wallet must actually hold shares for the row to be legitimate, and the
 * demo seeder funds derived depositor EOAs rather than the admin EOA, so this
 * spec makes its own signed deposit first rather than depending on state left
 * behind by another spec.
 *
 * That setup deposit must be verified in two ways before the UI assertion can
 * mean anything (issue #1366) — get either wrong and a setup failure is
 * misreported as the registry regression this spec exists to catch:
 *
 *   1. Both transaction receipts are checked for `status === "success"`. viem
 *      resolves `waitForTransactionReceipt` for REVERTED transactions too.
 *   2. The share-balance read is POLLED, never sampled once. A confirmed
 *      receipt does not imply readable state on this devnet — see
 *      `docs/testing/geth-state-lag.md`.
 */
import { setTimeout as sleep } from "node:timers/promises";
import { test, expect } from "@playwright/test";
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
import { openDapp } from "./helpers/wallet";
import { erc20Abi, vaultAbi } from "../../src/lib/abi";

/** 5 USDC (6 decimals) — enough to mint a clearly non-zero share balance. */
const DEPOSIT_USDC = 5_000_000n;
const POLL_INTERVAL_MS = 2_000;
const POLL_TIMEOUT_MS = 180_000;

/**
 * Raw `vault.balanceOf(who)` via eth_call — no wallet needed.
 *
 * Throws on a JSON-RPC error rather than reporting a zero balance (issue
 * #1346). `0n` is a meaningful on-chain fact to every caller here, so a node
 * that refused the call must not be indistinguishable from a wallet holding
 * nothing. Under the poll below that mattered twice over: an errored eth_call
 * would otherwise be read as "not settled yet" and burn the full 180s timeout
 * before failing with "geth state-lag never settled" — naming the wrong cause
 * after the longest possible wait.
 */
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
  const j = (await res.json()) as { result?: string; error?: { message?: string } };
  if (j.error) {
    throw new Error(`eth_call vault.balanceOf reverted or failed: ${j.error.message ?? "unknown"}`);
  }
  return j.result && j.result !== "0x" ? BigInt(j.result) : 0n;
}

/**
 * Poll `predicate` until it returns non-null, or throw naming what never
 * settled. Same shape as multi-vault-withdrawal / router-deposit /
 * vault-deposit-withdraw — see `docs/testing/geth-state-lag.md`.
 */
async function waitUntil<T>(predicate: () => Promise<T | null>, description: string): Promise<T> {
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  while (Date.now() < deadline) {
    const v = await predicate();
    if (v !== null) return v;
    await sleep(POLL_INTERVAL_MS);
  }
  throw new Error(`registry-receipt-rows: timed out waiting for ${description}`);
}

/**
 * Sign USDC.approve(vault, amount) + vault.deposit(amount, admin) and await both.
 *
 * viem resolves `waitForTransactionReceipt` for REVERTED transactions too, so
 * each receipt's `status` is asserted here. Without that, a reverted deposit
 * (paused vault, deposit cap, insufficient USDC) passes silently and only
 * surfaces downstream as a zero share balance — which reads as "the registry
 * decode broke" and points at entirely the wrong defect (issue #1366).
 */
async function depositAsAdmin(endpoints: DevnetEndpoints, amount: bigint): Promise<void> {
  const account = privateKeyToAccount(endpoints.admin_private_key as Hex);
  const wallet = createWalletClient({ account, transport: http(endpoints.rpc_url) });
  const publicClient = createPublicClient({ transport: http(endpoints.rpc_url) });

  const approveTx = await wallet.sendTransaction({
    chain: null,
    to: endpoints.usdc_addr as Address,
    data: encodeFunctionData({
      abi: erc20Abi,
      functionName: "approve",
      args: [endpoints.vault_addr as Address, amount],
    }),
  });
  const approveReceipt = await publicClient.waitForTransactionReceipt({
    hash: approveTx,
    timeout: 60_000,
  });
  expect(
    approveReceipt.status,
    `USDC.approve(${endpoints.vault_addr}, ${amount}) REVERTED (tx=${approveTx}, ` +
      `block=${approveReceipt.blockNumber}) — the deposit below cannot succeed, so this is an ` +
      `approve failure, not a registry-decode failure`,
  ).toBe("success");

  const depositTx = await wallet.sendTransaction({
    chain: null,
    to: endpoints.vault_addr as Address,
    data: encodeFunctionData({
      abi: vaultAbi,
      functionName: "deposit",
      args: [amount, account.address],
    }),
  });
  const depositReceipt = await publicClient.waitForTransactionReceipt({
    hash: depositTx,
    timeout: 60_000,
  });
  expect(
    depositReceipt.status,
    `vault.deposit(${amount}, ${account.address}) on ${endpoints.vault_addr} REVERTED ` +
      `(tx=${depositTx}, block=${depositReceipt.blockNumber}) — likely a paused vault, a deposit ` +
      `cap, or insufficient admin USDC. No shares were minted, so this is a deposit failure, ` +
      `not a registry-decode failure`,
  ).toBe("success");
}

test.describe("VaultRegistry getVault decode — live receipt rows", () => {
  let endpoints: DevnetEndpoints;

  test.beforeAll(() => {
    endpoints = loadEndpoints();
  });

  test("BalancesPanel renders a receipt row for a registry-listed vault the wallet holds", async ({
    page,
  }) => {
    // The primary vault is registered with the registry at deploy time
    // (contracts/script/DeployVaultRegistry.s.sol), so it appears in
    // listVaults() and must therefore decode through getVault().
    await depositAsAdmin(endpoints, DEPOSIT_USDC);

    // Guard against a vacuous UI assertion: confirm on-chain that the wallet
    // really does hold shares, so a missing row means a decode/derivation
    // fault rather than an empty position.
    //
    // This read is POLLED, not sampled once. A confirmed receipt does not mean
    // readable state: for a short window after the deposit is mined, a
    // `latest`-pinned eth_call still resolves against pre-deposit state and
    // returns 0. That is the documented geth read-after-write state-lag class
    // (`docs/testing/geth-state-lag.md`), and sampling once here is what
    // reddened unrelated PRs in issue #1366. The deposit itself is already
    // proven non-reverted by depositAsAdmin's receipt assertions above, and
    // vaultBalanceOf throws rather than returning 0n when the node refuses the
    // call (issue #1346) — so a zero reaching this poll really is the lag and
    // nothing else. Poll until it settles, and fail loudly naming the lag if it
    // never does.
    let reads = 0;
    const shares = await waitUntil(async () => {
      reads += 1;
      const bal = await vaultBalanceOf(
        endpoints.rpc_url,
        endpoints.vault_addr,
        endpoints.admin_addr,
      );
      return bal > 0n ? bal : null;
    }, `vault.balanceOf(${endpoints.admin_addr}) on ${endpoints.vault_addr} to report the shares minted by the confirmed deposit (geth state-lag never settled)`);
    console.log(
      `registry-receipt-rows: admin holds ${shares} shares, visible after ${reads} read(s)` +
        `${reads > 1 ? " — geth state-lag was hit and the poll absorbed it" : ""}.`,
    );

    await openDapp(page, endpoints, { role: "admin" });

    await expect(page.getByTestId("balances-panel")).toBeVisible({ timeout: 30_000 });

    // The discriminating assertion. Address casing follows whatever
    // listVaults() returns (viem checksums it), so match case-insensitively.
    const receiptRow = page.locator(
      `[data-testid="balances-panel-row-receipt-${endpoints.vault_addr}" i]`,
    );
    await expect(
      receiptRow,
      "no receipt row for a registry-listed vault the wallet holds shares in — " +
        "VaultRegistryContext decoded zero vaults, which means registryAbi.getVault " +
        "has drifted from VaultRegistry.sol again (issue #1348)",
    ).toBeVisible({ timeout: 60_000 });

    // The row is populated from a real ERC-4626 read off the vault address
    // derived from the decoded record, so both cells must carry real values.
    const symbol = page.locator(
      `[data-testid="balances-panel-row-receipt-${endpoints.vault_addr}-symbol" i]`,
    );
    const amount = page.locator(
      `[data-testid="balances-panel-row-receipt-${endpoints.vault_addr}-amount" i]`,
    );
    await expect(symbol).not.toBeEmpty();
    await expect(amount).not.toBeEmpty();
    await expect(amount).not.toContainText("undefined");
  });
});
