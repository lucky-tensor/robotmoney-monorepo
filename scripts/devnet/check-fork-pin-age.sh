#!/usr/bin/env bash
# Report — and optionally gate on — the age of the committed Base fork pin.
#
# Canonical: docs/technical/full-stack-devnet.md §"Fork-state fixture" →
#            "Pin age (issue #1386)", which documents this gate's thresholds,
#            why the age is measured from CURRENT.json's captured_at, and the
#            refresh recipe this script's failure message points at.
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

# Validate each threshold separately rather than looping over an expansion:
# `${MAX_AGE_DAYS:+"$MAX_AGE_DAYS"}` leaves the outer expansion unquoted, so a
# value containing glob metacharacters would be pathname-expanded before the
# digit test and the unexpanded value would still be what reaches the `-gt`
# comparison below.
require_days() {
  case "$2" in
    ''|*[!0-9]*)
      echo "ERROR: day thresholds must be non-negative integers, got '$2' for $1" >&2
      exit 2
      ;;
  esac
}
require_days --warn-days "$WARN_DAYS"
if [ -n "$MAX_AGE_DAYS" ]; then require_days --max-age-days "$MAX_AGE_DAYS"; fi

if [ ! -f "$MANIFEST" ]; then
  echo "ERROR: fork-state manifest not found: $MANIFEST" >&2
  exit 2
fi

# Everything read out of the manifest is echoed into GitHub Actions workflow
# commands (`::warning::` / `::error::`) below. `jq -r` emits embedded newlines
# literally, so an unsanitised field would let a value in a committed JSON file
# start its own output line, which the runner parses as a *new* workflow
# command — enough to forge annotations, mask arbitrary log substrings with
# `::add-mask::`, or collapse the rest of the step behind `::group::`. Strip CR
# and LF and bound the length of every field before it reaches an echo. The
# `state_sha256` digest binding does not help here: it covers the .anvil-state
# blob, not this manifest's own fields.
# Strip CR/LF (which is what would start a new command line), defang any `::`
# prefix so the token cannot read as a workflow command even when quoted back
# inside a diagnostic, and bound the length. A legitimate ISO-8601 timestamp
# contains single colons but never a doubled one, so this leaves real values
# untouched.
scrub() { printf '%s' "$1" | tr -d '\n\r' | sed 's/::/: :/g' | cut -c1-64; }

CAPTURED_AT="$(scrub "$(jq -r '.captured_at // empty' "$MANIFEST")")"
if [ -z "$CAPTURED_AT" ]; then
  echo "ERROR: $MANIFEST has no captured_at field; the pin's age cannot be determined" >&2
  exit 2
fi

# `captured_at` is written by snapshot-fork.sh as `date -u +%Y-%m-%dT%H:%M:%SZ`.
# GNU date parses it with -d; BSD/macOS date needs -j -f. Try both rather than
# making this script Linux-only, since a maintainer refreshing the fixture is
# exactly who runs check-fork-manifest.sh by hand.
CAPTURED_EPOCH="$(date -u -d "$CAPTURED_AT" +%s 2>/dev/null \
  || date -u -j -f '%Y-%m-%dT%H:%M:%SZ' "$CAPTURED_AT" +%s 2>/dev/null \
  || true)"
if [ -z "$CAPTURED_EPOCH" ]; then
  echo "ERROR: $MANIFEST captured_at is not a parseable timestamp: $CAPTURED_AT" >&2
  exit 2
fi

# `fork_block` is presentational here (the age comes from captured_at), so a
# malformed value must not fail the gate — but it must not be able to inject a
# workflow command either. Accept digits only; anything else reports as
# "unparseable" rather than being echoed.
FORK_BLOCK="$(scrub "$(jq -r '(.fork_block // "unknown") | tostring' "$MANIFEST")")"
case "$FORK_BLOCK" in
  ''|*[!0-9]*) FORK_BLOCK="unparseable" ;;
esac
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
