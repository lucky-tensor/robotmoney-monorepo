#!/usr/bin/env bash
# Offline behaviour self-test for scripts/devnet/check-fork-pin-age.sh.
#
# Canonical: docs/development/smoke-test-design.md (Devnet section).
# Issue: #1386.
#
# The age gate's whole value is that it cannot stay quiet about a stale pin.
# A gate nobody exercises is exactly the class of check that let the real pin
# reach 48 days unnoticed, so this drives the helper against synthetic
# manifests — no network, no Docker, the real fixture untouched — and asserts
# each branch actually behaves:
#
#   fresh pin              -> exit 0, no warning annotation
#   past the warn cadence  -> exit 0, `::warning::` emitted
#   past --max-age-days    -> exit 3, `::error::` emitted
#   under --max-age-days   -> exit 0
#   missing captured_at    -> exit 2 (loud, never a silent pass)
#   unparseable captured_at-> exit 2
#   future captured_at     -> exit 0, age clamped to 0 (no underflow)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HELPER="$REPO_ROOT/scripts/devnet/check-fork-pin-age.sh"

WORKDIR="$(mktemp -d -t fork-pin-age-selftest.XXXXXX)"
trap 'rm -rf "$WORKDIR"' EXIT

FAILURES=0

# Write a manifest whose captured_at is $1 days from now, expressed as a
# `date` offset ("-2 days" for the past, "+5 days" for the future). `date -d`
# collapses a doubled sign, so the sign is passed explicitly rather than
# negated here.
write_manifest() {
  local offset="$1" path="$2"
  local ts
  ts="$(date -u -d "$offset" +%Y-%m-%dT%H:%M:%SZ)"
  jq -n --arg ts "$ts" '{fixture:"base-1.json",state_file:"base-1.anvil-state",fork_block:1,chain_id:8453,captured_at:$ts}' > "$path"
}

# run_case <name> <expected-exit> <expected-substring-or-EMPTY> <args...>
run_case() {
  local name="$1" want_exit="$2" want_text="$3"; shift 3
  local out rc=0
  out="$("$HELPER" "$@" 2>&1)" || rc=$?
  if [ "$rc" -ne "$want_exit" ]; then
    echo "FAIL [$name]: exit $rc, expected $want_exit" >&2
    echo "$out" | sed 's/^/    /' >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if [ -n "$want_text" ] && ! printf '%s' "$out" | grep -qF -- "$want_text"; then
    echo "FAIL [$name]: output did not contain '$want_text'" >&2
    echo "$out" | sed 's/^/    /' >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  echo "  ok: $name"
}

# run_case_absent <name> <expected-exit> <forbidden-substring> <args...>
run_case_absent() {
  local name="$1" want_exit="$2" bad_text="$3"; shift 3
  local out rc=0
  out="$("$HELPER" "$@" 2>&1)" || rc=$?
  if [ "$rc" -ne "$want_exit" ]; then
    echo "FAIL [$name]: exit $rc, expected $want_exit" >&2
    echo "$out" | sed 's/^/    /' >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if printf '%s' "$out" | grep -qF -- "$bad_text"; then
    echo "FAIL [$name]: output unexpectedly contained '$bad_text'" >&2
    echo "$out" | sed 's/^/    /' >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  echo "  ok: $name"
}

FRESH="$WORKDIR/fresh.json"; write_manifest "-2 days" "$FRESH"
STALE="$WORKDIR/stale.json"; write_manifest "-48 days" "$STALE"
FUTURE="$WORKDIR/future.json"; write_manifest "+5 days" "$FUTURE"

echo "[selftest] soft path"
run_case_absent "fresh pin emits no warning" 0 "::warning::" --manifest "$FRESH"
run_case "fresh pin reports its age" 0 "age_days=2" --manifest "$FRESH"
run_case "stale pin warns" 0 "::warning::" --manifest "$STALE"
run_case "stale pin reports its age" 0 "age_days=48" --manifest "$STALE"
run_case_absent "warn threshold is honoured" 0 "::warning::" --manifest "$STALE" --warn-days 60

echo "[selftest] hard gate"
run_case "over --max-age-days fails" 3 "::error::" --manifest "$STALE" --max-age-days 30
run_case_absent "under --max-age-days passes" 0 "::error::" --manifest "$STALE" --max-age-days 60

echo "[selftest] malformed input is loud"
echo '{"fork_block":1}' > "$WORKDIR/no-ts.json"
run_case "missing captured_at fails loudly" 2 "no captured_at" --manifest "$WORKDIR/no-ts.json"
echo '{"captured_at":"not-a-date"}' > "$WORKDIR/bad-ts.json"
run_case "unparseable captured_at fails loudly" 2 "not a parseable timestamp" --manifest "$WORKDIR/bad-ts.json"
run_case "missing manifest fails loudly" 2 "not found" --manifest "$WORKDIR/absent.json"
run_case "non-numeric threshold rejected" 2 "non-negative integers" --manifest "$FRESH" --warn-days abc

echo "[selftest] future capture does not underflow"
run_case "future captured_at clamps to zero" 0 "age_days=0" --manifest "$FUTURE"

if [ "$FAILURES" -ne 0 ]; then
  echo "[selftest] FAILED: $FAILURES case(s)" >&2
  exit 1
fi
echo "[selftest] OK: check-fork-pin-age.sh honours every threshold and fails loudly on bad input"
