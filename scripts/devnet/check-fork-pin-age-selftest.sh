#!/usr/bin/env bash
# Offline behaviour self-test for scripts/devnet/check-fork-pin-age.sh.
#
# Canonical: docs/technical/full-stack-devnet.md §"Fork-state fixture" →
#            "Pin age (issue #1386)".
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

# Write a manifest whose captured_at is $1 days from now (negative for the
# past, positive for the future). Formatted via python3 rather than `date`
# arithmetic so the self-test runs the same on GNU and BSD userlands — the
# helper under test accepts both, and the test must not be the narrower one.
write_manifest() {
  local days="$1" path="$2"
  local ts
  ts="$(python3 -c "import datetime,sys; print((datetime.datetime.now(datetime.timezone.utc)+datetime.timedelta(days=float(sys.argv[1]))).strftime('%Y-%m-%dT%H:%M:%SZ'))" "$days")"
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

FRESH="$WORKDIR/fresh.json"; write_manifest -2 "$FRESH"
STALE="$WORKDIR/stale.json"; write_manifest -48 "$STALE"
FUTURE="$WORKDIR/future.json"; write_manifest 5 "$FUTURE"

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

# Manifest fields are echoed into GitHub Actions workflow commands, and `jq -r`
# emits embedded newlines literally — so a field carrying a newline plus a
# `::`-prefixed string could forge a second workflow command (`::add-mask::`,
# `::group::`, a fake `::error::`) from a committed file. Assert the helper
# emits exactly ONE workflow command per run regardless of what the manifest
# contains, and that the injected text never reaches the output.
echo "[selftest] manifest fields cannot forge workflow commands"
INJECT=$'48896605\n::add-mask::hunter2\n::error::FORGED'
jq -n --arg fb "$INJECT" --arg ts "$(python3 -c "import datetime;print((datetime.datetime.now(datetime.timezone.utc)-datetime.timedelta(days=48)).strftime('%Y-%m-%dT%H:%M:%SZ'))")" \
  '{fork_block:$fb, captured_at:$ts}' > "$WORKDIR/inject-block.json"
jq -n --arg ts "$INJECT" '{fork_block:1, captured_at:$ts}' > "$WORKDIR/inject-ts.json"

# fork_block is presentational, so a hostile value must neither inject nor fail
# the gate: the run still warns (age 48 > 21) and still exits 0.
run_case "hostile fork_block still warns"        0 "::warning::" --manifest "$WORKDIR/inject-block.json"
run_case "hostile fork_block reported unparseable" 0 "fork_block=unparseable" --manifest "$WORKDIR/inject-block.json"
run_case_absent "hostile fork_block does not emit add-mask" 0 "::add-mask::" --manifest "$WORKDIR/inject-block.json"
run_case_absent "hostile fork_block does not emit forged error" 0 "FORGED" --manifest "$WORKDIR/inject-block.json"
# captured_at is load-bearing, so a hostile value must fail loudly instead.
run_case "hostile captured_at fails loudly" 2 "not a parseable timestamp" --manifest "$WORKDIR/inject-ts.json"
run_case_absent "hostile captured_at does not emit add-mask" 2 "::add-mask::" --manifest "$WORKDIR/inject-ts.json"

# Exactly one workflow-command line, whatever the manifest holds — on BOTH the
# soft (::warning::) and hard (::error::) paths, since each emits its own
# annotation and each interpolates the manifest's fields.
#
# The match is anchored with optional leading whitespace, not at column 0: the
# Actions runner trims leading whitespace before matching a command prefix, so
# `grep -c '^::'` would pass a payload the runner would still honour.
for m in "$WORKDIR/inject-block.json" "$STALE"; do
  for gate in "" "--max-age-days 30"; do
    # shellcheck disable=SC2086  # $gate is a literal flag pair, intentionally split.
    n=$("$HELPER" --manifest "$m" $gate 2>&1 | grep -cE '^[[:space:]]*::' || true)
    label="$(basename "$m")${gate:+ (hard gate)}"
    if [ "$n" -eq 1 ]; then
      echo "  ok: exactly one workflow command emitted ($label)"
    else
      echo "FAIL [workflow-command count]: $label emitted $n workflow-command lines, expected 1" >&2
      FAILURES=$((FAILURES + 1))
    fi
  done
done

echo "[selftest] thresholds that would disable the gate are rejected"
run_case "glob threshold rejected" 2 "non-negative integers" --manifest "$FRESH" --max-age-days '*'
# An uncomparable integer would make `[ -gt ]` error inside an `if`, which set -e
# does not catch, so the gate would silently pass. Must be rejected up front.
run_case "uncomparable threshold rejected" 2 "at most 5 digits" --manifest "$STALE" --max-age-days 99999999999999999999999
run_case_absent "uncomparable threshold does not exit 0" 2 "OK: pin is within" --manifest "$STALE" --max-age-days 99999999999999999999999

if [ "$FAILURES" -ne 0 ]; then
  echo "[selftest] FAILED: $FAILURES case(s)" >&2
  exit 1
fi
echo "[selftest] OK: check-fork-pin-age.sh honours every threshold and fails loudly on bad input"
