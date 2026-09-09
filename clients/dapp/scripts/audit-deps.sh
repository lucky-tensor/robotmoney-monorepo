#!/usr/bin/env bash
# Dependency vulnerability gate for the dApp.
# Canonical: docs/technical/security-model.md §8
#
# Runs `bun audit --audit-level=high`, which exits non-zero on any high or
# critical (CVSS >= 7.0) advisory in the resolved dependency tree. This matches
# the threshold used by the Rust `cargo audit` gate in suite-04.
#
# We use `bun audit` (not `npm audit`) because the dApp's lockfile is bun.lock;
# `npm audit` requires a package-lock.json and fails with ENOLOCK here.
#
# ACCEPTED ADVISORIES
# Each id below is a pre-existing high/critical advisory in a transitive
# dependency that cannot yet be upgraded without a breaking change. They are
# suppressed so the gate has a green baseline and blocks merges on any NEW
# high/critical advisory. Every entry MUST carry a dated justification.
# Re-evaluate on or before each expiry; remove once the upstream fix lands.
set -euo pipefail

# Format: "<GHSA-id> # <package>: <reason>. expires: <YYYY-MM-DD>"
ACCEPTED_ADVISORIES=(
  # ws (transitive via viem > isows, wagmi's WalletConnect connector, jsdom, and
  # @vitest/browser). The memory-exhaustion DoS requires a malicious WebSocket peer
  # sending crafted tiny fragments; the dApp only opens WS connections to known RPC /
  # WalletConnect endpoints, and the test-only consumers (jsdom, vitest) never listen.
  "GHSA-96hv-2xvq-fx4p" # ws: memory exhaustion DoS from tiny fragments/chunks. expires: 2026-12-01
  # vite (build-time / dev-server only, transitive via @tailwindcss/vite,
  # @vitejs/plugin-react, and vitest). The server.fs.deny bypass affects only the dev
  # server on Windows alternate-path forms; production serves a static prebuilt bundle
  # and CI runs on Linux. Fixed in vite >6.4.2, which is a major-version upgrade.
  "GHSA-fx2h-pf6j-xcff" # vite: server.fs.deny bypass on Windows alternate paths. expires: 2026-12-01
  # form-data (transitive via the wallet-connector SDK chain). The CRLF injection
  # requires the app to build a multipart form with attacker-controlled field/file
  # names; the dApp never constructs multipart requests itself — form-data is only
  # reachable through the pinned wallet SDK.
  "GHSA-hmw2-7cc7-3qxx" # form-data: CRLF injection via unescaped multipart field names. expires: 2026-12-01
  # hono (transitive via wagmi > @wagmi/connectors > porto). The CORS-middleware
  # advisory only triggers when an app mounts hono's CORS middleware with the
  # origin left at its wildcard default while sending credentials; the dApp never
  # runs hono itself — hono is only reachable through the pinned porto wallet SDK.
  "GHSA-88fw-hqm2-52qc" # hono: CORS middleware reflects any Origin with credentials on wildcard default. expires: 2026-12-01
  # js-yaml (build-time / dev-dependency only, transitive via eslint > @eslint/eslintrc).
  # The CPU exhaustion vulnerability in merge keys affects only YAML parsing during
  # linting of developer-controlled config files; it does not affect the production runtime bundle.
  "GHSA-2883-xcg3-v3hh" # js-yaml: maxTotalMergeKeys does not limit CPU use for empty merge sources. expires: 2026-12-01
)

ignore_args=()
for entry in "${ACCEPTED_ADVISORIES[@]}"; do
  # Each array entry is "<id> # <comment>"; keep only the leading id.
  id="${entry%%#*}"
  id="${id// /}"
  [ -n "$id" ] || continue
  ignore_args+=(--ignore "$id")
done

echo "Running bun audit --audit-level=high (${#ACCEPTED_ADVISORIES[@]} accepted advisories suppressed)"
exec bun audit --audit-level=high "${ignore_args[@]}"
