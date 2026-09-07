// Canonical: docs/development/false-green-shapes.md

/**
 * Regression cover for the deposit revert decoder (issue #1380).
 *
 * The decoder lives on a failure path that a green CI run never executes, so
 * without this test nothing in CI would ever prove it still decodes anything —
 * a silent-rot shape in its own right. The live end-to-end demonstrations
 * (a forced `PerDepositCapExceeded`, and a forced out-of-gas, both against a
 * real geth node) are recorded on the pull request; this pins the pure
 * classification step, which is where the decode tables can rot when an ABI
 * moves.
 *
 * The cases are chosen to cover every branch a real failure could take: the
 * vault's own guards, the adapters', the two third-party protocols the
 * adapters call into, Aave's require-string codes, an empty payload, and a
 * selector no table knows.
 */
import { describe, it, expect } from "vitest";
import { encodeErrorResult, toFunctionSelector } from "viem";
import { classifyRevertData, strategyAdapterErrorAbi } from "../e2e/helpers/deposit-diagnostics";
import { cometErrorAbi, metaMorphoErrorAbi } from "../e2e/helpers/protocol-errors";
import { robotMoneyVaultAbiGenerated } from "../../src/lib/abi.generated";

describe("classifyRevertData", () => {
  it("names a vault guard error from its 4-byte selector", () => {
    const data = encodeErrorResult({
      abi: robotMoneyVaultAbiGenerated,
      errorName: "PerDepositCapExceeded",
    });
    const got = classifyRevertData(data, "execution reverted");
    expect(got.kind).toBe("named");
    expect(got.summary).toContain("PerDepositCapExceeded");
    expect(got.summary).toContain("RobotMoneyVault");
  });

  it("names an adapter error and renders its arguments", () => {
    const data = encodeErrorResult({
      abi: strategyAdapterErrorAbi,
      errorName: "ExposureCapExceeded",
      args: [1n, 2n, 3n],
    });
    const got = classifyRevertData(data, "execution reverted");
    expect(got.kind).toBe("named");
    expect(got.summary).toContain("ExposureCapExceeded(1, 2, 3)");
    expect(got.summary).toContain("strategy adapter");
  });

  // The adapters hand USDC to real Base protocol code on this devnet, so an
  // error raised there is the likeliest decode target of all — and it arrives
  // as a bare selector with no hint of which contract raised it.
  it("names a Compound V3 Comet error and attributes it to Comet", () => {
    const data = encodeErrorResult({ abi: cometErrorAbi, errorName: "SupplyCapExceeded" });
    const got = classifyRevertData(data, "execution reverted");
    expect(got.kind).toBe("named");
    expect(got.summary).toContain("SupplyCapExceeded()");
    expect(got.summary).toContain("Compound V3 Comet");
  });

  it("names a MetaMorpho error and attributes it to Morpho", () => {
    const data = encodeErrorResult({ abi: metaMorphoErrorAbi, errorName: "AllCapsReached" });
    const got = classifyRevertData(data, "execution reverted");
    expect(got.kind).toBe("named");
    expect(got.summary).toContain("AllCapsReached()");
    expect(got.summary).toContain("MetaMorpho");
  });

  it("resolves an Aave V3 require-string code to its Errors-library name", () => {
    const got = classifyRevertData(
      encodeErrorResult({
        abi: [{ type: "error", name: "Error", inputs: [{ name: "", type: "string" }] }],
        errorName: "Error",
        args: ["51"],
      }),
      "execution reverted",
    );
    expect(got.kind).toBe("named");
    expect(got.summary).toContain("SUPPLY_CAP_EXCEEDED");
  });

  // The distinction the whole issue turns on: empty revert data is an
  // out-of-gas, a selector is a custom error. These two cases must never
  // collapse into the same report.
  it("reports empty revert data as out of gas, not as a custom error", () => {
    for (const empty of [undefined, null, "0x"]) {
      const got = classifyRevertData(empty, "out of gas");
      expect(got.kind).toBe("empty");
      expect(got.summary).toContain("out of gas");
      expect(got.summary).toContain("NOT a custom error");
      expect(got.summary).toContain('"out of gas"');
    }
  });

  it("reports an unrecognised selector verbatim instead of swallowing it", () => {
    const selector = toFunctionSelector("SomeErrorNoTableKnows(uint256)");
    const got = classifyRevertData(selector, "execution reverted");
    expect(got.kind).toBe("unknown");
    expect(got.summary).toContain(selector);
  });

  it("flags data too short to carry a selector as malformed", () => {
    const got = classifyRevertData("0xdead");
    expect(got.kind).toBe("malformed");
  });
});
