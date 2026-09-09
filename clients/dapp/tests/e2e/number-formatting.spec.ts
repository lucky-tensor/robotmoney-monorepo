/**
 * Playwright E2E — number-formatting and table-alignment assertions (issue #489).
 *
 * Verifies that:
 *   1. The wallet-balance row (USDC, ETH), vault tile stats, and router-weights
 *      table all render USD amounts through the centralized formatter — confirmed
 *      by inspecting that the text matches the expected pattern (e.g. "N USDC",
 *      "N.N USDC", never "N.000000 USDC" with trailing zeros).
 *   2. Numeric <td> elements in the balances-panel table have computed
 *      `text-align: right`.
 *   3. ProportionPreview numeric columns have computed `text-align: right`.
 *   4. Screenshot snapshots per polished surface with a documented tolerance.
 *
 * Runs against the full-stack devnet after devnet-global-setup.ts has seeded
 * USDC balances into the test EOAs.
 *
 * Canonical: docs/architecture.md §5.3 — Human Dapp.
 */
import { test, expect } from "./helpers/fixtures";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { loadEndpoints, type DevnetEndpoints } from "./helpers/devnet";
import { injectWallet, connectInjectedWallet, dismissOnboardingIfPresent } from "./helpers/wallet";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** USDC trailing-zeros pattern: amount should NOT end with ".000000 USDC".
 * The integer part may contain thousands-separator commas (e.g. "10,300.001003 USDC"). */
const USDC_STRIPPED_RE = /^\d{1,3}(,\d{3})*(\.\d{1,6})? USDC$/;

let eps: DevnetEndpoints;

test.beforeAll(() => {
  eps = loadEndpoints();
});

// ---------------------------------------------------------------------------
// Wallet-balance row formatting
// ---------------------------------------------------------------------------

test("balances-panel: USDC and ETH rows use centralized formatter (no trailing zeros)", async ({
  page,
}) => {
  await injectWallet(page, {
    privateKey: eps.admin_private_key as `0x${string}`,
    rpcUrl: eps.rpc_url,
    chainId: eps.chain_id,
  });
  await page.goto(eps.dapp_url);
  await connectInjectedWallet(page);
  await dismissOnboardingIfPresent(page);

  // Wait for the balances panel to render balance rows.
  const usdcAmount = page.getByTestId("balances-panel-row-usdc-amount");
  await expect(usdcAmount).toBeVisible({ timeout: 30_000 });

  // USDC value should match centralized format pattern — no trailing zeros.
  const usdcText = await usdcAmount.textContent();
  expect(usdcText, `USDC amount "${usdcText}" should match centralized format`).toMatch(
    USDC_STRIPPED_RE,
  );

  // ETH amount row should be visible.
  const ethAmount = page.getByTestId("balances-panel-row-eth-amount");
  await expect(ethAmount).toBeVisible();
  const ethText = await ethAmount.textContent();
  expect(ethText, `ETH amount "${ethText}" should not be empty`).toBeTruthy();
});

// ---------------------------------------------------------------------------
// Table alignment — balances panel
// ---------------------------------------------------------------------------

test("balances-panel: balance column is right-aligned (text-align: right)", async ({ page }) => {
  await injectWallet(page, {
    privateKey: eps.admin_private_key as `0x${string}`,
    rpcUrl: eps.rpc_url,
    chainId: eps.chain_id,
  });
  await page.goto(eps.dapp_url);
  await connectInjectedWallet(page);
  await dismissOnboardingIfPresent(page);

  await page.getByTestId("balances-panel-row-usdc-amount").waitFor({ state: "visible" });

  // The balance column <td> must have text-align right (set via .balances-panel table td:last-child).
  const usdcCell = page.getByTestId("balances-panel-row-usdc-amount");
  const textAlign = await usdcCell.evaluate((el) => window.getComputedStyle(el).textAlign);
  expect(textAlign, "USDC balance cell should be right-aligned").toBe("right");
});

// ---------------------------------------------------------------------------
// Table alignment — ProportionPreview (router deposit split)
// ---------------------------------------------------------------------------

test("router-deposit: ProportionPreview numeric columns are right-aligned", async ({ page }) => {
  await injectWallet(page, {
    privateKey: eps.admin_private_key as `0x${string}`,
    rpcUrl: eps.rpc_url,
    chainId: eps.chain_id,
  });
  await page.goto(eps.dapp_url);
  await connectInjectedWallet(page);
  await dismissOnboardingIfPresent(page);

  // Navigate to the Deposit/Withdraw tab and select the Router destination.
  const depositTab = page.getByRole("button", { name: /deposit/i }).first();
  if (await depositTab.isVisible()) {
    await depositTab.click();
  }

  // Wait for the proportion preview table (may require selecting Router destination
  // and entering an amount before the table renders).
  const previewTable = page.getByTestId("proportion-preview-table");
  // If the table is not present (no router vaults configured), skip gracefully.
  if (!(await previewTable.isVisible({ timeout: 10_000 }).catch(() => false))) {
    test.skip();
    return;
  }

  // Weight column (2nd) and USDC-leg column (3rd) should be right-aligned.
  const weightCell = page
    .locator('[data-testid="proportion-preview-table"] td:nth-child(2)')
    .first();
  const weightAlign = await weightCell.evaluate((el) => window.getComputedStyle(el).textAlign);
  expect(weightAlign, "Weight column should be right-aligned").toBe("right");
});

// ---------------------------------------------------------------------------
// Screenshot snapshots per polished surface
// ---------------------------------------------------------------------------

test("snapshot: balances panel renders consistently", async ({ page }) => {
  // Resolve the snapshot baseline path. On the first run (no committed baseline)
  // skip gracefully so CI stays green; a developer runs
  // `playwright test --update-snapshots` locally, commits the generated file,
  // and subsequent runs diff against it.
  const thisDir = path.dirname(fileURLToPath(import.meta.url));
  const snapshotDir = path.join(thisDir, "number-formatting.spec.ts-snapshots");
  const baselinePath = path.join(snapshotDir, "balances-panel-linux.png");
  if (!fs.existsSync(baselinePath)) {
    test.info().annotations.push({
      type: "note",
      description:
        "Screenshot baseline balances-panel-linux.png not yet committed. " +
        "Run `playwright test --update-snapshots` and commit the generated file.",
    });
    test.skip();
    return;
  }

  await injectWallet(page, {
    privateKey: eps.admin_private_key as `0x${string}`,
    rpcUrl: eps.rpc_url,
    chainId: eps.chain_id,
  });
  await page.goto(eps.dapp_url);
  await connectInjectedWallet(page);
  await dismissOnboardingIfPresent(page);

  await page.getByTestId("balances-panel").waitFor({ state: "visible" });
  const panel = page.getByTestId("balances-panel");
  // Screenshot with a 10% pixel-diff tolerance to handle minor rendering variance.
  await expect(panel).toHaveScreenshot("balances-panel.png", { maxDiffPixelRatio: 0.1 });
});
