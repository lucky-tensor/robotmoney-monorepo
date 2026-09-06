# CI Suite Inventory

Each section is one GitHub Actions workflow file. Steps are listed in
execution order as they appear (or should appear) in the job.

The CI job has no special devnet setup step — the test suite itself starts
and tears down the Docker Compose stack whenever it needs a clean slate.

## Foundry version

Every `foundry-rs/foundry-toolchain@v1` step in `.github/workflows/` is pinned
to **`v1.7.1`**. Wherever a step below reads "Install Foundry toolchain", that
is the version it installs — there are no floating `stable` or `nightly`
channels left.

Foundry 1.8.0 replaced `forge doc`'s mdbook generator with a
[vocs](https://vocs.dev) site generator and changed how `forge fmt` indents a
struct literal inside a broken method chain. Both changes are silent (`forge
doc` still exits 0), so the release turned `generated-docs-freshness` and
`solidity-lint` red on `dev` with no repo change (issue #1263). The pin is
containment, not the destination: adopting the vocs layout and lifting it is
issue #1264, and `grep -rn 'Unpin via issue #1264' .github/workflows/` is the
complete worklist.

Match the pin locally with `foundryup --install v1.7.1`; a newer `forge` will
regenerate `contracts/doc/` into a layout the freshness comparator refuses to
compare (it names the mismatch rather than reporting stale docs).

## Environment key

| Symbol | Meaning |
|--------|---------|
| `devnet` | Geth + Lighthouse Docker Compose stack (`testing/ethereum-testnet/config/`). Lifecycle owned by the test code. |
| `anvil` | In-process Anvil EVM. No Docker. |
| `fork` | Anvil forked from the checked-in golden fixture (`testing/fixtures/fork-state/`) at a pinned block — deterministic, offline, no secret, no live RPC (ADR-0011). CI fails loudly if the fixture is missing or zero fork tests run; it never silent-skips. (Live Base-mainnet forking survives only as the non-blocking **nightly drift alarm** via a free public RPC — see suite 5.) |
| `none` | No chain. Static analysis, pure unit tests, doc checks. |

---

## Suites

### 1–2. Smart contract unit tests, invariant tests, and coverage gate
**Suggested file:** `.github/workflows/forge-tests.yml`
**Environment:** `anvil`
**Trigger paths:** `contracts/**`, `foundry.toml`

**Jobs:**
- `unit` — forge unit tests; runs immediately on trigger
- `invariant` — forge fuzz/invariant tests; runs in parallel with `unit`
- `coverage` — coverage gate check; **needs `unit` and `invariant`** (only worth running if tests pass)

**Steps — `unit` job:**
1. Checkout repository
2. Install Foundry toolchain
3. Cache Foundry build artifacts (`cache/`, `out/`)
4. `forge fmt --check`
5. `forge build`
6. `forge test` — unit tests: every public function, access control boundary, revert path, event emission, ERC-4626 rounding invariant
7. `forge test` (four-vault real-TVL pyramid, issue #592) — a dedicated, named
   step guards the real four-vault end state so it cannot silently regress:
   `DeployDemoExtraVaults.t.sol` asserts **all four** PRD §11 vaults are
   registered Active, three are router-eligible while the deSPXA RWA vault is
   direct-seed-only (ADR-0006 §1), and every vault reports non-zero
   `totalAssets` after a routed + direct deposit; `RwaVault.t.sol` covers the
   deSPXA deposit/redeem round-trip and stale-oracle halt; the
   `BasketVault`/`AgentTokenVault` suites pin per-vault basket composition.

**Steps — `invariant` job:**
1. Checkout repository
2. Install Foundry toolchain
3. Cache Foundry build artifacts (`cache/`, `out/`)
4. `forge build`
5. `forge test` with fuzzer enabled — invariant tests: share accounting, per-agent cap sequences, deposit monotonicity, reentrancy under malicious stub, pause invariant

**Steps — `coverage` job:**
1. Checkout repository
2. Install Foundry toolchain + Python
3. Cache Foundry build artifacts (`cache/`, `out/`)
4. `forge coverage` with `check_gateway_coverage.py` — enforces branch-coverage gate on `RobotMoneyGateway`

> **The coverage compile is memory-bound, and the budget is now gated (issue #1298).**
> `forge coverage` runs without `--ir-minimum`. That flag enables `viaIR`, whose peak
> RSS grows steeply with the compile set. Peak RSS of the whole coverage run with
> solc 0.8.24, measured on an unconstrained host:
>
> | compile set | `--ir-minimum` | peak RSS | wall |
> |---|---|---|---|
> | 174 files (`dev` @ `969cfc84`) | yes | 17.48 GiB | 7m12s |
> | 178 files (#1293 @ `3faeadd9`) | yes | 18.00 GiB | 9m30s |
> | 174 files | no | 2.27 GiB | 0m24s |
> | 178 files | no | 2.30 GiB | 1m10s |
>
> A GitHub-hosted `ubuntu-latest` runner has 16 GB, and **both `viaIR` figures are
> above it**. `dev` at 174 files survived only because the allocator reuses arenas
> under memory pressure instead of growing; PR #1293's four extra files were enough
> that it no longer could, and the runner was OOM-killed mid-compile at 7m26s of a
> 20-minute timeout. GitHub reports that as `The runner has received a shutdown
> signal` / `exit code 143`, which reads as infrastructure flake and invites a re-run
> that cannot succeed. The kill is **not** a timeout: the two deaths landed at 7m26s
> and 8m42s, different elapsed times, both far short of the 20-minute deadline.
>
> Nothing in `contracts/` needs `viaIR` to compile under coverage's unoptimised
> profile, and dropping it *raised* measured branch coverage (49.0% → 58.5%) because
> `--ir-minimum`'s degraded source mappings were losing branch hits. Two exclusions
> that existed only as `--ir-minimum` workarounds — `^test_getPastVotes_` (AZ-GOV-1)
> and `CustodyInvariantGuardTest` (issue #944) — were removed at the same time, so the
> coverage set grew rather than shrank.
>
> Dropping `viaIR` also collapses the job's sensitivity to compile-set size, from
> ~133 MB per added Solidity file to ~7.7 MB.
>
> The step runs under `.github/scripts/run_coverage_memory_guard.py`, which measures
> the run's peak RSS and **fails the job above 50% of runner RAM**. That is deliberate:
> the next contract-adding PR should meet an explanatory red gate with the number in
> it, not rediscover the cliff as an unfixable "flake". Raising the ceiling is a
> deliberate act (`COVERAGE_MEM_CEILING_PCT`), not a default.

---

### 3. Solidity quality gate
**Suggested file:** `.github/workflows/solidity-quality.yml`
**Environment:** `none`
**Trigger paths:** `contracts/**`, `foundry.toml`

**Jobs:**
- `lint` — fmt, build, NatSpec check; single job, runs immediately
- `slither` — static analysis; **needs `lint`** (avoids running expensive analysis on code that doesn't build or format-check)

**Steps — `lint` job:**
1. Checkout repository
2. Install Foundry toolchain
3. Cache Foundry build artifacts (`cache/`, `out/`)
4. `forge fmt --check` — formatting
5. `forge build --force` — clean build; zero warnings enforced via `--deny warnings` in `foundry.toml`
6. `forge doc --check` — NatSpec coverage threshold: every `external` and `public` function on `RobotMoneyGateway` must carry `@notice`, `@param`, and `@return` tags; script fails if any are missing

**Steps — `slither` job:**
1. Checkout repository
2. Install Foundry toolchain + Python + Slither
3. Cache Foundry build artifacts (`cache/`, `out/`)
4. `forge build` — produce artifacts for Slither
5. `slither .` — standard detector set (reentrancy, uninitialized storage, dangerous delegatecall, tx.origin, unchecked low-level calls)
6. Dependency audit — check imported OpenZeppelin and Aave interface versions against known-vulnerable releases

---

### 4. Rust quality gate
**Suggested file:** `.github/workflows/rust-quality.yml`
**Environment:** `none`
**Trigger paths:** `clients/rust-payment-client/**`, `testing/ethereum-testnet/e2e-rust/**`, `services/explorer-indexer/**`

**Jobs:**
- `lint` — fmt and clippy across all crates, plus the workspace logging-facade guard; runs immediately
- `audit` — dependency vulnerability scan; runs in parallel with `lint` (independent of build cache)
- `doc-coverage` — build and rustdoc threshold check; **needs `lint`** (avoids running a full build on code that fails style checks)
- `test-target-coverage` — every cargo integration-test target is executed by a workflow or allowlisted with a reason (issue #1282); pure Python, no toolchain, so it answers even when the Rust build is broken. See [Integration-test target coverage](#integration-test-target-coverage).

**Steps — `lint` job:**
1. Checkout repository
2. Install Rust toolchain + clippy
3. Cargo cache
4. `cargo fmt --check` — formatting across all crates
5. `cargo clippy --all-targets --all-features -- -D warnings` — zero warnings enforced. `--all-targets` is what type-checks every crate's `tests/` integration binaries; the root manifest is a virtual manifest with no `default-members`, so this one command covers every workspace member. **Do not drop `--all-targets`** — see [Rust `tests/` compile coverage](#rust-tests-compile-coverage) (issue #1295), which fails red if it is removed.
6. `cargo_test_require_executed.sh -p rmpc-logging --test workspace_uses_shared_facade` — every binary and service entrypoint initialises logging through `rmpc_logging::init_service` and not `tracing_subscriber::fmt()` (issue #247). Source-text walk, no chain, no Docker. It was executed by no workflow until issue #1282, so the regression it exists to make load-bearing was not.

**Steps — `test-target-coverage` job:**
1. Checkout repository
2. `pip install pyyaml`
3. `python3 .github/scripts/check_cargo_test_target_coverage.py --list-executed` — red on any `tests/*.rs` target no workflow runs
4. `bash .github/scripts/tests/test_check_cargo_test_target_coverage.sh` — drives the checker's own failure paths against synthetic workspaces and against this repo plus one synthetic dark file

**Steps — `audit` job:**
1. Checkout repository
2. Install Rust toolchain
3. `cargo install cargo-audit --locked` — install the advisory scanner
4. `cargo audit` — runs over **every** `Cargo.lock` in the repo against the rustsec/advisory-db. There is exactly one: the root workspace lockfile. `services/explorer-indexer`, `testing/doctests`, `testing/ethereum-testnet/e2e-rust`, and `testing/fork-e2e-rust` are workspace **members**, so cargo resolves them against the root lock; the standalone lockfiles they used to carry were removed (issue #1202) because cargo never read or regenerated them and they drifted into pinning versions no build used. The loop is kept as-is so a genuinely standalone workspace added later is audited automatically. Exits non-zero on any vulnerability advisory. Ignore configuration is read from `.cargo/audit.toml`, which suppresses pre-existing sub-high advisories with dated justifications so the gate has a green baseline and blocks merges on any new advisory. To accept a known low-risk advisory temporarily, add its RUSTSEC id to the `ignore` list in that file with a reason and expiry comment.

**Steps — `doc-coverage` job:**
1. Checkout repository
2. Install Rust toolchain + rustdoc
3. Cargo cache
4. `cargo build --all-targets` — clean build; surfaces compile errors not caught by clippy
5. `cargo doc --no-deps --all-features 2>&1 | tee "$RUNNER_TEMP/.../rustdoc.log"` + `check_rustdoc_coverage.py` — enforces doc coverage threshold: every `pub` function, struct, and enum in `rmpc` and `explorer-indexer` crates must carry a doc comment; script exits non-zero if coverage falls below threshold

---

### 5. Fork integration tests (protocol adapters)
**Suggested file:** `.github/workflows/fork-integration.yml`
**Environment:** `fork`
**Tier / triggers:** HEAVY — 4 Geth/Anvil devnet slots, 20-25 min wall-clock. Gates every `pull_request` into `dev` (no path filter) and runs on `push` to `dev`. Feature PRs into phase branches skip this suite; `suite-06` (rmpc-unit) provides fast feedback on those.

**Fixture, not live RPC — golden on the merge gate, live on the nightly (ADR-0011):**
Merge-gating runs (feature-PR and `dev`-merge) fork the **checked-in golden
fixture** (`testing/fixtures/fork-state/`, loaded via `anvil --load-state` for
the Rust layer and via a pinned-block fork of `CURRENT.anvil-state` for the
Solidity forge tests) — deterministic, offline, **no CI secret, no live RPC**.
Coverage is **loud** per the repo test-coverage policy: a missing fixture or
zero executed fork tests fails CI, never silent-skips. (This corrects the
legacy Solidity fork tests, which called `vm.createSelectFork` against a
never-provisioned `RMPC_FORK_RPC_URL`/`FORK_RPC_URL` secret and therefore
silently skip-cleaned to a false green.) A **non-blocking nightly drift alarm**
forks live **Base mainnet at latest** via a **free public RPC** (public default,
no secret — e.g. `https://base-rpc.publicnode.com`) and re-runs the suite; on
failure it opens/updates a tracking issue instead of blocking a merge. That
nightly is what catches real upstream drift (pool migrations, ABI changes,
oracle heartbeat changes) a pinned snapshot cannot see. It is dispatched by the
nightly orchestrator (suite 21).

**Why Anvil here, and why this is not redundant with the Geth+Lighthouse devnet harness:**
This suite forks **Base mainnet** state (real deployed contracts, real DEX
pools, real USDC — committed as the golden fixture, refreshed live only by the
nightly) and runs the Rust client (`rmpc`) against it. The goal is to catch ABI encoding drift, address-constant mistakes, and real-world RPC error shapes — bugs that only show up against actually-deployed mainnet contracts. The smoke-test devnet (Geth+Lighthouse, see suite 14) cannot do this: it deploys fresh contracts on an empty chain, so it cannot tell you "the calldata `rmpc` generates still matches what is deployed at the real gateway address on Base."

Anvil is used specifically because `anvil --fork-url` is the only ergonomic way to mount mainnet state at a pinned block and let tests mutate it locally (cheat codes like `anvil_setBalance` to fund test accounts on forked USDC). One anvil child per test gives cheap fork-restart-per-test isolation (per the ADR), with no snapshot/revert orchestration. Geth+Lighthouse cannot fork mainnet state this way; that stack is purpose-built for the empty-devnet "boot a real chain locally" scenario.

| Concern | Suite 5 (Anvil fork) | Devnet harness (suite 14, smoke-test `--full-stack`) |
|---|---|---|
| Chain | Anvil forking Base mainnet | Real Geth+Lighthouse, empty genesis |
| Contracts | Already-deployed mainnet ones | Freshly deployed by Fixture |
| Catches | ABI/address/RPC-shape drift vs prod | Full-stack flow (dapp→indexer→explorer), real block times |
| Speed | Seconds per test (instant mining) | ~12s blocks, minutes |

The two suites are complements, not duplicates. The retired Anvil "OpenClaw demo" suite (#242/#244) used Anvil to demo the whole product — that role was correctly taken over by the Geth+Lighthouse smoke-test. Suite 5's Anvil usage targets a job Geth+Lighthouse cannot do.

A per-test audit of suite-05's coverage against the alternative suites is recorded in [suite-05-audit.md](./suite-05-audit.md) (issue #248). The audit's recommendation is **keep**, with a follow-up slim of two tests that duplicate suite-6 coverage.

**Jobs:**
- `pr-smoke` — fast subset against the **golden fixture** (offline, no secret); runs on every PR trigger
- `full-suite` — all scenarios against the **golden fixture**; runs on push to `dev` and `workflow_dispatch`; no dependency on `pr-smoke` (different trigger context, not sequential)
- `live-drift-alarm` — **nightly, non-blocking** (ADR-0011): forks live Base mainnet at latest via a **free public RPC** (public default, no secret) and re-runs the suite as a drift alarm; on failure opens/updates a tracking issue rather than blocking merges. Schedule-only (dispatched by suite 21); never a PR gate.

**Steps (`pr-smoke` / `full-suite`, golden-fixture path):**
1. Checkout repository
2. Install Rust toolchain + clippy
3. Install Foundry toolchain
4. Cargo cache
5. `cargo fmt --check` + `cargo clippy`
6. `cargo test --no-run` — build test binaries
7. _(pr-smoke only)_ `abi_address_sanity` + `vault_deposit_redeem_smoke` — fast subset
8. _(full-suite only)_ All scenarios: `abi_address_sanity`, `vault_deposit_redeem_smoke`, `dex_route_smoke`, `failure_surface_smoke`, `gas_estimate_reality_check`, plus the remaining `rmpc_get_*` fork tests: `rmpc_get_vault_fork_base_mainnet`, `rmpc_get_balance_fork`, `rmpc_get_allowance_fork`, `rmpc_get_tx_fork`

---

### 6. Rust client unit tests
**Suggested file:** `.github/workflows/rmpc-unit.yml`
**Environment:** `none`
**Trigger paths:** none — runs on every PR and push, with no `paths:` filter.
This is not an oversight: step 6 (`plugin_skill_command_examples`, issue
#1203) must run on docs-only PRs that touch only `plugins/**/*.md`, which a
`clients/rust-payment-client/**` filter would exclude — silently reinstating
the false-green shape #1199/#1203 were filed about (issue #1231).

**Jobs:**
- `unit` — single job, no dependencies

**Steps:**
1. Checkout repository
2. Install Rust toolchain
3. Cargo cache
4. `cargo clippy -p rust-payment-client --all-targets -- -D warnings` — type-checks the crate's 23 `tests/` integration binaries in this job, so a struct-literal break in `tests/` fails here rather than only in suite 4 (issue #1295)
5. `cargo test --lib` — calldata builder output, preflight rejection cases, nonce management logic, fee policy guard, JSON output schema conformance, config parsing
6. `cargo_test_require_executed.sh --test plugin_skill_command_examples` — the one integration binary run here (issue #1203); fails red on a zero-tests-collected run

---

### 7. Rust client integration tests
**Suggested file:** `.github/workflows/rmpc-integration.yml`
**Environment:** `devnet` (Geth + Lighthouse)
**Tier / triggers:** HEAVY — gates every `pull_request` into `dev` (no path filter) and runs on `push` to `dev`.

**Jobs:**
- `geth-tests` — devnet-backed scenarios; runs immediately; should not run if suite 6 (`rmpc-unit`) is failing on the same commit (enforce via `workflow_run` dependency or branch protection)
- `nonce-race-stress` — in-process stress test, no chain; runs in parallel with `geth-tests`

**Steps — `geth-tests` job:**
1. Checkout repository
2. Verify Docker is available
3. Install Rust toolchain + clippy
4. Install Foundry toolchain
5. Cargo cache
6. `cargo fmt --check` + `cargo clippy` on both `rmpc` and `e2e-rust` crates
7. Pre-pull Docker images
8. `cargo build --release` — produce `rmpc` binary
9. `cargo test --test skill_docs_parity` — skill-package parity (no Docker)
10. `cargo test --test dapp_toml_roundtrip` — dApp TOML round-trip (no Docker)
11. `cargo test --release --test smoke --test-threads=1` — devnet boots inside test
12. `docker compose down -v` — explicit teardown between binaries
13. `cargo test --release --test scenarios --test-threads=1` — all policy/failure scenarios
14. `docker compose down -v`
15. `cargo test --release --test window_cap --test-threads=1`
16. `docker compose down -v` (always, on failure)

**Steps — `nonce-race-stress` job:**
1. Checkout repository
2. Install Rust toolchain
3. Cargo cache
4. `bash .github/scripts/stress_nonce_race.sh` — runs the race test 100× in-process; no chain

**Version guards — `rmpc-parity` job, ahead of the release build (issues #1191, #1243):**
- `.github/scripts/assert_manifest_ahead_of_tags.sh` — the rmpc manifest must be strictly ahead of every published `rmpc-v*.*.*` release. Scoped to changes under `clients/rust-payment-client/**`, because the state it reports belongs to the branch rather than to the change; the bare `v*.*.*` namespace belongs to `release-dapp` and cannot raise the rmpc floor.
- `.github/scripts/tests/test_assert_manifest_ahead_of_tags.sh` and `.github/scripts/tests/test_bump_rmpc_manifest.sh` — synthetic-repository exercises for the two scripts above, neither of whose real caller runs on a PR. See `docs/development/releasing.md`.

**CLI surface — `rmpc-parity` job (issue #1282):** one further step,
*rmpc CLI surface (deposit, router, status, self-check, get-\*)*, runs the sixteen
mockito-backed targets that no workflow named before: `cli`, `cli_deposit`,
`cli_status`, `cli_self_check`, `deposit_router` and the twelve `cli_get_*`
suites. They drive the built `rmpc` binary through `assert_cmd` against a
mockito JSON-RPC server — no chain, no Docker — which is why they belong in this
binary-only job rather than the devnet matrix. `cli_deposit.rs` alone is 687
lines covering the deposit happy path, chain-id mismatch, paused gateway, fee
cap, concurrent lock, receipt timeout, duplicate replay and revert. Wrapped in
`cargo_test_require_executed.sh`.

---

### 8. Explorer indexer tests
**Suggested file:** `.github/workflows/explorer-indexer.yml`
**Environment:** `devnet`
**Trigger paths:** `services/explorer-indexer/**`, `testing/explorer-indexer/**`

**Jobs:**
- `fast` — migration idempotency, block ingestion, RPC failure recovery, consensus-receipt indexing, and the reorg + read-path suites; uses a Postgres testcontainer; runs immediately
- `explorer-api` — the `clients/explorer-api` read API: IC committee / regime / consensus-receipt endpoints, plus the HTTP contract, CORS, router-shape and schema-parity suites; Postgres testcontainer
- `devnet` — reorg handling and finality-gated indexing against real Geth+Lighthouse; runs in parallel with `fast` (independent environments)

**Steps — `fast` job:**
1. Checkout repository
2. Install Rust toolchain + clippy
3. Cargo cache
4. `cargo fmt --check` + `cargo clippy`
5. `cargo test --no-run`
6. `cargo test --test migrations` — migration idempotency (Postgres testcontainer started by the test)
7. `cargo test --test idempotency` — block ingestion against known deposit events; double-count guard
8. `cargo test --test rpc_failure` — RPC failure recovery; reconnect and resume from last confirmed block
9. `cargo_test_require_executed.sh -p explorer-indexer --test consensus_receipt_indexing` — ReceiptRecorded / ReceiptReleased ingestion and the reorg rewrite of `consensus_receipts` (issue #1247)
10. `cargo_test_require_executed.sh -p explorer-indexer --test cursor_header_reorg --test reorg_cursor_vault_status --test account_history --test account_position_vote_power --test committee_indexing --test multi_vault --test vault_detail --test vault_registry` — the eight targets no workflow ran until issue #1282. The first two are the repository's **only** reorg-correctness suites; issue #1283 shipped because they were dark.

**Steps — `explorer-api` job:**
1. Checkout repository
2. Verify Docker is available (loud-skip if the testcontainer resource is missing)
3. Install Rust toolchain + cargo cache
4. `cargo_test_require_executed.sh -p explorer-api --test committee_api --test regime_api --test consensus_receipt_api` — IC endpoint handlers (issues #1105, #1247)
5. `cargo_test_require_executed.sh -p explorer-api --test endpoints --test router_introspection --test cors --test canonical_schema` — the four sibling suites left dark when the two above were wired up (issue #1282). `endpoints.rs` is the only executor of the HTTP contract the dApp reads; `router_introspection.rs` is the guard for §11 "the API does not sign, authorize, or write".

Steps 9 and 10 run through `.github/scripts/cargo_test_require_executed.sh`, which fails the step when zero tests were collected — a `tests/<name>.rs` file cargo was never told to run is a silent skip, not coverage. Step 10 additionally sets `EXPLORER_INDEXER_REQUIRE_PG=1`, which turns an unavailable Postgres testcontainer into a panic: the executed-count guard cannot distinguish a real pass from a test that returned early because its fixture handed it `None`, and that shape counted as passed.

**Steps — `devnet` job:**
1. Checkout repository
2. Verify Docker is available
3. Install Rust toolchain
4. Cargo cache
5. `cargo test --test fork_indexer` — reorg handling (orphaned-block row removal) and finality-gated indexing against devnet (requires real Geth + Lighthouse fork choice)

---

### 9. dApp quality gate
**Suggested file:** `.github/workflows/dapp-quality.yml`
**Environment:** `none`
**Trigger paths:** `clients/dapp/**`

**Jobs:**
- `lint-build` — single job, no dependencies

**Steps:**
1. Checkout repository
2. Setup Bun + Node 22
3. `bun install --frozen-lockfile`
4. `bun run fmt` — Prettier check
5. `bun run lint` — ESLint
6. `bunx tsc -b` — TypeScript type check
7. `bun run test` — Vitest: component rendering, browser-side key generation, credential boundary (no key material in DOM), form validation
8. `bun run build` — verify production build succeeds
9. `bash scripts/check-csp.sh` — Enforce strict CSP: builds the production bundle, serves it with `vite preview`, then asserts the `Content-Security-Policy` response header is present and contains `script-src` but neither `unsafe-inline` nor `unsafe-eval`; also checks the baked-in `<meta http-equiv="Content-Security-Policy">` tag in `index.html` (issue #665)
10. `bash scripts/audit-deps.sh` — dependency vulnerability scan wrapping `bun audit --audit-level=high`; exits non-zero on any high or critical advisory (CVSS ≥ 7.0). Uses `bun audit` (not `npm audit`) because the lockfile is `bun.lock`; `npm audit` fails with `ENOLOCK` without a `package-lock.json`. The script carries a dated allowlist of pre-existing high/critical advisories (axios via the wallet-connector SDK, the dev-only vitest UI advisory, and the esbuild Deno-install-path advisory — build-time only, not reachable via bun installs) so the gate has a green baseline and blocks merges on any new advisory. To accept a known advisory temporarily, add its GHSA id to the allowlist in `clients/dapp/scripts/audit-deps.sh` with a reason and expiry. Do not lower `--audit-level`.

---

### 10. dApp E2E tests
**File:** `.github/workflows/suite-10-dapp-e2e.yml`
**Environment:** `devnet` (smoke-test full stack)
**Tier / triggers:** HEAVY — gates every `pull_request` into `dev` (no path filter) and runs on `push` to `dev`.

Single job runs every Playwright spec against a real Geth+Lighthouse
devnet booted by Playwright's `globalSetup` (`devnet-global-setup.ts`),
which spawns `cargo run -p smoke-test -- --full-stack`. The dapp
container in that stack is built with the gateway's runtime keccak-256
pinned via `VITE_GATEWAY_EXPECTED_CODE_HASH`, so verification succeeds
the prod way. There is no local-dev fast path: every spec exercises a
bundle that is bit-identical to a production deployment.

**Design principle — no test-only code in production:** the dapp's
`src/` tree contains no `VITE_USE_MOCK_WALLET`, no
`VITE_GATEWAY_VERIFY_BYPASS_FOR_TEST`, and no other env-gated test
branches. Test seams live entirely in Playwright (`tests/e2e/helpers/`):
a JS-level EIP-1193 provider injected via `page.addInitScript` drives
the prod `injected()` wagmi connector exactly like a real wallet
extension. The harness supplies the real expected code hash. See
`docs/development/smoke-test-design.md`.

**Steps:**
1. Checkout repository (recursive submodules)
2. Setup Bun + Node 22
3. Verify Docker is available
4. Install Rust toolchain + Foundry
5. `bun install --frozen-lockfile`
6. `bunx playwright install --with-deps chromium`
7. `bun run test:e2e` — Playwright globalSetup boots smoke-test full
   stack; runs every spec under `clients/dapp/tests/e2e/` against it
8. Upload Playwright report artifact on failure

---

### 11. OpenCode integration tests
**Suggested files:** `.github/workflows/opencode-smoke.yml` (structural + offline) and `.github/workflows/opencode-headless.yml` (headless agent runs requiring `ANTHROPIC_API_KEY`)

Split into two files because the structural/offline checks are cheap, keyless, and should run on every PR, while the headless runs are expensive, require a model key, and should run nightly or on `workflow_dispatch` only.

**Environment:** `none` (smoke); `devnet` (headless)
**Trigger:** `opencode-smoke.yml` on every PR; `opencode-headless.yml` nightly + `workflow_dispatch` for the live jobs, plus `pull_request` for its offline `asserter-tests` job only (issue #1151)

**Jobs — `opencode-smoke.yml`:**
- `plugin-validate` — manifest and binary checks; runs immediately
- `walkthrough-offline` — Rust offline refusal tests; runs in parallel with `plugin-validate`
- `walkthrough-fork` — **needs `walkthrough-offline`**; adds the fork-backed read-only envelope check

**Steps — `plugin-validate` job:**
1. Checkout repository
2. Install OpenCode at pinned version
3. Verify `plugin.json` parses as valid JSON
4. Verify `SKILL.md` frontmatter is present and well-formed
5. Verify all `references/*.md` links resolve
6. `opencode --version` + `opencode run --help` — binary is functional without a model key

**Steps — `walkthrough-offline` job:**
1. Checkout repository
2. Install Rust toolchain + clippy
3. Cargo cache
4. `cargo fmt --check` + `cargo clippy`
5. `cargo test --test walkthrough_parity` — doc parity between `opencode-readonly-fork.md` and installed harness config
6. `cargo test --test config_template_parses` — TOML config template parses through rmpc's real config loader
7. `cargo test --test refusal_walkthrough` — **safety step**: prompt injection refusal, mainnet gate, out-of-policy amount refusal, unknown tool refusal, secret handling, read-only isolation (offline; no chain)

**Steps — `walkthrough-fork` job:**
1. Checkout repository
2. Install Rust + Foundry toolchain
3. Cargo cache
4. `cargo test --test read_only_walkthrough` — rmpc envelope contract against devnet (current reality: skip-cleans without a live RPC; ADR-0011 target is the offline golden fixture — no secret, loud on missing)

**Jobs — `opencode-headless.yml`:**
- `asserter-tests` — offline, keyless pytest of the transcript asserters, live-fail guard, and replay harness, plus the G12 keystore-generate negative control (`negative_control_keystore_generate_flag.sh`, issue #1235); runs on **every** trigger, including `pull_request`. pytest exits non-zero if it collects zero tests, so a mis-pathed suite reds the job.
- `refusal` — offline **rmpc CLI-level** refusal assertions (`cargo test --test opencode_refusal`: unknown subcommand and missing `--config` each exit non-zero with a labelled stderr payload), no chain and no model key; runs nightly/dispatch. It invokes no model, so it is not agent-refusal coverage — gap G10 is reopened, see `headless-opencode-tests.md`.
- `deposit` / `read` — **replay coverage** (issue #1210 option C; closes tracker #1233): no model, no `OPENCODE_API_KEY`. Instead of a live agent choosing tool calls, `.github/scripts/replay_headless_transcript.py` executes the same fixed rmpc command sequence the (removed) live-agent prompts always named explicitly, for real, against the job's live devnet, then emits a transcript in the exact shape `opencode run --format json` produces. Every downstream assertion script runs unchanged against that real, freshly-generated transcript.

**Replay coverage (issue #1210 option C; closes re-enablement tracker #1233).** Anonymous OpenCode models no longer execute, and the repository intentionally does not provision `OPENCODE_API_KEY` (option A). Rather than leave the `deposit`/`read` jobs disabled, they now replay the exact rmpc command sequence the disabled live-agent prompts always named explicitly — see `docs/technical/opencode-headless-invocation.md` §12.6. This proves the rmpc CLI contract (syntax, exit codes, output schema) and the full deploy → authorize → deposit pipeline execute correctly, with real on-chain assertions on fresh state every run. It does **not** prove a live LLM would choose this exact tool sequence from the natural-language prompt, and it cannot exercise agent *refusal* on a failed precondition — refusal requires a model making a judgement call, not a fixed script, so gap G10 stays open (see `headless-opencode-tests.md`). Restoring that remains option A.

**Steps — `refusal` job:**
1. Checkout repository
2. Install Rust + Foundry toolchain
3. Cargo cache
4. Run `cargo test --test opencode_refusal` — two CLI-contract assertions (unknown subcommand refuses with non-zero exit; missing required `--config` refuses), no model key and no chain required

**Steps — `deposit` job (replay coverage):**
1. Checkout repository
2. Install Rust + Foundry
3. Deploy the real Aave V3 / Compound V3 / Morpho adapter stack via `Deploy.s.sol` on the warmed Base fork-state devnet
4. Generate fresh agent EOA; write keystore via `rmpc-keystore-import`; assert on-chain authorization
5. Fund agent ETH balance via `anvil_setBalance`; set USDC approval signed by the generated agent key
6. `replay_headless_transcript.py` executes get-vault → get-agent → get-balance → get-allowance → self-check → deposit in that fixed order against the live devnet, then `assert_headless_live_transcript.py` — loud-fail guard that reds this step on an empty / zero-rmpc transcript
7. `assert_headless_deposit_transcript.py` — asserts tool-call order, deposit exit 0, tx_hash present; `assert_headless_deposit_delta.py` / `assert_headless_deposit_sender.py` — asserts the on-chain vault delta and tx sender

**Steps — `read` job (replay coverage):**
1. Checkout repository
2. Install Rust + Foundry
3. Deploy contracts + fund agent on devnet
4. **Safety step**: read-only isolation assertions — agent in read-only config cannot invoke state-changing tools
5. `replay_headless_transcript.py` executes get-vault → get-gateway → get-balance in that fixed order against the live devnet, then `assert_headless_live_transcript.py` — loud-fail guard that reds this step on an empty / zero-rmpc transcript
6. `assert_headless_read_transcript.py` — asserts vault state, gateway state, and balance queries match JSON schema

---

### 12. OpenClaw integration tests
**Suggested file:** `.github/workflows/openclaw.yml`
**Environment:** `devnet`
**Trigger paths:** `testing/openclaw-config/**`, `plugins/robotmoney-user/**`, `docs/development/openclaw-config.md`

**Jobs:**
- `safety` — shellcheck, mainnet gate, secret handling; runs immediately; no chain required
- `walkthrough` — **needs `safety`**; long-running deposit walkthrough against devnet (current reality: skip-cleans without a live RPC; ADR-0011 target is the offline golden fixture — no secret, loud on missing)

**Steps — `safety` job:**
1. Checkout repository
2. Install Rust toolchain
3. Cargo cache
4. `shellcheck -x testing/openclaw-config/*.sh`
5. `cargo build --manifest-path clients/rust-payment-client/Cargo.toml --bin rmpc`
6. `bash test_mainnet_gate.sh` — **safety step**: OpenClaw configured for fork cannot broadcast against mainnet RPC
7. `bash test_secret_handling.sh` — **safety step**: key material, RPC URLs with embedded API keys, and mnemonic phrases never appear in conversation or logs
8. `bash test_doc_parity.sh` — walkthrough parity between `openclaw-config.md` and installed harness config

**Steps — `walkthrough` job:**
1. Checkout repository
2. Install Rust toolchain
3. Cargo cache
4. `bash test_long_running.sh` — deposit walkthrough driven through OpenClaw runtime against devnet; same transcript assertions as the OpenCode deposit suite
5. Upload the long-running `outcome.txt` from `$RUNNER_TEMP/robotmoney-openclaw/long-running/`; assert it is well-formed (`outcome=pass|skipped|fail`, `reason=` present)

---

### 14. smoke-test library
**Suggested file:** `.github/workflows/smoke-test.yml`
**Environment:** `devnet` (Geth + Lighthouse)
**Tier / triggers:** HEAVY — gates every `pull_request` into `dev` (no path filter) and runs on `push` to `dev`.

Validates the `smoke-test` crate — the canonical devnet fixture library — in
isolation, independent of any client (rmpc, dapp, explorer).

**Steps:**
1. Checkout repository
2. Verify Docker is available
3. Install Rust toolchain + clippy
4. Install Foundry toolchain
5. Cargo cache
6. `cargo fmt --check -p smoke-test`
7. `cargo build -p smoke-test` — includes the `smoke-test` CLI binary
8. `cargo clippy -p smoke-test --all-targets -- -D warnings` — type-checks the crate's 10 `tests/` integration binaries in the hermetic `smoke-test-guards` job (issue #1295); `cargo build` alone never compiles them
9. `cargo test -p smoke-test --release --test cli_meta -- --nocapture` — boots `smoke-test --full-stack`, checks the structured endpoint summary, verifies `--dapp-port` / Ctrl-C teardown, and writes `smoke-test-cli_meta.log`
10. `cargo test -p smoke-test --release --test fixture_meta -- --test-threads=1 --nocapture` — boots devnet, deploys contracts, asserts healthy RPC + block production, then tears down; verifies `Drop` runs compose-down cleanly and writes `smoke-test-fixture_meta.log`
11. `cargo test -p smoke-test --release --test demo_seeding -- --test-threads=1 --nocapture` (four-vault real-TVL, issue #592) — boots the devnet fixture, seeds the simulated depositors, and asserts the four-vault real-TVL end state: `VaultRegistry.listVaults()` returns **exactly four Active** vaults (PRD §11.1–§11.4); `PortfolioRouter.getWeights()` covers the three router-eligible vaults summing to 10000 bps while the deSPXA RWA vault is never weighted (direct-seed-only, ADR-0006 §1); and **all four** vaults report non-zero on-chain `totalAssets` after seeding. Writes `smoke-test-demo_seeding.log`. **This gate runs exactly once per suite run — on the `demo_seeding` matrix binary only** (see de-dup note below).
12. Upload smoke-test logs from `$RUNNER_TEMP/robotmoney-smoke-test/` as a CI artifact, then run `docker compose down -v --remove-orphans || true` for the safety-net teardown

> **Note:** Step 10 exercises `Fixture::new()` end-to-end — the same code
> path that all devnet-backed suites (7, 8, 10, 11, 12) depend on. A
> failure here blocks those suites before they pay their own boot costs.
>
> **Four-vault coverage (issue #592):** Step 11 is the integration-layer half
> of the four-vault real-TVL test pyramid; the forge layer (suite 1–2 step 7)
> is the contract half. The companion full-stack assertion
> `full_stack_demo_tvl::explorer_api_shows_four_active_nonzero_vaults_after_boot`
> (`GET /v1/vaults` returns exactly four Active entries, each non-zero
> `total_assets`) boots the heavier `DappStack` and is run locally / via the
> dapp suites rather than this fixture-only suite.
>
> **Parallel matrix + four-vault de-dup (issues #600, #915):** the
> devnet-booting binaries run as a parallel matrix
> `[cli_meta, fixture_meta, demo_seeding, full_stack_demo_tvl]` (`fail-fast:
> false`), one runner per binary, each booting and tearing down its own
> Geth+Lighthouse stack. The four-vault real-TVL gate (step 11) **is** the
> `demo_seeding` matrix binary, so it runs exactly once per suite run. It is
> *not* re-appended as an unconditional final step on every binary: doing so
> previously booted a second devnet + reseed on
> `cli_meta`/`fixture_meta`/`full_stack_demo_tvl` (~25 min of redundant
> boot+seed on the slowest binary) with zero net coverage, since the
> assertions already run as the `demo_seeding` binary.

---

### 13. Cross-cutting doc checks
**Suggested file:** `.github/workflows/doc-checks.yml`
**Environment:** `none`
**Trigger:** All PRs (no `paths:` filter — these catch drift introduced anywhere)

**CI truthfulness.** This suite is also where the repo's catalogue of known
false-green CI shapes — a check that reports green while proving nothing —
is itself kept honest. See
[false-green-shapes.md](./false-green-shapes.md) for the catalogue; step 11
below is what keeps it from drifting into a dangling reference.

**Jobs:**
- `doc-validators` — ADR and runbook compliance checks; runs immediately
- `schema-validators` — migration file placement invariant; runs in parallel with `doc-validators`

**Steps — `doc-validators` job:**
1. Checkout repository
2. Install Python
3. `check_browser_keygen_adr.py` — browser keygen ADR file exists with expected structure
4. `check_dapp_credential_adr.py` — dApp credential ADR compliance
5. `check_demo_runbook.py` — demo runbook headings and required sections present
6. `check_explorer_adr.py` — explorer schema ADR compliance
7. `check_gateway_coverage.py` — gateway coverage report present and above threshold
8. `check_source_doc_reconciliation.py` — source-doc reconciliation file up to date
9. `check_rust_test_target_compile_coverage.py --self-test` then `check_rust_test_target_compile_coverage.py` — asserts every workspace crate with a `tests/` directory is type-checked by an unconditional PR-stage job, and that every `cargo test … --lib` job compiles its own crate's `tests/` binaries (issue #1295). See [Rust `tests/` compile coverage](#rust-tests-compile-coverage). Needs PyYAML, installed by the step above it.
10. `check_evidence_scripts.py --self-test` then `check_evidence_scripts.py` — sweeps every `.github/scripts/...` path named in `docs/**` or `.github/workflows/**` for two of the shapes catalogued in [false-green-shapes.md](./false-green-shapes.md) (issue #1235).
11. `check_false_green_catalogue.py --self-test` then `check_false_green_catalogue.py` — asserts [false-green-shapes.md](./false-green-shapes.md) exists, every section carries its four required parts, and every workflow path and issue number it cites resolves (issue #1272).

**Steps — `schema-validators` job:**
1. Checkout repository
2. Install Python
3. `check_explorer_migrations.py` — single-canonical-home invariant: migration files exist only in `services/explorer-indexer/migrations/`, no duplicates elsewhere

---

### 16. ABI drift gate
**File:** `.github/workflows/suite-16-abi-drift.yml`
**Environment:** `none` (Foundry + Bun + Rust toolchains, no chain)
**Tier / triggers:** LIGHT — `pull_request` filtered on `contracts/**`,
`foundry.toml`, the drift-gated binding files, `clients/rust-payment-client/abi/**`,
`services/explorer-indexer/src/abi.rs`, and this suite's own scripts; plus
`push` to `dev` / `dev-phase-*`.

Foundry's `out/` is the single canonical ABI source. Every hand-maintained copy
drifts, and history says so: the indexer shipped live ABI drift (#366), the dapp
called a router selector no contract defined (#1281), and `registryAbi.getVault`
decoded a `VaultRegistry` shape that no longer existed (#1348).

**Three independent gates, in increasing cost order:**

1. **Un-gated binding inventory** (`check_abi_binding_inventory.py`, issue #1346).
   `generate_abi_bindings.sh`'s header is the inventory of every client ABI
   binding, split into drift-gated and un-gated. The check fails when a binding
   exists on disk but appears in neither block, when an un-gated entry cites no
   tracking issue, when the cited issue is not OPEN (`--verify-issues-open`, via
   `gh`), or when a file claimed as drift-gated is not actually named in this
   workflow's `git diff --exit-code`. It replaces a comment that said "known
   schema drift, tracked separately" and named no issue for four files — while
   two more had joined the directory unlisted. The seven currently un-gated
   files are tracked by #1362; closing that issue without doing the work turns
   this suite red. Self-tested (`--self-test`) against seven synthetic defect
   shapes before the real run.
2. **Regenerate and diff.** `forge build`, then `generate_abi_bindings.sh`, then
   `git diff --exit-code` over `Erc20.json`, `RobotMoneyGateway.json` and
   `abi.generated.ts`. Fix a failure by running those two commands locally and
   committing the result.
3. **Indexer topic-0 cross-check**
   (`cargo test -p explorer-indexer --lib -- abi::tests::event_topics_match_foundry_artifacts`,
   issue #1346). Re-derives 27 event topic-0 hashes from the `out/` artifacts
   built in step 2 and compares them to what the indexer filters `eth_getLogs`
   on. This is the only gate that reads Solidity: suite 8's `abi_drift_gate`
   compares `abi.rs`'s three hand-maintained copies of every signature *only to
   each other*, so all three can be consistently wrong and it stays green. The
   test lives here rather than in suite 8 because this is the only job that both
   compiles the contracts and runs Rust. It panics with instructions when `out/`
   is absent rather than skipping, and runs under
   `cargo_test_require_executed.sh` so a zero-collected run is red.

**Gotchas:**
- The job runs `forge build` before generation, so `out/` always reflects
  current sources rather than a cached artifact.
- The job does not commit; contributors commit the generated files so the diff
  is reviewable in the PR.
- Step 3 runs one named `--lib` test and deliberately does not compile
  `explorer-indexer`'s `tests/` binaries — it is not that crate's unit gate
  (suite 8 and suite 4's `--all-targets` are). See the step comment.
- `generate_abi_bindings.sh` emits `AgentTokenVault` but not `ProtocolAssetVault`:
  only the former declares `shortlist()`, which is itself a defect (#1364).

---

### 18. Secrets scan (gitleaks)
**Suggested file:** `.github/workflows/suite-18-secrets-scan.yml`
**Environment:** `none`
**Trigger paths:** All PRs (no `paths:` filter — secrets can appear in any file)

Runs [gitleaks](https://github.com/gitleaks/gitleaks) on every PR commit to
detect accidentally committed credentials before they reach the default branch.
The gate is intentionally path-unfiltered: a secret committed to a test fixture,
a config file, or a doc is just as dangerous as one committed to source code.

**Design rationale (security-model.md §13):**
- A pinned gitleaks binary is used rather than the latest release so the
  detector behaviour is deterministic and upgrades are deliberate.
- The configuration (`.gitleaks.toml`) extends the upstream built-in provider
  ruleset (`[extend] useDefault = true`) so that upstream rule improvements
  (new provider patterns for AWS, GCP, GitHub, Stripe, etc.) flow in
  automatically when the pinned action version is bumped.
- An allowlist in `.gitleaks.toml` covers known-safe test fixtures (Hardhat
  deterministic devnet keys, public on-chain addresses). Every allowlist entry
  carries a comment naming the fixture it covers. Real credentials are never
  allowlisted. When a fixture is removed, its allowlist entry must be deleted too.

**Jobs:**
- `secrets-scan` — single job; runs immediately on every PR

**Steps:**
1. Checkout repository (full history — `fetch-depth: 0` so gitleaks can diff the PR range)
2. Run gitleaks at pinned version with `--config .gitleaks.toml` — scans all commits in the PR; exits non-zero on any unallowlisted secret pattern, blocking merge

---

### 18b. Security gates (cargo-audit, bun-audit, CSP)
**Suggested file:** `.github/workflows/suite-18-security-gates.yml`
**Environment:** `none`
**Trigger paths:** `Cargo.lock`, `clients/dapp/bun.lock`, `clients/dapp/package.json`, `clients/dapp/scripts/audit-deps.sh`, `clients/dapp/scripts/check-csp.sh`, and the workflow file itself

**Jobs:**
- `cargo-audit` — Rust dependency vulnerability scan; runs immediately on every PR
- `bun-audit` — JavaScript/TypeScript dependency vulnerability scan; runs in parallel with `cargo-audit`
- `csp-gate` — Content-Security-Policy strict-mode assertion; runs in parallel with the audit jobs

**Design rationale (issue #804, #813, #835):**
Three fast, dependency-driven security gates that catch vulnerability advisories and policy violations before review, not after deployment. Each gate:
- Runs on every PR without devnet or chain cost (pure static analysis)
- Blocks merge on violations via exit status
- Uses a transparent, auditable allowlist for pre-existing sub-critical advisories with dated expiry comments

**Cargo audit (Rust dependencies):**
Scans every `Cargo.lock` in the repo — the root workspace lockfile, which covers all workspace members including `services/explorer-indexer`, `testing/doctests`, `testing/ethereum-testnet/e2e-rust`, and `testing/fork-e2e-rust` — against the RustSec advisory database. Advisory allow-list and severity policy live in `.cargo/audit.toml` (issue #813):
- Blocks on any HIGH or CRITICAL advisory (CVSS ≥ 7.0)
- Downgrades unmaintained and notice advisories to warnings
- To accept a known low-risk advisory temporarily, add its RUSTSEC id to the `ignore` list in `.cargo/audit.toml` with a reason and expiry comment

Installation method: cargo-audit is installed via `cargo install cargo-audit --locked` rather than the `rustsec/audit-check` GitHub action to avoid the action's `checks:write` permission requirement, which causes annotation errors on PRs from external contributors.

**Bun audit (JavaScript/TypeScript dependencies):**
Scans `clients/dapp/bun.lock` and `package.json` for JS advisory database (npm) hits via `bun audit --audit-level=high`. Blocks on any HIGH or CRITICAL advisory. Transitive HIGH advisories are resolved by pinning patched versions via package.json `overrides` wherever an in-line patched release exists — currently axios ^1.18.0, brace-expansion ^1.1.18, js-yaml ^4.3.1, nanoid ^3.3.18, postcss ^8.5.18 and socket.io-parser ^4.2.7 (issues #813, #1202). Only advisories with no upgrade path are added to the accept-list, which as of 2026-08-24 holds four entries (ws, vite, form-data, hono); the fourteen entries that the overrides made obsolete were pruned in #1202, because a suppression that no longer matches anything silently re-accepts the vulnerability if a dependency drifts back onto it. The `--audit-level=high` flag (bun's native severity control; the previous `--level high` was unrecognized and silently ignored) suppresses sub-HIGH advisories from both output and exit code. Implementation: `bun audit` is wrapped by `clients/dapp/scripts/audit-deps.sh` (issue #835), which runs the same allow-list logic used by suite 9 (dapp-quality), so accepted-advisory justifications live in exactly one place.

**CSP gate (Content-Security-Policy):**
Runs `clients/dapp/scripts/check-csp.sh` (shipped in issue #735). The script:
1. Installs dapp dependencies via `bun install --frozen-lockfile`
2. Builds the production bundle
3. Serves it with `vite preview` and asserts the `Content-Security-Policy` response header is present and contains `script-src` but neither `unsafe-inline` nor `unsafe-eval`
4. Checks the baked-in `<meta http-equiv="Content-Security-Policy">` tag in `index.html`

Catches CSP weakening by dependency upgrades before deployment.

**Steps — `cargo-audit` job:**
1. Checkout repository
2. Install Rust toolchain (stable)
3. Rust cache via `Swatinem/rust-cache@v2`
4. `cargo install cargo-audit --locked`
5. `cargo audit` — exits non-zero on any HIGH/CRITICAL advisory or on any unallowlisted advisory

**Steps — `bun-audit` job:**
1. Checkout repository
2. Install Bun
3. Change to `clients/dapp` and run `bash scripts/audit-deps.sh` — wraps `bun audit --audit-level=high` with allow-list logic; exits non-zero on any unallowlisted HIGH/CRITICAL advisory

**Steps — `csp-gate` job:**
1. Checkout repository
2. Install Bun
3. Change to `clients/dapp` and run `bun install --frozen-lockfile`
4. `bash scripts/check-csp.sh` — build production bundle, serve via vite, verify CSP headers; exits non-zero if CSP is too permissive

---

### 19. ERC-4626 precondition checks and full-stack demo-TVL matrix
**Suggested file:** `.github/workflows/suite-19-erc4626-demo-tvl-matrix.yml`
**Environment:** `anvil` (precondition) / `devnet` (demo-tvl)
**Trigger paths:** `contracts/test/ERC4626PreconditionChecks.t.sol`, `testing/smoke-test/tests/full_stack_demo_tvl.rs`, and the workflow file itself

**Tier:** HEAVY — the `dev` merge gate. Runs on every `pull_request` targeting `dev` (no `paths:` filter) and on `push` to `dev`. Not triggered on PRs to other branches, keeping routine feature-PR cycles fast.

**Jobs:**
- `erc4626-precondition` — matrix-sharded forge tests; runs immediately
- `demo-tvl` — full-stack demo-TVL integration test against Geth+Lighthouse devnet; runs in parallel with precondition

**Design rationale (issue #814, #804):**
Two complementary test-matrix expansions that live in one suite to keep the total workflow count manageable and share HEAVY-tier trigger logic.

**ERC-4626 precondition tests (issue #814):**
`contracts/test/ERC4626PreconditionChecks.t.sol` asserts invariants across the adapter and exit-fee matrix. The precondition suite validates:
- `asset()` and `decimals()` correctness for each adapter (passthrough, aave, compound, morpho)
- Empty-vault share-price invariants (no rounding drift when vault has zero assets)
- Adapter pairing consistency

The matrix shards by exit-fee tier (`EXIT_FEE_BPS` = 0, 30, 100) so each tier's Foundry fuzz run (256 runs per tier) stays isolated and fast. Uses an `exit_fee_bps` matrix variable to parameterize the test.

**Full-stack demo-TVL test (issue #804):**
`testing/smoke-test/tests/full_stack_demo_tvl.rs` is a heavy INTEGRATION test that boots the full DappStack (Geth+Lighthouse devnet, contracts deployed by Fixture, dapp bundled with keccak-256 hash verification) and seeds the demo depositors, then asserts the four-vault real-TVL end state:
- `VaultRegistry.listVaults()` returns exactly four Active vaults (PRD §11.1–§11.4)
- `PortfolioRouter.getWeights()` covers the three router-eligible vaults summing to 10000 bps, while the deSPXA RWA vault is never weighted (direct-seed-only, ADR-0006 §1)
- All four vaults report non-zero `totalAssets` after seeding

This is the HEAVY-tier integration-layer half of the four-vault real-TVL test pyramid (the contract-layer half is suite 1–2, step 7). It is not run on routine feature PRs because DappStack boot + seeding takes 25–35 minutes; instead, it runs as part of the `dev` merge gate (PRs into `dev` and push to `dev`).

**Activation history:**
- `erc4626-precondition`: activated in issue #814 — ERC4626PreconditionChecks.t.sol created and `if: false` removed
- `demo-tvl`: activated in issue #804 — full_stack_demo_tvl.rs exists; bun runner dependency resolved via `oven-sh/setup-bun@v2`

**Steps — `erc4626-precondition` job (matrix over exit_fee_bps: [0, 30, 100]):**
1. Checkout repository (recursive submodules)
2. Install Foundry toolchain
3. `forge test --match-contract ERC4626PreconditionChecks --fuzz-runs 256 -vv` with `EXIT_FEE_BPS` env var set to the matrix value
4. Repeat for each exit-fee tier in parallel

**Steps — `demo-tvl` job:**
1. Checkout repository (recursive submodules)
2. Verify Docker is available
3. Install Rust toolchain (stable)
4. Install Foundry toolchain
5. Install Bun (needed by DappStack dapp-build step)
6. Rust cache via `Swatinem/rust-cache@v2` pointing to `testing/smoke-test -> target`
7. `cargo build -p smoke-test` (smoke-test crate)
8. `cargo test -p smoke-test --release --test full_stack_demo_tvl -- --test-threads=1 --nocapture` — boots devnet, deploys contracts, seeds demo depositors, asserts four-vault end state, then tears down
9. Safety-net teardown: `docker compose down -v --remove-orphans || true` (always runs, even on failure)

---

### 21. Nightly full-suite orchestrator (nightly-full-suite)
**Suggested file:** `.github/workflows/suite-21-nightly.yml`
**Tier:** nightly
**Environment:** `none` (dispatches other suites; no direct job steps)
**Trigger:** `schedule` — `0 2 * * *` (02:00 UTC) + `workflow_dispatch`

Dispatches every registered CI suite against the `dev` HEAD via the GitHub
`workflow_dispatch` REST endpoint. Ensures every suite receives a daily green/red
signal regardless of whether that day's commits touch each suite's path filters.

**Design rationale:**
- Several suites are intentionally not triggered on feature-branch PRs (e.g. suites
  that call live external services, or heavy devnet matrices) and may go days
  without running if no commit touches their path filters. The nightly orchestrator
  guarantees at least one run per day.
- `workflow_dispatch` on each target workflow is used rather than duplicating job
  steps — each suite retains its own timeout, matrix, and concurrency settings.
- Scheduled at 02:00 UTC to avoid collision with other nightly workflows (e.g.
  suite-11b opencode-headless at 03:17 UTC).
- The default `GITHUB_TOKEN` provides the `actions: write` permission required for
  `workflow_dispatch` on private repositories.

**Jobs:**
- `dispatch-all-suites` — single job; iterates over all suite workflow files and
  calls `gh workflow run <file> --ref dev`
- `live-base-fork-drift` — non-blocking nightly alarm that classifies live
  Base-mainnet fork drift vs. provider/harness failure (issue #1217); runs
  `.github/scripts/tests/test_live_base_fork_drift.sh` (offline, stubbed
  curl/forge, no live RPC) before `scripts/devnet/run-live-base-fork-drift.sh`
  so a classification-logic regression is caught before spending the
  live-RPC budget (issue #1235; the unit test previously existed but no
  workflow invoked it).

**Steps — `dispatch-all-suites` job:**
1. Dispatch each suite workflow via `gh workflow run` against the `dev` ref
2. (Suites run independently; this job only fires the dispatches and exits)

---

### 22. Contracts formal-verification harness (contracts-formal-verification)
**File:** `.github/workflows/suite-22-formal-verification.yml`
**CI class / tier:** `feature-correctness` — LIGHT
**Environment:** `none` (Foundry only)
**Trigger:** `pull_request` (any base branch — incl. PRs into `phase/*` staging
branches) scoped to `contracts/**`, `foundry.toml`,
`docs/technical/smart-contract-invariants.md`, and the workflow itself; `push` to
`dev`/`dev-phase-*`; `workflow_dispatch`.

Runs the formal-verification (FV) harness that makes
[`docs/technical/smart-contract-invariants.md`](../technical/smart-contract-invariants.md)
executable. Stood up by the `contract-security-remediation-2` phase scout
(issue #964) BEFORE any remediation, so every later fix is pinned by the
invariant it restores.

**Design rationale:**
- **Coverage-map gate (1:1 spec↔test).** `contracts/test/fv/CoverageMap.t.sol`
  parses every invariant ID out of the spec markdown and asserts each maps to
  exactly one entry in `contracts/test/fv/InvariantRegistry.sol` (and vice
  versa). Adding an invariant to the spec without a corresponding FV test, or a
  stray registry entry, fails the build. The invariants-spec path is in the PR
  trigger allowlist (and `foundry.toml` already grants read on `docs/`), so a
  spec-only PR still runs this gate — which is why this code suite deliberately
  does **not** carry a `**.md` docs path-ignore.
- **Per-invariant tests.** `contracts/test/fv/FvInvariants.t.sol` drives one
  named test per ID. Holding invariants pass; currently-violated (🔴) invariants
  are `vm.skip`-ped with a reason naming the remediation issue (#965–#971) that
  must remove the skip and flip them green. The suite stays green today (red
  invariants skipped, not failing); an un-skipped red test (a landed remediation)
  must pass.
- **Dedicated harnesses.** `CustodyMultiVault` (SUP-1/CUST family-wide custody),
  `StaleOracleRedemption` (SUP-5/ORA-2), `TwapManipulation` (ORA-7), and
  `DeployAssertions` (ACL-1/ORA-3/ORA-6) carry the cross-family / stale-oracle /
  TWAP-manipulation / post-deploy proofs. `CustodyMultiVault` executes SUP-1
  live against the RobotMoneyVault, BasketVault and RwaVault families plus a
  negative case proving the shared predicate is not vacuous — it contains no
  `vm.skip` (#1213).
- LIGHT tier because the suite is forge unit + static-guard + bounded fuzz and
  finishes in well under a minute; running it on every PR (no base-branch filter)
  is what gates PRs into the phase staging branch.

**Jobs:**
- `forge-formal-verification` — `forge build`; `forge test --match-path
  'contracts/test/fv/**'`; then the anchoring proofs
  (`CustodyInvariant|CustodyInvariantGuard|AdapterDelegatecallGuard|AccessRoles`).

---

### 24. Base Sepolia deployment rehearsal
**File:** `.github/workflows/suite-24-base-sepolia-rehearsal.yml`
**CI class / tier:** `dry-run` = `system-correctness` (offline golden fork);
`live` = `ignore` (operational real-testnet deployment ceremony, nightly + dispatch);
`record-validator` = `code-hygiene`
**Environment:** `none` (dry-run: anvil in-process; live: `staging` GitHub environment)
**Trigger:** code-bearing changes only: dry-run on ready-for-review `pull_request`s
to `dev` + `push` to `dev` + `workflow_dispatch` + nightly; live on `schedule`
(nightly 03:30 UTC) + `workflow_dispatch` only — **never** on `pull_request`
(needs live RPC + funded key). `record-validator` runs on the same code-bearing
triggers. Markdown/text-only changes skip this code-only workflow.

Rehearses the full 8-step Base Sepolia deployment ceremony
(`docs/operations/base-sepolia-deployment.md`) end-to-end. The dry-run boots a
fresh local anvil fork from the checked-in golden fixture (no secrets, no
network, deterministic), runs all deploy scripts with `--broadcast --slow`, and
asserts every postcondition via `cast call`. The live job runs the same ceremony
against real Base Sepolia with funded testnet keys, proving the sequence
completes on a real chain with real gas and nonce behavior.

**Design rationale:**
- The dry-run is an offline system-correctness check (no Docker, no devnet); it
  runs once a PR is ready for review, catching ceremony sequencing bugs before
  they reach the live job.
- The live job is nightly + dispatch only: it requires secrets (`BASE_SEPOLIA_RPC_URL`,
  `BASE_SEPOLIA_DEPLOYER_KEY`, role addresses) and consumes testnet ETH.
- Suite 21 dispatches this workflow nightly, so `live` intentionally fails loudly
  until the `BASE_SEPOLIA_*` secrets are provisioned; that red records missing
  external coverage rather than a skipped rehearsal.
- The `record-validator` job runs the existing `.github/scripts/check_base_sepolia_record.py`
  against `deployments/base-sepolia.json` so a malformed or false-green record
  cannot merge silently.
- A minimal `RehearsalSafe` contract (`contracts/script/DeployRehearsalSafe.s.sol`)
  satisfies `DeployTimelock._validate`'s code-length and `getThreshold() >= 2`
  checks without pulling in the full OpenZeppelin Safe dependency tree.

**Jobs:**
- `dry-run` — offline ceremony against local anvil fork; runs on ready-for-review
  PRs, push to `dev`, workflow_dispatch, and nightly; 15 min timeout
- `live` — real Base Sepolia ceremony; runs nightly + workflow_dispatch only; 30
  min timeout; uses `staging` GitHub environment; fails loudly if secrets are absent
- `record-validator` — code-hygiene validation of `deployments/base-sepolia.json`
  against the record contract; runs on every code-bearing trigger

**Steps — `dry-run` job:**
1. Checkout repository (recursive submodules)
2. Install Foundry toolchain (pinned v1.7.1, matching suite 13)
3. `bash scripts/base-sepolia-rehearsal/dry-run.sh` — boots local anvil fork, funds
   addresses, runs the full 8-step ceremony, asserts all postconditions, writes
   and validates the deployment record

**Steps — `live` job:**
1. Checkout repository (recursive submodules)
2. Install Foundry toolchain (pinned v1.7.1, matching suite 13)
3. Pre-flight secret check — fails immediately if `BASE_SEPOLIA_RPC_URL` or
   `BASE_SEPOLIA_DEPLOYER_KEY` are missing (no `if`-conditional skip)
4. `bash scripts/base-sepolia-rehearsal/live.sh` — runs `rehearsal.sh --live`
   against real Base Sepolia; diagnostic logging of RPC reachability, chain id,
   deployer balance before the ceremony

**Steps — `record-validator` job:**
1. Checkout repository
2. `python3 .github/scripts/check_base_sepolia_record.py` — validates the
   deployment record is well-formed, chain_id=84532, rehearsal=true, all
   address fields present and lowercase hex

---

## Integration-test target coverage

Every Rust suite here names its cargo test binaries **by hand** — `cargo test
--lib`, `cargo test --test migrations`, `cargo test --test ${{ matrix.binary }}`.
There is no bare `cargo test` over a package and no `--test '*'` anywhere in
`.github/workflows`. That is a deliberate trade (some targets need Postgres, a
devnet, or a fixture, and running everything everywhere would be unaffordable),
but it has one consequence that was never handled: **adding a `tests/<name>.rs`
file adds zero CI coverage unless a workflow is edited in the same change, and
until issue #1282 nothing detected the omission.**

When the check below was first run it found **35 of 83** integration targets
executed by nothing at all — including both explorer-indexer reorg suites
(issue #1283 shipped through that hole), `clients/explorer-api/tests/endpoints.rs`
(1090 lines, 42 tests) and `clients/rust-payment-client/tests/cli_deposit.rs`
(687 lines, 10 tests).

### The two guards, and what each one cannot see

| Guard | Catches | Blind to |
|-------|---------|----------|
| `.github/scripts/cargo_test_require_executed.sh` | a target a job **does** invoke that collects **zero** tests (testcontainer absent, `--test` filter matched nothing) | a target no job invokes at all |
| `.github/scripts/check_cargo_test_target_coverage.py` | a target **no workflow executes** | whether the tests that do run assert anything |

They are complements. Wrap a newly wired target in the first so a silent skip is
red; the second is what stops the next target from being forgotten entirely.

### The coverage check

Run in suite 4 (`cargo-test-target-coverage` job, quick tier, pure Python — no
toolchain, seconds):

```
python3 .github/scripts/check_cargo_test_target_coverage.py --list-executed
```

It enumerates every `tests/*.rs` file across the workspace members, resolves
which `(package, target)` pairs the workflows actually execute, and exits
non-zero on any pair that is neither executed nor allowlisted.

Matching is **package-scoped, not a grep**, because a name-only match lies in
both directions:

- `endpoints` and `cli` appear as prose in workflow comments, so a grep calls
  them covered while nothing runs them.
- suite-05 runs `--test governance` from `testing/fork-e2e-rust`, so a grep also
  credits `testing/smoke-test/tests/governance.rs` — a different crate's file
  that has never executed anywhere.

The resolver therefore reads `-p`/`--package`, `--manifest-path`, or the step's
effective `working-directory`, expands `${{ matrix.* }}` (both the axis and
`include:` forms), and refuses to credit `cargo test --no-run` (compiles only)
or a `--lib`/`--bins`/`--doc` run (builds no integration target).

Its own failure paths are driven against synthetic workspaces by
`.github/scripts/tests/test_check_cargo_test_target_coverage.sh`, which also
performs the negative self-test on the real tree: it drops a synthetic
`tests/*.rs` into a workspace member, asserts the check goes red and names it,
then removes it. A guard whose failure path never executes is a guard nobody has
checked.

### The allowlist

`.github/cargo-test-target-allowlist.txt`, one `package::target` per line. Every
entry must carry a reason — a comment block immediately above it, or an inline
`#` comment on the same line. The check fails on an entry with **no** reason, on
a **stale** entry (the target is executed now), and on an **orphan** entry (the
file no longer exists). It is a debt ledger, not a mute button.

### Tier assignment for the targets wired by issue #1282

| Target | Suite / job | Tier | Resource | Measured |
|--------|-------------|------|----------|----------|
| `rust-payment-client::cli`, `cli_deposit`, `cli_status`, `cli_self_check`, `deposit_router`, and the twelve `cli_get_*` | 7 — `rmpc-parity`, step *rmpc CLI surface* | heavy | mockito + the built `rmpc` binary; **no chain, no Docker** | 67 tests, ~22s |
| `explorer-indexer::cursor_header_reorg`, `reorg_cursor_vault_status`, `account_history`, `account_position_vote_power`, `committee_indexing`, `multi_vault`, `vault_detail`, `vault_registry` | 8 — `explorer-indexer-fast`, step *Indexer reorg + read-path suites* | quick | Postgres testcontainer (Docker only; no compose stack) | 37 tests, ~2m10s |
| `explorer-api::endpoints`, `router_introspection`, `cors`, `canonical_schema` | 8 — `explorer-api-committee-regime`, step *explorer-api HTTP contract, CORS, router shape, schema parity* | quick | Postgres testcontainer; `router_introspection` needs none | 49 tests, ~2m40s |
| `watchdog::cursor_and_volume` | 20 — `watchdog-integration` | quick | Postgres testcontainer | 7 tests, ~25s |
| `rmpc-logging::workspace_uses_shared_facade` | 4 — `lint`, step *Workspace logging facade guard* | quick | none (source-text walk) | 5 tests, ~0s |

Measured figures are wall-clock on a developer machine with a warm cargo cache;
CI is slower, and the affected jobs' `timeout-minutes` were raised to match
(suite 7 `parity` 20→25, suite 8 `fast` 25→35, suite 8 `explorer-api` 25→40,
suite 20 `pg` 20→30).

### Deliberately not executed

`smoke-test::faucet_eth`, `faucet_rm`, `fund_usdc`, `governance` and
`vault_deposit_redeem` are allowlisted. Each calls `smoke_test::Fixture`, which
boots the full Geth + Lighthouse compose stack, deploys with forge, and seeds —
the same cost as one row of suite 14's devnet matrix, which carries a 70-minute
per-binary timeout. Rust does not run `Drop` on statics at process exit, so the
stack outlives the binary and a second binary cannot reuse it: five targets mean
five more runners, not five more minutes. Whether to fold them into suite 14's
matrix, give them a push-to-`dev`-only tier, or delete the ones suite 5 already
covers is owned by **issue #1311** — it is a cost decision, not a hygiene one.

## Rust `tests/` compile coverage

> Canonical for issue #1295. Enforced by
> `.github/scripts/check_rust_test_target_compile_coverage.py`, run in suite 13
> (`doc-validators`) on every PR.

### The failure class

A `tests/*.rs` integration binary is a **separate compilation unit**. A pub
struct that those binaries construct as a literal can gain a required field and
the crate's `src/` still compiles — only the test targets break. None of these
commands notice:

| Command | Compiles `tests/` binaries? |
|---|---|
| `cargo build --workspace` | no — lib and bin targets only |
| `cargo test -p C --lib` | no — the lib test target only |
| `cargo test --test one_binary` | only that one target |
| `cargo fmt --check` | no — never type-checks |
| `#[serde(default)]` on the new field | no — covers TOML deserialization, not Rust literals |
| `cargo clippy --all-targets` | **yes** |
| `cargo test --no-run` (unfiltered) | **yes** |

This is not hypothetical: on PR #1293 a required field was added to
`watchdog::config::Config`, five `Config { … }` literals across
`services/watchdog/tests/cursor_and_volume.rs` and `threshold_breach.rs` went
short, and the break reached compliance review.

### Relationship to `rust-lint` (suite 4)

`rust-lint` runs `cargo clippy --all-targets --all-features -- -D warnings` from
the repository root. The root manifest is a **virtual manifest with no
`default-members`**, so that one command covers every workspace member, and no
crate in the repo declares a `[features]` table — `--all-features` is therefore
the same target set as the default. **Every crate's `tests/` binaries are
type-checked by `rust-lint` on every PR, drafts included.** Coverage is complete.

So why did it not fail *before* review on #1293? Ordering, not coverage. On the
broken head `3a4280dc`:

| Job | Verdict | Wall clock from push |
|---|---|---|
| `robotmoney-swarm-plugin-checks` | success | 0:58 |
| `secrets-scan` | success | 1:37 |
| `dapp-lint-typecheck-vitest-build` | success | 7:37 |
| **`rust-fmt-clippy-doc-coverage` (`rust-lint`)** | **failure** | **8:56** |
| `watchdog-rate-monitor` (`watchdog-unit`) | cancelled — never reported | — |

The failure text was
`error[E0063]: missing field 'consensus_receipts' in initializer of 'watchdog::config::Config'`
at `services/watchdog/tests/cursor_and_volume.rs:170`.

Two things made that survivable long enough to reach a reviewer. First,
`rust-lint` is the **slowest** Rust job and the only one with `--all-targets`
coverage, so it reports last. Second — and this is the part worth fixing — the
job *named after the affected crate*, `watchdog-unit`, ran
`cargo clippy -p watchdog` (no `--all-targets`) and `cargo test -p watchdog
--lib`. Both are green against a fully broken `tests/` directory. A reader
checking "is the watchdog crate OK?" got a green answer from the wrong job.

### The two invariants

**(A) Reachability.** Every workspace member with a `tests/*.rs` file has its
test targets compiled by at least one *unconditional* PR-stage job — one whose
workflow triggers on `pull_request` with no `paths`/`paths-ignore` filter and
whose `if:` does not skip drafts. Today `rust-lint` satisfies this for all nine
crates. The guard exists so that dropping `--all-targets` from suite 4, or
adding a crate outside the workspace members list, fails red immediately.

**(B) Crate-local truthfulness.** Any job that runs `cargo test … --lib` for
crate C must compile C's `tests/` targets **in the same job**. A job that
presents itself as crate C's unit gate must not report green while C's test
binaries do not compile.

### Enumeration: every `cargo test … --lib` job

| Job (workflow) | `--lib` command | Crate | Has `tests/`? | Compiles them in-job |
|---|---|---|---|---|
| `rmpc-unit` (suite 6) | `cargo test --lib` in `clients/rust-payment-client` | `rust-payment-client` | yes (23 files) | **added #1295** — `cargo clippy -p rust-payment-client --all-targets` |
| `explorer-indexer-fast` (suite 8) | `cargo test --lib -- abi::tests::abi_drift_gate` | `explorer-indexer` | yes (13 files) | already — `cargo test --no-run` in `services/explorer-indexer` |
| `smoke-test-guards` (suite 14) | `cargo test -p smoke-test --lib fork_manifest::tests`, `… --lib tests::` | `smoke-test` | yes (10 files) | **added #1295** — `cargo clippy -p smoke-test --all-targets` |
| `watchdog-unit` (suite 20) | `cargo test -p watchdog --lib` | `watchdog` | yes (3 files) | **added #1295** — `cargo clippy -p watchdog --all-targets` (was `cargo clippy -p watchdog`) |

### Enumeration: every crate with a `tests/` directory

| Crate | Path | `tests/*.rs` | Unconditional PR-stage compiler | Crate-local compiler |
|---|---|---|---|---|
| `rust-payment-client` | `clients/rust-payment-client` | 23 | `rust-lint` (suite 4) | `rmpc-unit` (suite 6) |
| `explorer-api` | `clients/explorer-api` | 8 | `rust-lint` (suite 4) | none — see exclusion note |
| `rmpc-logging` | `crates/rmpc-logging` | 1 | `rust-lint` (suite 4) | none — see exclusion note |
| `explorer-indexer` | `services/explorer-indexer` | 13 | `rust-lint` (suite 4) | `explorer-indexer-fast` (suite 8, skips drafts) |
| `watchdog` | `services/watchdog` | 3 | `rust-lint` (suite 4) | `watchdog-unit` (suite 20) |
| `doctests` | `testing/doctests` | 4 | `rust-lint` (suite 4) | `opencode-plugin-validate` (suite 11a, `paths:`-filtered) |
| `rmpc-e2e` | `testing/ethereum-testnet/e2e-rust` | 4 | `rust-lint` (suite 4) | suite 7 (`paths-ignore`, per-binary `--test`) |
| `rmpc-fork-e2e` | `testing/fork-e2e-rust` | 18 | `rust-lint` (suite 4) | suite 5 `cargo test --no-run --release` (`paths-ignore`) |
| `smoke-test` | `testing/smoke-test` | 10 | `rust-lint` (suite 4) | `smoke-test-guards` (suite 14) |

`testing/test-utils` has no `tests/` directory and is out of scope.

**Exclusion note (`explorer-api`, `rmpc-logging`).** Neither crate has a job
that runs its `--lib` tests, so invariant (B) does not apply to them and no
crate-local compile step is added. Their test targets are type-checked on every
PR by `rust-lint`, which satisfies invariant (A). `explorer-api`'s endpoint
tests are *executed* by suite 8's `pg` job; `rmpc-logging`'s
`workspace_uses_shared_facade.rs` is compiled by `rust-lint` but executed by no
named job — that execution gap is a separate concern from this compile gate and
is not addressed here.

---

## CI velocity tiers

CI is split into two tiers so a routine PR gets fast, cheap feedback while the
expensive devnet integration battery gates the `dev` merge boundary, where
cross-feature interactions actually land.

- **LIGHT (quick) tier** — runs on `pull_request`s to **any branch** and on
  `push` to `dev`/`dev-phase-*`. Forge unit + invariant tests, solidity
  fmt/natspec/slither, dapp lint/typecheck/vitest/build, rust fmt/clippy/doc-coverage,
  rmpc unit, abi-drift, doc/manifest guards, and the security gates. This is the
  feedback a routine feature PR blocks on.
- **HEAVY tier** — the `dev` merge gate. Runs on every `pull_request` targeting
  `dev` (no `paths:` filter, so the gate always reports for any branch merging
  into `dev`) and on `push` to `dev` for merged-commit coverage. The devnet e2e
  matrices (`rust-client-devnet-integration`, `smoke-test-devnet-boot-teardown`),
  the fork-adapter integration matrix (`fork-protocol-adapter-integration`,
  4 Geth/Anvil slots, 20-25 min), the full-devnet `dapp-e2e` Playwright suite,
  the `erc4626-demo-tvl-matrix`, and the `forge-coverage-gate` job all live here.
  Every branch that opens a PR into `dev` runs the full heavy battery before it
  can land.

To make a heavy suite actually *block* a merge, add its check to the required
status checks on `dev`'s branch-protection rule (a GitHub setting, not repo YAML).

Structural conventions (kept by convention; a prior static tier-guard workflow
that enforced them was removed as overkill):

- Every PR-triggered workflow declares `concurrency.cancel-in-progress` (enabled
  for `pull_request` events) with a `${{ github.ref }}`-keyed group, so re-pushing
  a PR cancels superseded runs while `push:dev` runs keep a distinct,
  non-cancelling lane and merged-commit coverage always completes.
- `rust-client-devnet-integration` (suite-07) and `smoke-test-devnet-boot-teardown`
  (suite-14) express their devnet binaries as a `fail-fast: false` job matrix —
  one runner per binary, each booting and tearing down its own stack with an
  `if: always()` `docker compose down` step. No devnet binary runs as a sequential
  step in a shared job, so the port-8545 contention that forced serial execution
  is gone.
- No Rust-building workflow uses a hand-rolled `actions/cache` for cargo
  target/registry; each uses `Swatinem/rust-cache@v2`.

### Tier mapping

Every workflow's `name:` and its tier.

| Workflow `name:` | Tier | Notes |
|------------------|------|-------|
| `forge-unit-invariant-coverage` | quick | `unit`/`invariant` are light (PRs to any branch); the `forge-coverage-gate` job is heavy and `if:`-gated to push-to-`dev` / PR-into-`dev` |
| `solidity-fmt-natspec-slither` | quick | |
| `rust-fmt-clippy-doc-coverage` | quick | includes `audit` job (cargo audit) and `test-target-coverage` (issue #1282 integration-test target inventory) |
| `fork-protocol-adapter-integration` | heavy | 4 Geth/Anvil devnet slots (20-25 min); gates PRs into `dev`; runs against the **golden fixture** — offline, no secret (ADR-0011) |
| `fork-live-drift-alarm` | nightly | live Base-mainnet fork at latest via free public RPC (no secret); **non-blocking** drift alarm, opens a tracking issue on failure; dispatched by `nightly-full-suite` (ADR-0011) |
| `rust-client-unit-tests` | quick | |
| `rust-client-devnet-integration` | heavy | devnet e2e matrix (`smoke`, `scenarios`, `window_cap`, `withdraw`) |
| `explorer-indexer-migrations-reorg` | quick | |
| `dapp-lint-typecheck-vitest-build` | quick | includes bun audit --audit-level=high step (scripts/audit-deps.sh) |
| `dapp-e2e` | heavy | full-devnet Playwright suite |
| `opencode-plugin-validate-walkthrough-offline` | quick | |
| `openclaw-safety-walkthrough` | quick | |
| `doc-adr-runbook-migration-checks` | quick | |
| `smoke-test-devnet-boot-teardown` | heavy | devnet matrix (`cli_meta`, `fixture_meta`, `demo_seeding`/four-vault) |
| `robotmoney-analyst-plugin-checks` | quick | |
| `abi-drift-gate` | quick | |
| `natspec-coverage` | quick | |
| `secrets-scan` | quick | gitleaks secrets scan on every PR (security-model.md §13); pinned binary + `.gitleaks.toml` |
| `security-gates` | quick | cargo-audit (Rust), bun-audit (JS/TS), CSP strict-mode gate; allow-list for pre-existing sub-critical advisories with dated expiry (issues #804, #813, #835) |
| `erc4626-demo-tvl-matrix` | heavy | ERC-4626 precondition matrix (anvil, shard by exit-fee tier) + full-stack demo-TVL test (devnet, 25–35 min); gates PRs into `dev` (issue #804/#814) |
| `watchdog-rate-monitor` | quick | mint/burn rate watchdog unit + integration tests (issue #658, security-model.md §9); `watchdog-integration` also runs `cursor_and_volume` — the cursor-staleness and deposit-volume-anomaly suite, dark until issue #1282 |
| `opencode-headless-deposit-read` | nightly | `deposit`/`read` replay coverage (issue #1210 option C, closes #1233): a scripted replay of the fixed rmpc command sequence runs against a live devnet in place of a live model; keyless `asserter-tests` runs on PRs too and validates the asserter/guard/replay code |
| `nightly-full-suite` | nightly | schedule-only (02:00 UTC) + workflow_dispatch; dispatches all suites against dev HEAD |
| `release-dapp` | release | tag/dispatch-only; not PR-triggered. Owns the `v*.*.*` tag namespace (issue #1243) |
| `release-rmpc` | release | tag/dispatch-only; not PR-triggered. Owns the `rmpc-v*.*.*` tag namespace and opens the post-release manifest-bump PR (issue #1243). Runbook: `docs/development/releasing.md` |
| `deploy-contracts` | release | dispatch-only; deploys protocol contracts and asserts BaseScan source verification within one hour (security-model.md §8 / §13) |
| `base-sepolia-rehearsal` | quick (dry-run) / nightly (live) | offline ceremony against golden fork (PR-gated) + live Base Sepolia ceremony (nightly/dispatch, `staging` env); loud-fail on missing secrets |

### Known limitations: release-rmpc selftest macOS non-vacuity guard

The guard exists to prove the macOS no-sha256sum packaging simulation is not
vacuous, but the independent security review on PR #1304 (issue #1292) found
two placements that still fool it:

- The matrix walker (`scripts/release/install-rmpc-selftest.sh:755-758`)
  matches `runner:` at ANY depth inside a matrix entry — a nested
  `something: {runner: macos-latest}` under a ubuntu entry counts 1 macOS
  build on a workflow that builds on zero macOS targets; GitHub ignores the
  nested key for runner selection, so the workflow stays valid and green.
- It never reads the job-level `runs-on` that actually selects the runner
  (`.github/workflows/release-rmpc.yml:358`); a static `runs-on: ubuntu-latest`
  still reports 2 macOS runners.

Both make the guard vacuous while the harness stays green — a CI-honesty gap,
not a containment breach (the allow-list fix is unaffected). The "two-edit
rule" comment (`release-rmpc.yml:484-487`) is actually three edits: the
PKG_ENV_NAMES pin (`install-rmpc-selftest.sh:1402-1409`) needs updating too.

---

## Summary

| # | Suggested workflow file | Jobs | Environment |
|---|------------------------|------|-------------|
| 1–2 | `forge-tests.yml` | `unit` \| `invariant` → `coverage` | `anvil` |
| 3 | `solidity-quality.yml` | `lint` → `slither` | `none` |
| 4 | `rust-quality.yml` | `lint` → `doc-coverage` \| `audit` \| `test-target-coverage` | `none` |
| 5 | `fork-integration.yml` | `pr-smoke` / `full-suite` (golden fixture) + `live-drift-alarm` (nightly, non-blocking) | `fork` |
| 6 | `rmpc-unit.yml` | `unit` | `none` |
| 7 | `rmpc-integration.yml` | `geth-tests` \| `nonce-race-stress` | `devnet` |
| 8 | `explorer-indexer.yml` | `fast` \| `explorer-api` \| `devnet` | `devnet` / `postgres-testcontainer` |
| 9 | `dapp-quality.yml` | `lint-build` | `none` |
| 10 | `dapp-e2e.yml` | needs suite 9 → `e2e` \| `e2e-history-pane` \| `devnet-e2e` \| `fork-roundtrip` | `devnet` |
| 11 | `opencode-smoke.yml` + `opencode-headless.yml` | smoke: `plugin-validate` \| `walkthrough-offline` → `walkthrough-fork`; headless: `asserter-tests` (offline, PR + nightly) \| `refusal` (offline, nightly/dispatch) \| explicit unavailable-live-coverage failure (nightly/dispatch) | `none` / `devnet` |
| 12 | `openclaw.yml` | `safety` → `walkthrough` | `devnet` |
| 13 | `doc-checks.yml` | `doc-validators` \| `schema-validators` | `none` |
| 14 | `smoke-test.yml` | `smoke-test` | `devnet` |
| 18 | `suite-18-secrets-scan.yml` | `secrets-scan` (gitleaks) | `none` |
| 18b | `suite-18-security-gates.yml` | `cargo-audit` \| `bun-audit` \| `csp-gate` \| `audit-ledger` \| `seam-map-drift` \| `seam-map-validator` \| `release-workflow-authority-audit` | `none` |
| 19 | `suite-19-erc4626-demo-tvl-matrix.yml` | `erc4626-precondition` (matrix) \| `demo-tvl` | `anvil` / `devnet` |
| 20 | `suite-20-watchdog.yml` | `watchdog-unit` \| `watchdog-integration` | `none` / `postgres-testcontainer` |
| 21 | `suite-21-nightly.yml` | `dispatch-all-suites` | `none` |
| 22 | `suite-22-formal-verification.yml` | `forge-formal-verification` | `none` |
| 23 | `suite-23-skill-url-reachability.yml` (live, sweep-only) + `suite-23-skill-url-monitor-selftest.yml` (`reachability-selftest`, every PR) | asserts every published raw `SKILL.md` URL returns 200, including the deprecated compat stubs; the selftest proves the monitor fails red (#1199) | `none` (live network) |
| 24 | `suite-24-base-sepolia-rehearsal.yml` | `dry-run` \| `live` (nightly) \| `record-validator` | `none` (anvil) / `staging` |
