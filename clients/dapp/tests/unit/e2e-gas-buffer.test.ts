// Guard for issue #1388: every e2e spec that signs a devnet transaction
// directly must buffer its gas estimate.
//
// viem fills a missing `gas` field with the raw `eth_estimateGas` result, and
// that result is the SMALLEST limit at which the outermost frame succeeds —
// under EIP-150's 63/64 rule the deepest frame therefore has zero usable
// margin, however large the outer limit looks. Measured on the committed fork
// fixture, the 5 USDC `vault.deposit` estimates at 1,415,397 but consumes only
// 1,225,785 on success; sending it with anything below the full estimate runs
// `MetaMorpho.totalAssets()` out of gas three frames down. See
// `docs/testing/geth-gas-estimation.md`.
//
// `.eslintrc.cjs` bans the bare call as a lint error. This test asserts the
// same invariant from the vitest side, because `dapp-e2e` is a slow,
// devnet-dependent job and this is the fast check that actually runs on every
// PR. Runs in the vitest `node` project (filesystem access).
//
// This is test robustness, not a fix for the deposit's gas cost. That cost is
// issue #1391 — `_targetBpsFor()` floors `MAX_BPS / 3` to 3333, so the adapter
// targets sum to 9999 and `_routeDeposit`'s pass 1 places nothing for a
// balanced fully-deployed vault, making every deposit pay for two rounds of
// adapter staticcalls. Buffering the limit does not remove that.
import { describe, it, expect } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { bufferGas, GAS_BUFFER_NUMERATOR, GAS_BUFFER_DENOMINATOR } from "../e2e/helpers/gas";

const E2E_DIR = fileURLToPath(new URL("../e2e", import.meta.url));

/** The estimate and the successful consumption measured on the fork fixture. */
const MEASURED_ESTIMATE = 1_415_397n;
const MEASURED_GAS_USED = 1_225_785n;

describe("e2e gas buffer (issue #1388)", () => {
  it("matches the 1.5x buffer the mock wallet and Fixture::cast_send apply", () => {
    expect(GAS_BUFFER_NUMERATOR).toBe(3n);
    expect(GAS_BUFFER_DENOMINATOR).toBe(2n);
    expect(bufferGas(1_000_000n)).toBe(1_500_000n);
  });

  it("clears the whole measured gas range for the routed deposit", () => {
    // The same deposit ranges 803,827 -> 1,414,199 gas purely as a function of
    // adapter balances (issue #1386). The buffered limit must clear the top of
    // that range with room, or the buffer is decorative.
    expect(bufferGas(MEASURED_ESTIMATE)).toBeGreaterThan(1_414_199n);
  });

  it("would not have absorbed the failure without the buffer", () => {
    // Sanity-check the premise: the bare estimate does NOT clear the observed
    // worst case, which is exactly why the unbuffered specs went out of gas.
    expect(MEASURED_ESTIMATE).toBeLessThan(1_414_199n + 2_000n);
    expect(MEASURED_ESTIMATE).toBeGreaterThan(MEASURED_GAS_USED);
  });

  it("no e2e spec calls walletClient.sendTransaction() directly", () => {
    const specs = readdirSync(E2E_DIR).filter((f) => f.endsWith(".spec.ts"));
    expect(specs.length).toBeGreaterThan(0);
    const offenders = specs.filter((f) => {
      const src = readFileSync(`${E2E_DIR}/${f}`, "utf8");
      // Strip comments so prose about the old pattern does not trip the guard.
      const code = src.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
      return /\.sendTransaction\s*\(/.test(code);
    });
    expect(
      offenders,
      "These specs forward an unbuffered eth_estimateGas result as their gas limit. " +
        "Use sendBufferedTransaction() from tests/e2e/helpers/gas.ts " +
        "(issue #1388, docs/testing/geth-gas-estimation.md).",
    ).toEqual([]);
  });
});
