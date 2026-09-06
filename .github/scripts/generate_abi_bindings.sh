#!/usr/bin/env bash
# generate_abi_bindings.sh — extract ABI arrays from Foundry artifacts.
#
# Canonical: docs/architecture.md §5.2 — Agent Permissions Gateway (ABI artifact pipeline)
#
# WHY THIS SCRIPT EXISTS
# The Foundry out/ directory is the single canonical source for all contract
# ABIs. Any copy maintained by hand will drift; this script is the authoritative
# re-derivation step. CI runs it and diffs the output so divergence is caught
# before merge (issue #374).
#
# USAGE
#   Run from the repository root after `forge build` has populated out/:
#     bash .github/scripts/generate_abi_bindings.sh
#
# THE TWO BLOCKS BELOW ARE MACHINE-READ (issue #1346)
# `.github/scripts/check_abi_binding_inventory.py`, run by suite-16, parses them.
# It fails CI when an ABI file exists on disk but appears in neither block, when
# an un-gated entry cites no tracking issue, or when the issue it cites is not
# open. Editing the prose is fine; keep the `#   <path> ...` line shape.
#
# FULLY-GENERATED OUTPUTS (CI drift-gated)
#   clients/rust-payment-client/abi/Erc20.json          ← TestERC20 (mint/burn)
#   clients/rust-payment-client/abi/RobotMoneyGateway.json
#   clients/dapp/src/lib/abi.generated.ts
#
# UN-GATED HAND-MAINTAINED FILES (each MUST cite an OPEN tracking issue)
# Measured against the Foundry artifacts in #1346: only two of these are
# actually drifted; the rest are partial excerpts that could be regenerated.
# All seven are tracked by #1362, which stays open until they are.
#   clients/rust-payment-client/abi/MockVault.json — adds paused(), absent from the artifact; rmpc reads real vaults through this test fixture's ABI (tracking issue #1362)
#   clients/rust-payment-client/abi/PortfolioRouter.json — partial excerpt, 5 of 78 entries, no drift (tracking issue #1362)
#   clients/rust-payment-client/abi/RouterGovernance.json — partial excerpt, 17 of 76 entries, no drift (tracking issue #1362)
#   clients/rust-payment-client/abi/VaultRegistry.json — stale nine-field VaultRecord getVault; breaks rmpc get-vaults (tracking issue #1362)
#   clients/rust-payment-client/abi/ConsensusRecommendationReceipt.json — partial excerpt, only the reads `rmpc governance draft-proposal` needs (tracking issue #1362)
#   clients/rust-payment-client/abi/InvestmentCommitteePolicy.json — signature-equivalent to the artifact; regeneratable today (tracking issue #1362)
#   clients/rust-payment-client/abi/TimelockController.json — partial excerpt, 16 of 44 entries, no drift (tracking issue #1362)
#
# WHY `ProtocolAssetVault` IS NOT GENERATED HERE (issue #1346 AC4)
# The dapp's `BASKET_VAULT_SHORTLIST_ABI` names both basket vaults, so both were
# candidates for generation. Only `AgentTokenVault` is emitted: it is the only
# one that declares `shortlist()` (AgentTokenVault.sol:127 overrides it;
# ProtocolAssetVault adds only maxAssets()). Emitting ProtocolAssetVault would
# add a 175-entry ABI that no hand-maintained dapp binding can be compared
# against. That asymmetry is itself a defect — tracked in issue #1364 — and
# ProtocolAssetVault joins this script when it is fixed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$REPO_ROOT/out"
RUST_ABI="$REPO_ROOT/clients/rust-payment-client/abi"
DAPP_LIB="$REPO_ROOT/clients/dapp/src/lib"

if [[ ! -d "$OUT" ]]; then
  echo "ERROR: Foundry out/ directory not found at $OUT" >&2
  echo "       Run 'forge build' first." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Helper: extract .abi from a Foundry artifact and write compact JSON
# ---------------------------------------------------------------------------
extract_abi() {
  local artifact="$1"
  local dest="$2"
  python3 -c "
import json, sys
with open('$artifact') as f:
    d = json.load(f)
print(json.dumps(d['abi'], indent=2))
" > "$dest"
  echo "  wrote $dest"
}

# ---------------------------------------------------------------------------
# 1. Rust ABI JSON files (fully generated — CI drift-gated)
# ---------------------------------------------------------------------------
echo "==> Generating Rust ABI JSON files (drift-gated)..."

# Erc20.json maps to TestERC20 (adds mint/burn used by the test fixture)
extract_abi "$OUT/TestERC20.sol/TestERC20.json"                  "$RUST_ABI/Erc20.json"
extract_abi "$OUT/RobotMoneyGateway.sol/RobotMoneyGateway.json"  "$RUST_ABI/RobotMoneyGateway.json"

# ---------------------------------------------------------------------------
# 2. TypeScript generated ABI file for the dapp (fully generated — CI drift-gated)
# ---------------------------------------------------------------------------
echo "==> Generating dapp TypeScript ABI bindings..."

python3 - "$OUT" "$DAPP_LIB/abi.generated.ts" <<'PYEOF'
import json
import sys

out_dir = sys.argv[1]
dest = sys.argv[2]

def load_abi(path):
    with open(path) as f:
        return json.load(f)["abi"]

def abi_to_ts(abi):
    """Render a JSON ABI array as a TypeScript as const literal."""
    return json.dumps(abi, indent=2)

gateway_abi      = load_abi(f"{out_dir}/RobotMoneyGateway.sol/RobotMoneyGateway.json")
erc20_abi        = load_abi(f"{out_dir}/TestERC20.sol/TestERC20.json")
vault_abi        = load_abi(f"{out_dir}/MockVault.sol/MockVault.json")
robot_vault_abi  = load_abi(f"{out_dir}/RobotMoneyVault.sol/RobotMoneyVault.json")
registry_abi     = load_abi(f"{out_dir}/VaultRegistry.sol/VaultRegistry.json")
router_abi       = load_abi(f"{out_dir}/PortfolioRouter.sol/PortfolioRouter.json")
agent_vault_abi  = load_abi(f"{out_dir}/AgentTokenVault.sol/AgentTokenVault.json")

content = f"""\
// THIS FILE IS AUTO-GENERATED — DO NOT EDIT BY HAND.
// Re-generate with: bash .github/scripts/generate_abi_bindings.sh
// Source: contracts/out/ (Foundry build artifacts)
// Issue: #374

/**
 * Full RobotMoneyGateway ABI — generated from Foundry artifact.
 * abi.ts does not import from this file — it hand-maintains its own excerpt
 * (see abi.ts's header). This file's canonical signatures are instead
 * cross-checked against abi.ts's exports by
 * clients/dapp/tests/unit/abi-parity.test.ts (issue #1281), which fails on
 * any abi.ts entry that has no exact match here.
 */
export const gatewayAbiGenerated = {abi_to_ts(gateway_abi)} as const;

/**
 * Full TestERC20 ABI (standard ERC-20 + mint/burn) — generated from Foundry artifact.
 */
export const erc20AbiGenerated = {abi_to_ts(erc20_abi)} as const;

/**
 * Full MockVault ABI — generated from Foundry artifact.
 */
export const vaultAbiGenerated = {abi_to_ts(vault_abi)} as const;

/**
 * Full RobotMoneyVault ABI — generated from Foundry artifact.
 *
 * Includes the adapter allowlist governance surface (setAdapterAllowed,
 * setAdapterCodeHashAllowed, AdapterAllowedSet, AdapterCodeHashAllowedSet).
 * Dapp admin panels drive the on-chain guard through this typed binding.
 * Canonical: docs/technical/security-model.md, issue #444.
 */
export const robotMoneyVaultAbiGenerated = {abi_to_ts(robot_vault_abi)} as const;

/**
 * Full VaultRegistry ABI — generated from Foundry artifact.
 */
export const registryAbiGenerated = {abi_to_ts(registry_abi)} as const;

/**
 * Full PortfolioRouter ABI — generated from Foundry artifact.
 */
export const routerAbiGenerated = {abi_to_ts(router_abi)} as const;

/**
 * Full AgentTokenVault ABI — generated from Foundry artifact.
 *
 * This is the canonical counterpart for `abi.ts`'s hand-maintained
 * `BASKET_VAULT_SHORTLIST_ABI`, which had none until issue #1346 — leaving it
 * the last binding `abi-parity.test.ts` could not cover. `shortlist()` returns
 * five parallel arrays, not a struct array; getting that wrong was scan
 * finding DAPP-5.
 *
 * `ProtocolAssetVault` is deliberately absent: it does not declare
 * `shortlist()` at all (issue #1364). See this script's header.
 */
export const agentTokenVaultAbiGenerated = {abi_to_ts(agent_vault_abi)} as const;
"""

with open(dest, "w") as f:
    f.write(content)

print(f"  wrote {dest}")
PYEOF

# Format the generated TypeScript file with Prettier so it passes the dapp
# fmt check (suite-09).  Requires the dapp dependencies to be installed; if
# they are not, the script skips formatting and prints a reminder.
PRETTIER_BIN="$REPO_ROOT/clients/dapp/node_modules/.bin/prettier"
if [[ -x "$PRETTIER_BIN" ]]; then
  "$PRETTIER_BIN" --write "$DAPP_LIB/abi.generated.ts" >/dev/null
  echo "  formatted $DAPP_LIB/abi.generated.ts with Prettier"
else
  echo "  WARNING: Prettier not found at $PRETTIER_BIN"
  echo "           Run 'bun install' inside clients/dapp/ then re-run this script"
  echo "           to produce a Prettier-compliant abi.generated.ts."
fi

echo ""
echo "ABI generation complete."
echo ""
echo "Drift-gated files (CI checks these):"
echo "  clients/rust-payment-client/abi/Erc20.json"
echo "  clients/rust-payment-client/abi/RobotMoneyGateway.json"
echo "  clients/dapp/src/lib/abi.generated.ts"
echo ""
echo "NOTE: seven files under clients/rust-payment-client/abi/ are NOT regenerated"
echo "      here — they are hand-maintained excerpts or carry entries the Rust"
echo "      client needs at compile time. They are listed in this script's header"
echo "      and every one of them must cite an OPEN tracking issue; the inventory"
echo "      is enforced by .github/scripts/check_abi_binding_inventory.py in"
echo "      suite-16. They are tracked by issue #1362."
