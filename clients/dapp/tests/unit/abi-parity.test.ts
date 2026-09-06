/**
 * abi.ts <-> abi.generated.ts parity check (issue #1281).
 *
 * suite-16-abi-drift.yml regenerates `abi.generated.ts` from the Foundry
 * `out/` artifacts and fails if it drifts from the committed copy — but the
 * dapp actually encodes/decodes every on-chain call with the hand-maintained
 * `src/lib/abi.ts` (`abi.ts`'s own header: "Canonical: none — hand-maintained
 * ABI surface; mirrors contracts/"), which that CI suite never reads. Nothing
 * previously caught `abi.ts` drifting from the real contracts.
 *
 * That gap already bit: `routerAbi.activeVaults` in `abi.ts` named a
 * selector no Solidity source in the repo ever defined. This test closes the
 * gap by comparing every function/event/error `abi.ts` exports against the
 * canonical signatures Foundry produced in `abi.generated.ts`, and fails on
 * any entry with no exact match.
 *
 * "Exact match" here means the canonical Solidity signature — name plus
 * positional parameter types (nested tuples expanded recursively) — for
 * inputs, and the same for outputs. This is deliberately what determines
 * 4-byte selector / return-data ABI compatibility; it does NOT include
 * `stateMutability` (e.g. `view` vs `pure`), which is a compiler-level
 * annotation with no bearing on whether a client can correctly encode a call
 * or decode its result.
 *
 * Coverage note: this test now has NO carve-outs. `BASKET_VAULT_SHORTLIST_ABI`
 * was the last one — it had no canonical counterpart at all, because
 * `.github/scripts/generate_abi_bindings.sh` generated bindings for neither
 * basket vault. Issue #1346 extended the generator to emit
 * `agentTokenVaultAbiGenerated`, so `shortlist()` is now compared against real
 * Foundry output like every other binding. `AgentTokenVault` is the counterpart
 * because it is the only contract that declares `shortlist()`;
 * `ProtocolAssetVault` does not, which is its own defect (issue #1364).
 *
 * `registryAbi` (previously excluded here for its own real, pre-existing
 * `getVault` drift from `VaultRegistry.sol` — issue #1348) is fixed and
 * included below.
 */
import { describe, it, expect } from "vitest";
import {
  gatewayAbi,
  erc20Abi,
  vaultAbi,
  routerAbi,
  registryAbi,
  BASKET_VAULT_SHORTLIST_ABI,
} from "../../src/lib/abi";
import {
  gatewayAbiGenerated,
  erc20AbiGenerated,
  robotMoneyVaultAbiGenerated,
  routerAbiGenerated,
  registryAbiGenerated,
  agentTokenVaultAbiGenerated,
} from "../../src/lib/abi.generated";

interface AbiParam {
  type: string;
  components?: readonly AbiParam[];
}

interface AbiEntry {
  type: string;
  name?: string;
  inputs?: readonly AbiParam[];
  outputs?: readonly AbiParam[];
}

/** Recursively render a parameter's canonical type, expanding tuples. */
function typeSignature(param: AbiParam): string {
  if (param.type.startsWith("tuple")) {
    const arraySuffix = param.type.slice("tuple".length);
    const components = param.components ?? [];
    return `(${components.map(typeSignature).join(",")})${arraySuffix}`;
  }
  return param.type;
}

function paramsSignature(params: readonly AbiParam[] | undefined): string {
  return (params ?? []).map(typeSignature).join(",");
}

/** Canonical signature: `type name(inputTypes) -> (outputTypes)`. Ignores stateMutability. */
function canonicalSignature(entry: AbiEntry): string {
  const inputs = paramsSignature(entry.inputs);
  if (entry.type === "function") {
    return `function ${entry.name}(${inputs}) -> (${paramsSignature(entry.outputs)})`;
  }
  return `${entry.type} ${entry.name}(${inputs})`;
}

const CHECKED_TYPES = new Set(["function", "event", "error"]);

interface AbiPair {
  label: string;
  handMaintained: readonly unknown[];
  canonical: readonly unknown[];
}

const PAIRS: readonly AbiPair[] = [
  { label: "gatewayAbi", handMaintained: gatewayAbi, canonical: gatewayAbiGenerated },
  { label: "erc20Abi", handMaintained: erc20Abi, canonical: erc20AbiGenerated },
  { label: "vaultAbi", handMaintained: vaultAbi, canonical: robotMoneyVaultAbiGenerated },
  { label: "routerAbi", handMaintained: routerAbi, canonical: routerAbiGenerated },
  { label: "registryAbi", handMaintained: registryAbi, canonical: registryAbiGenerated },
  {
    label: "BASKET_VAULT_SHORTLIST_ABI",
    handMaintained: BASKET_VAULT_SHORTLIST_ABI,
    canonical: agentTokenVaultAbiGenerated,
  },
];

describe("abi.ts <-> abi.generated.ts parity (issue #1281)", () => {
  for (const { label, handMaintained, canonical } of PAIRS) {
    const handEntries = (handMaintained as AbiEntry[]).filter((e) => CHECKED_TYPES.has(e.type));
    const canonicalEntries = (canonical as AbiEntry[]).filter((e) => CHECKED_TYPES.has(e.type));

    describe(label, () => {
      it.each(handEntries.map((entry) => [`${entry.type} ${entry.name}`, entry] as const))(
        "%s has an exact canonical-signature match in abi.generated.ts",
        (_title, entry) => {
          const match = canonicalEntries.find(
            (c) => c.type === entry.type && c.name === entry.name,
          );
          expect(
            match,
            `${label}.${entry.name}: no ${entry.type} named "${entry.name}" exists in the ` +
              `canonical Foundry ABI (abi.generated.ts) — this selector is not real`,
          ).toBeDefined();
          expect(canonicalSignature(entry)).toBe(canonicalSignature(match as AbiEntry));
        },
      );
    });
  }
});
