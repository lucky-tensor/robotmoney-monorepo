#!/usr/bin/env bash
# Canonical: docs/development/smoke-test-design.md (Devnet section).
# Implements: issue #255 — CI guard around the fork-block manifest.
#
# Validates testing/ethereum-testnet/config/fork-block.json against the
# rules in smoke_test::fork_manifest, and exercises the genesis-alloc
# ingester end-to-end against the committed Anvil fixture. Both checks
# run via the smoke-test crate's CLI binaries — so this script is a
# thin wrapper meant to be invoked from CI workflows or by hand from a
# developer shell.
#
# Exit codes:
#   0 — manifest valid AND ingester produced a non-empty alloc including
#       the canonical Base USDC address.
#   non-zero — see the per-binary exit codes in
#       testing/smoke-test/src/bin/fork-manifest-validate.rs and
#       testing/smoke-test/src/bin/genesis-ingester.rs.
#
# Usage:
#   bash scripts/devnet/check-fork-manifest.sh                  # validate only
#   bash scripts/devnet/check-fork-manifest.sh --require-pinned # release gate
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

REQUIRE_PINNED_FLAG=""
if [ "${1:-}" = "--require-pinned" ]; then
  REQUIRE_PINNED_FLAG="--require-pinned"
fi

MANIFEST="$REPO_ROOT/testing/ethereum-testnet/config/fork-block.json"
SNAPSHOT="$REPO_ROOT/testing/fixtures/fork-state/CURRENT.anvil-state"
STATE_MANIFEST="$REPO_ROOT/testing/fixtures/fork-state/CURRENT.json"

# sha256 integrity binding (ADR-0011 addendum, issue #1152): recompute the
# digest of the raw blob bytes and compare against the manifest's
# state_sha256 field before anything downstream (the ingester, then any fork
# suite via run-golden-forge-forks.sh) consumes the fixture. Missing field or
# mismatch is a loud, non-zero failure — never a warn-and-continue.
echo "[check-fork-manifest] verifying fork-state blob integrity (state_sha256)"
"$REPO_ROOT/scripts/devnet/fork-state-digest.sh" verify "$SNAPSHOT" "$STATE_MANIFEST"

# Pin-age visibility (issue #1386). The digest check above proves the blob
# matches its manifest; it says nothing about whether the pin still describes
# anything like the present. The devnet's chain clock is wall-clock now while
# the forked Aave/Compound/Morpho state is frozen at the pin, so the simulated
# accrual interval grows by a day per day — and until now nothing reported it,
# which is how the pin reached 48 days unnoticed. Reported (and annotated past
# the refresh cadence) on every run, but deliberately NOT a hard failure here:
# this script is on the pull-request path and a stale pin is a maintenance
# signal, not a reason to red the merge queue. The hard gate lives in the
# nightly live-base-fork-drift job, which passes --max-age-days.
echo "[check-fork-manifest] reporting fork pin age"
"$REPO_ROOT/scripts/devnet/check-fork-pin-age.sh"

# Build the validator + ingester binaries once. Reuses the smoke-test
# crate's existing cargo cache.
echo "[check-fork-manifest] cargo build (smoke-test binaries)"
cargo build \
  --manifest-path "$REPO_ROOT/testing/smoke-test/Cargo.toml" \
  --bin smoke-test-fork-manifest-validate \
  --bin smoke-test-genesis-ingester \
  --quiet

# Resolve the Cargo target directory dynamically so the script works when
# CARGO_TARGET_DIR is overridden (e.g. by Swatinem/rust-cache@v2 in CI).
TARGET_DIR="$(cargo metadata \
  --manifest-path "$REPO_ROOT/testing/smoke-test/Cargo.toml" \
  --format-version=1 \
  --no-deps \
  | jq -r .target_directory)"

VALIDATOR="$TARGET_DIR/debug/smoke-test-fork-manifest-validate"
INGESTER="$TARGET_DIR/debug/smoke-test-genesis-ingester"

echo "[check-fork-manifest] validating $MANIFEST"
"$VALIDATOR" --manifest "$MANIFEST" $REQUIRE_PINNED_FLAG

# Ingester guard: produce alloc and assert canonical USDC is present and
# carries non-empty bytecode. Mirrors issue #255's acceptance criterion
# "geth genesis.json contains canonical Base USDC proxy with non-zero
# bytecode hash".
ALLOC_OUT="$(mktemp -t fork-alloc.XXXXXX.json)"
trap 'rm -f "$ALLOC_OUT"' EXIT

echo "[check-fork-manifest] running ingester against $SNAPSHOT"
"$INGESTER" --manifest "$MANIFEST" --snapshot "$SNAPSHOT" --output "$ALLOC_OUT"

USDC_LOWER="0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
if ! jq -e --arg a "$USDC_LOWER" '.[$a] | .code | length > 2' "$ALLOC_OUT" >/dev/null; then
  echo "ERROR: produced alloc lacks non-empty bytecode for canonical USDC $USDC_LOWER" >&2
  exit 10
fi

HARNESS_LOWER="0xae67a1b2a267a124cf762098e3cbf6b03329e6d5"
if ! jq -e --arg a "$HARNESS_LOWER" '.[$a].balance' "$ALLOC_OUT" >/dev/null; then
  echo "ERROR: produced alloc lacks ETH grant for HARNESS_USDC_HOLDER $HARNESS_LOWER" >&2
  exit 11
fi

# Balance-slot guard: USDC entry must carry the balances[holder] slot AND
# the totalSupply slot (slot 1). The slot index for balances[holder] is
# computed at test time so this guard catches any drift in the FiatTokenV2_1
# storage-layout constant inside genesis_alloc.rs.
BAL_SLOT=$(cast index address "$HARNESS_LOWER" 9)
TS_SLOT="0x0000000000000000000000000000000000000000000000000000000000000001"
if ! jq -e --arg a "$USDC_LOWER" --arg s "$BAL_SLOT" '.[$a].storage[$s]' "$ALLOC_OUT" >/dev/null; then
  echo "ERROR: USDC alloc entry is missing balances[holder] slot $BAL_SLOT" >&2
  exit 12
fi
if ! jq -e --arg a "$USDC_LOWER" --arg s "$TS_SLOT" '.[$a].storage[$s]' "$ALLOC_OUT" >/dev/null; then
  echo "ERROR: USDC alloc entry is missing totalSupply slot $TS_SLOT" >&2
  exit 13
fi

echo "[check-fork-manifest] OK"
