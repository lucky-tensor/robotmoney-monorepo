/**
 * VaultDetail composition panel for a ProtocolAssetVault (issue #1364).
 *
 * `ProtocolAssetVault`'s risk label is VOLATILE, so
 * `VaultDetail.tsx`'s `CompositionSection` classifies it as a basket vault and
 * renders `BasketShortlistPanel`, which calls `shortlist()`. Only
 * `AgentTokenVault` declared that function, so the call reverted on every
 * `ProtocolAssetVault` and the panel fell into its
 * `vault-detail-composition-error` branch — "unavailable", permanently, and
 * quietly.
 *
 * WHY THIS TEST IS NOT A MOCK-ONLY TAUTOLOGY
 * A test that hand-writes a five-array tuple and feeds it to a mocked
 * `useReadContract` proves only that the renderer can zip arrays — it would
 * have passed just as happily while the contract had no `shortlist()` at all.
 * So the fixture here is not hand-written: it is ABI-ENCODED with the canonical
 * `ProtocolAssetVault` Foundry artifact (`protocolAssetVaultAbiGenerated`, which
 * `.github/scripts/generate_abi_bindings.sh` re-derives from `out/` and
 * suite-16 drift-gates) and then DECODED with the hand-maintained fragment the
 * dapp actually calls with (`BASKET_VAULT_SHORTLIST_ABI`). If the compiled
 * contract does not declare `shortlist()`, `encodeFunctionResult` throws and
 * this file fails at import. If the two ABIs disagree on the return shape,
 * `decodeFunctionResult` throws or yields garbage and the row assertions fail.
 *
 * Canonical: docs/prd.md §11.2, §11.3 — basket vault composition.
 */
import { describe, it, expect, vi } from "vitest";
import { decodeFunctionResult, encodeFunctionResult } from "viem";
import { render, waitFor } from "./helpers/render";
import { VaultDetail } from "../../src/components/VaultDetail";
import { BASKET_VAULT_SHORTLIST_ABI } from "../../src/lib/abi";
import { protocolAssetVaultAbiGenerated } from "../../src/lib/abi.generated";
import type { FetchLike, VaultDetailResponse } from "../../src/lib/explorerApi";

// Base mainnet wETH / cbBTC — the assets ProtocolAssetVault's NatSpec names.
const WETH = "0x4200000000000000000000000000000000000006" as `0x${string}`;
const CBBTC = "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf" as `0x${string}`;
const WETH_POOL = "0x1111111111111111111111111111111111111111" as `0x${string}`;
const CBBTC_POOL = "0x2222222222222222222222222222222222222222" as `0x${string}`;
const VAULT_ADDR = "0x3333333333333333333333333333333333333333";

/**
 * The five parallel arrays a live `ProtocolAssetVault.shortlist()` returns:
 * (address[] tokens, address[] pools, uint24[] fees, bool[] active,
 *  uint256[] balances). The second asset is inactive with a residual balance —
 * the removed-but-still-held case `BasketVault.removeAsset` produces.
 */
const ON_CHAIN_RETURN = [
  [WETH, CBBTC],
  [WETH_POOL, CBBTC_POOL],
  [500, 3000],
  [true, false],
  [12_500_000n, 750_000n],
] as const;

/**
 * Round-trip through the real ABIs. Throws at import if the canonical
 * `ProtocolAssetVault` artifact has no `shortlist()` — which is exactly the
 * defect #1364 fixes — or if the hand-maintained dapp fragment cannot decode
 * what the contract encodes.
 */
const ENCODED = encodeFunctionResult({
  abi: protocolAssetVaultAbiGenerated,
  functionName: "shortlist",
  result: ON_CHAIN_RETURN as unknown as never,
});

const DECODED = decodeFunctionResult({
  abi: BASKET_VAULT_SHORTLIST_ABI,
  functionName: "shortlist",
  data: ENCODED,
});

// `useReadContract` is only dereferenced at render time, so referencing
// DECODED inside the factory is safe despite vi.mock hoisting.
vi.mock("wagmi", () => ({
  useReadContract: (opts: { functionName?: string }) => {
    if (opts.functionName === "shortlist") {
      return { data: DECODED, isError: false, isLoading: false };
    }
    return { data: undefined, isError: false, isLoading: false };
  },
  createConfig: () => ({}),
  http: () => ({}),
  fallback: (...args: unknown[]) => args[0],
  unstable_connector: () => ({}),
}));

function makeFetch(body: unknown): FetchLike {
  return vi.fn(async () => ({
    ok: true as const,
    status: 200,
    json: async () => body,
  })) as unknown as FetchLike;
}

/** An Active, VOLATILE vault — exactly how the indexer labels ProtocolAssetVault. */
function protocolAssetVaultFixture(): VaultDetailResponse {
  return {
    vault: {
      chain_id: 8453,
      address: VAULT_ADDR,
      name: "Robot Money Protocol",
      risk_label: "VOLATILE",
      status: 0,
      deposit_cap: "1000000000",
      tvl_history: [],
      indexed_at: "2026-01-01T12:00:00Z",
    },
    block_number: 1000,
    indexed_at: "2026-01-01T12:00:00Z",
  };
}

describe("ProtocolAssetVault shortlist() ABI (issue #1364)", () => {
  it("the canonical Foundry artifact declares shortlist() with five parallel arrays", () => {
    const entry = (protocolAssetVaultAbiGenerated as readonly Record<string, unknown>[]).find(
      (e) => e.type === "function" && e.name === "shortlist",
    );
    expect(
      entry,
      "ProtocolAssetVault must declare shortlist(); without it the dapp composition " +
        "panel renders 'unavailable' for every ProtocolAssetVault (issue #1364)",
    ).toBeDefined();

    const outputs = (entry as { outputs: { type: string }[] }).outputs;
    expect(outputs.map((o) => o.type)).toEqual([
      "address[]",
      "address[]",
      "uint24[]",
      "bool[]",
      "uint256[]",
    ]);
    expect((entry as { inputs: unknown[] }).inputs).toHaveLength(0);
  });

  it("contract-encoded shortlist() data decodes with the dapp's hand-maintained fragment", () => {
    expect(DECODED).toEqual(ON_CHAIN_RETURN);
  });

  /**
   * Negative control. Proves the round-trip above is load-bearing rather than a
   * formality: run it against the PRE-#1364 shape of the artifact (the same ABI
   * with `shortlist()` removed) and it throws. A test that could not fail is not
   * evidence, and the whole reason this defect survived is that the dapp's only
   * signal was a quietly degraded panel.
   */
  it("the same round-trip throws against an artifact with no shortlist() (pre-#1364)", () => {
    const preFix = (protocolAssetVaultAbiGenerated as readonly { name?: string }[]).filter(
      (e) => e.name !== "shortlist",
    );
    expect(preFix.length).toBe(protocolAssetVaultAbiGenerated.length - 1);
    expect(() =>
      encodeFunctionResult({
        abi: preFix,
        functionName: "shortlist",
        result: ON_CHAIN_RETURN as unknown as never,
      }),
    ).toThrow();
  });
});

describe("VaultDetail composition — ProtocolAssetVault renders rows, not 'unavailable'", () => {
  it("renders one row per basket asset with address, active flag and balance", async () => {
    const { getByTestId, getAllByTestId, queryByTestId } = render(
      <VaultDetail
        apiUrl="http://api"
        address={VAULT_ADDR}
        fetchImpl={makeFetch(protocolAssetVaultFixture())}
      />,
    );
    await waitFor(() => expect(getByTestId("vault-detail-composition")).toBeTruthy());

    // The regression: before #1364 this branch is what rendered.
    expect(
      queryByTestId("vault-detail-composition-error"),
      "composition panel must not fall into its 'unavailable' branch",
    ).toBeNull();
    expect(queryByTestId("vault-detail-composition-loading")).toBeNull();
    expect(
      queryByTestId("vault-detail-composition-empty"),
      "a configured basket must not render as empty",
    ).toBeNull();
    // A basket vault must not get the STABLE_YIELD / RWA static label either.
    expect(queryByTestId("vault-detail-composition-label")).toBeNull();

    const rows = getAllByTestId("vault-detail-composition-entry");
    expect(rows).toHaveLength(2);

    const addrs = getAllByTestId("vault-detail-composition-address").map((n) => n.textContent);
    expect(addrs[0]).toBe(`0x...${WETH.slice(-6).toUpperCase()}`);
    expect(addrs[1]).toBe(`0x...${CBBTC.slice(-6).toUpperCase()}`);

    const actives = getAllByTestId("vault-detail-composition-active").map((n) => n.textContent);
    expect(actives).toEqual(["active", "inactive"]);

    const balances = getAllByTestId("vault-detail-composition-balance").map((n) => n.textContent);
    expect(balances).toHaveLength(2);
    // formatTokenBalance(12_500_000n, 6) — a real number, not a placeholder.
    expect(balances[0]).toContain("12.5");
  });
});
