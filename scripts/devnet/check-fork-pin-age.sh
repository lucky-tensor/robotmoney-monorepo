#!/usr/bin/env bash
# Report — and optionally gate on — the age of the committed Base fork pin.
#
# Canonical: docs/development/smoke-test-design.md (Devnet section),
#            docs/technical/full-stack-devnet.md §"Fork-state fixture".
# Issue:     #1386.
#
# WHY THIS EXISTS
# The devnet's chain clock is wall-clock `now`
# (testing/ethereum-testnet/config/genesis/generate.sh, and
# smoke-test's `ensure_genesis_timestamp`), while the Aave V3 / Compound V3 /
# Morpho state the three adapters call is frozen at the pinned Base block
# captured in testing/fixtures/fork-state/CURRENT.json. Those protocols accrue
# interest as a function of `block.timestamp - lastUpdateTimestamp`, so the
# simulated interval between the snapshot and the devnet's "now" grows by one
# day per day. The pin used to be refreshed every 1-4 weeks; in 2026 it went
# 48 days without a refresh and nothing in CI said so. Silence is the defect
# this script fixes: the age is now printed on every run that validates the
# manifest, annotated as a GitHub `::warning::` past a soft threshold, and can
# be hard-gated with `--max-age-days` where failing is affordable (nightly).
#
# It deliberately does NOT hard-fail by default. A stale pin is a maintenance
# signal, not a reason to red every pull request in the queue — that is exactly
# the kind of unactionable blocking failure the CI-truthfulness work exists to
# remove. Pass `--max-age-days` from a scheduled job to get the hard signal.
#
# HOW TO REFRESH THE PIN when this reports a stale fixture:
#   RMPC_FORK_RPC_URL=<Base archive RPC> scripts/devnet/snapshot-fork.sh
# then update testing/ethereum-testnet/config/fork-block.json's `block_number`
# and `block_hash` to match the new CURRENT.json, and regenerate
# testing/fixtures/fork-state/genesis-alloc.json with
# `smoke-test-genesis-ingester` and
# testing/ethereum-testnet/config/expected-prices.json.
#
# NOTE ON THE RPC: snapshot-fork.sh defaults to
# https://base-rpc.publicnode.com, which serves state for only ~128 blocks
# (~4 minutes on Base) and rejects anything older with "Archive requests
# require a personal token". A capture session runs far longer than that, so
# the default endpoint cannot complete a refresh. Set RMPC_FORK_RPC_URL to a
# Base archive endpoint (issue #1239).
#
# Usage:
#   scripts/devnet/check-fork-pin-age.sh                     # report + warn
#   scripts/devnet/check-fork-pin-age.sh --warn-days 14
#   scripts/devnet/check-fork-pin-age.sh --max-age-days 30   # hard gate
#   scripts/devnet/check-fork-pin-age.sh --manifest <path>   # self-test hook
#
# Exit codes:
#   0 — age determined (whether or not the soft warning fired).
#   2 — manifest missing, unreadable, or has no usable `captured_at`.
#   3 — `--max-age-days` was supplied and the pin exceeds it.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# Historical refresh cadence for this fixture was 1-4 weeks (see `git log` on
# testing/ethereum-testnet/config/fork-block.json), so three weeks is the point
# past which the pin is outside its own established maintenance rhythm.
WARN_DAYS=21
MAX_AGE_DAYS=""
MANIFEST="$REPO_ROOT/testing/fixtures/fork-state/CURRENT.json"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --warn-days) WARN_DAYS="${2:?--warn-days needs a value}"; shift 2 ;;
    --max-age-days) MAX_AGE_DAYS="${2:?--max-age-days needs a value}"; shift 2 ;;
    --manifest) MANIFEST="${2:?--manifest needs a value}"; shift 2 ;;
    *) echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
  esac
done

for n in "$WARN_DAYS" ${MAX_AGE_DAYS:+"$MAX_AGE_DAYS"}; do
  case "$n" in
    ''|*[!0-9]*) echo "ERROR: day thresholds must be non-negative integers, got '$n'" >&2; exit 2 ;;
  esac
done

if [ ! -f "$MANIFEST" ]; then
  echo "ERROR: fork-state manifest not found: $MANIFEST" >&2
  exit 2
fi

CAPTURED_AT="$(jq -r '.captured_at // empty' "$MANIFEST")"
if [ -z "$CAPTURED_AT" ]; then
  echo "ERROR: $MANIFEST has no captured_at field; the pin's age cannot be determined" >&2
  exit 2
fi

if ! CAPTURED_EPOCH="$(date -u -d "$CAPTURED_AT" +%s 2>/dev/null)"; then
  echo "ERROR: $MANIFEST captured_at is not a parseable timestamp: $CAPTURED_AT" >&2
  exit 2
fi

FORK_BLOCK="$(jq -r '.fork_block // "unknown"' "$MANIFEST")"
NOW_EPOCH="$(date -u +%s)"
AGE_SECONDS=$((NOW_EPOCH - CAPTURED_EPOCH))
# A pin captured in the future is nonsense but must not underflow into a
# huge unsigned age; clamp and report it as zero rather than silently
# reporting a fresh-looking negative number.
if [ "$AGE_SECONDS" -lt 0 ]; then AGE_SECONDS=0; fi
AGE_DAYS=$((AGE_SECONDS / 86400))

echo "[check-fork-pin-age] fork_block=$FORK_BLOCK captured_at=$CAPTURED_AT age_days=$AGE_DAYS warn_days=$WARN_DAYS max_age_days=${MAX_AGE_DAYS:-none}"

REFRESH_HINT="Refresh with RMPC_FORK_RPC_URL=<Base archive RPC> scripts/devnet/snapshot-fork.sh, then realign testing/ethereum-testnet/config/fork-block.json, genesis-alloc.json and expected-prices.json. The default public endpoint (base-rpc.publicnode.com) prunes state after ~128 blocks and cannot complete a capture (issue #1239)."

if [ -n "$MAX_AGE_DAYS" ] && [ "$AGE_DAYS" -gt "$MAX_AGE_DAYS" ]; then
  echo "::error::The devnet's Base fork pin (block $FORK_BLOCK, captured $CAPTURED_AT) is $AGE_DAYS days old, over the $MAX_AGE_DAYS-day limit. The devnet clock is wall-clock now while the forked Aave/Compound/Morpho state is frozen at the pin, so the simulated accrual interval grows every day this is not refreshed (issue #1386). $REFRESH_HINT"
  exit 3
fi

if [ "$AGE_DAYS" -gt "$WARN_DAYS" ]; then
  echo "::warning::The devnet's Base fork pin (block $FORK_BLOCK, captured $CAPTURED_AT) is $AGE_DAYS days old, past the $WARN_DAYS-day refresh cadence. The devnet clock is wall-clock now while the forked protocol state is frozen at the pin, so the simulated accrual interval grows every day (issue #1386). $REFRESH_HINT"
  exit 0
fi

echo "[check-fork-pin-age] OK: pin is within the $WARN_DAYS-day refresh cadence"
