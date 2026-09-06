# Testcode Removal — Seam Map

**Scout issue:** #913
**Date:** 2026-06-18
**Canonical docs:** `.github/workflows/suite-11b-opencode-headless.yml`, `contracts/script/Deploy.s.sol`, `scripts/devnet/snapshot-fork.sh`, `docs/development/ci-suites.md`
**Downstream same-phase issues:** #912 (passthrough removal), #901
**Phase:** Testcode removal

---

## Purpose

This document maps the integration seams for the **Testcode removal** phase —
the removal of the `PassthroughAdapter` test escape hatch and its
`USE_PASSTHROUGH_ADAPTER` deploy branch from the production/devnet path. It is
a scout (stub/mapping) pass: **no contract, Rust, dapp, or workflow runtime
behaviour is changed by this issue.**

The phase's feature work (#912) replaces the passthrough escape hatch with the
real Aave V3 / Compound V3 / Morpho adapter path backed by the
`testing/fixtures/fork-state/CURRENT.anvil-state` snapshot, so every devnet
boot exercises real protocol state instead of a lossless stand-in.

---

## 1. CI-strategy finding — the phase premise is stale

The issue was specified under the **old** issue-#600 velocity-tier model, in
which the heavy devnet suites (suite-05 fork-integration, suite-07
rmpc-integration, suite-11b opencode-headless, suite-14 smoke-test) only
triggered on `push` to `dev`/`dev-phase-*` and on PRs **targeting**
`dev-phase-*`. Under that model a `dev-phase-testcode-removal` staging branch
was the only way to get those suites onto a feature PR's check set, and a
static `scripts/ci/check-workflow-tiers.sh` / `suite-17-ci-velocity-guard.yml`
guard policed the trigger YAML.

That model was **inverted and the guard deleted** by commit `a82b62d7`
(`ci(workflows): make heavy suites gate every PR into dev`), now on `dev`
(merge-base `8c91f9e0`). The current model on `origin/dev` and on this branch:

| Suite | Current trigger (`origin/dev`) |
|---|---|
| suite-05 fork-integration | `pull_request:` (no branch filter) + `push: [dev, dev-phase-*]` — runs on **every** PR |
| suite-07 rmpc-integration | `pull_request: branches: [dev]` + `push: [dev]` — gates every PR **into `dev`** |
| suite-14 smoke-test | `pull_request: branches: [dev]` + `push: [dev]` — gates every PR **into `dev`** |
| suite-19 erc4626-demo-tvl | `pull_request: branches: [dev]` + `push: [dev]` — gates every PR **into `dev`** |
| suite-11b opencode-headless | `schedule:` (nightly `17 3 * * *`) + `workflow_dispatch` only — **no `pull_request` trigger at all** |

Consequences for this phase's acceptance criteria:

- **AC #2 references a deleted script.** `scripts/ci/check-workflow-tiers.sh`
  (and its `scripts/ci/check_workflow_tiers.py` backing + `suite-17` workflow)
  were removed by `a82b62d7`. There is nothing to run; the "suite-17
  tier-guard invariant" cited in the issue body no longer exists. This AC is
  obsolete, not failing.
- **AC #3 is unachievable for suite-11b on any branch.** suite-11b has no
  `pull_request` trigger (nightly + manual dispatch only), so it never appears
  in a PR check set — not on a `dev-phase-*` PR and not on a `dev` PR. The
  other three heavy suites (05/07/14) already appear on **every PR into `dev`**
  with no staging branch needed.
- **The `dev-phase-testcode-removal` staging branch is no longer the
  mechanism that gets heavy suites onto #912's PR.** Targeting `dev` directly
  now triggers suite-05/07/14/19. A `dev-phase-*` base would actually trigger
  *fewer* heavy gates (only suite-05 by its unfiltered `pull_request`, since
  07/14/19 are filtered to `branches: [dev]`).

**Recommendation for #912:** open the passthrough-removal PR **against `dev`**
(not a phase branch) to get the suite-05/07/14/19 heavy gates automatically.
suite-11b must be validated by manual `workflow_dispatch` (or the nightly run)
regardless of base branch. A `dev-phase-testcode-removal` ref is created by
this scout to satisfy AC #1 and as an optional integration-staging convenience,
but it is **not** required for heavy-suite coverage and should not be relied on
for it.

---

## 2. Passthrough-removal seam map

### 2.1 `contracts/script/Deploy.s.sol` — the deploy branch

- **`Deployed.passthroughMode`** (struct field, ~line 99) — set from
  `vm.envOr("USE_PASSTHROUGH_ADAPTER", false)` at ~line 426.
- **Adapter-deploy branch** (`_doDeploy`, ~lines 419–432): when
  `passthroughMode` is true, deploys a single `PassthroughAdapter` and aliases
  all three typed adapter fields (`aaveAdapter`, `compoundAdapter`,
  `morphoAdapter`) to its address; otherwise deploys the three real adapters
  at canonical Base-mainnet addresses.
- **`_approveAndRegisterAdapters(Deployed)`** (~line 307) — the cap-bps
  surface. The passthrough branch registers the single adapter at
  **10_000 bps** (100%); the real branch registers the three adapters at
  **3_334 / 3_333 / 3_333 bps**. Removing passthrough means deleting the
  `if (d.passthroughMode)` arm here and in `_doDeploy`, the
  `passthroughMode` field, the `PassthroughAdapter` import (~line 15), and the
  three doc comments (~lines 32, 60–61, 84–85, 99–100, 385–386, 419–426, 458).
- All three call sites of `_approveAndRegisterAdapters` (~lines 156, 194, 231,
  268 — `run`, `runInProcess`, `runInProcessWith`) collapse to the single real
  branch once `passthroughMode` is gone.

### 2.2 suite-11b chain boot vs `fork-state/CURRENT`

- **suite-11b boots a bare devnet:** `.github/workflows/suite-11b-opencode-headless.yml`
  starts Anvil with `anvil &` (~line 108) — **no `--load-state`**, so it has
  no real Aave/Compound/Morpho storage. This is the historical reason the
  opencode-headless path needed passthrough: a bare Anvil can't satisfy the
  real adapters' protocol calls.
- **To remove passthrough here**, suite-11b's Anvil boot must load the snapshot:
  `anvil --load-state testing/fixtures/fork-state/CURRENT.anvil-state` (and
  likely pin chain-id / block to match the fixture manifest in
  `CURRENT.json`). Note the fixture is the pair
  `CURRENT.anvil-state` (load-state file) + `CURRENT.json` (manifest), **not** a
  `<CURRENT>` directory/symlink — `scripts/devnet/snapshot-fork.sh` writes
  `CURRENT.anvil-state` by copying the freshly-dumped `base-<BLOCK>.anvil-state`
  (~lines 524–542).
- `--dump-state`/`--load-state` schema is anvil-version-sensitive: the
  snapshot is produced inside the foundry Docker image (snapshot-fork.sh
  ~lines 40–49, 75–85) so the fixture round-trips into the CI anvil. suite-11b
  must use a compatible anvil version when it starts loading state.

### 2.3 `scripts/devnet/snapshot-fork.sh`

- Generates `testing/fixtures/fork-state/base-<BLOCK>.anvil-state` + `.json`
  and updates `CURRENT.anvil-state` / `CURRENT.json` (~lines 16–26, 494–542).
- It runs the **real** `Deploy.s.sol` (no `USE_PASSTHROUGH_ADAPTER`) plus
  `DeployVaultRegistry` / `PortfolioRouter` / `RouterGovernance` (~lines
  205–230) so the snapshot carries real adapter + registry bytecode and
  warmed protocol storage. This is already the no-passthrough path; the
  removal work makes the consumers (suite-11b, e2e-rust) match it.

### 2.4 Every `USE_PASSTHROUGH_ADAPTER` / `PassthroughAdapter` reference

Production / harness code (removal targets for #912):

- `contracts/script/Deploy.s.sol` — env branch + cap-bps (see 2.1).
- `contracts/adapters/PassthroughAdapter.sol` — the adapter contract itself
  (decide: delete vs keep test-only; see #901 scope).
- `testing/ethereum-testnet/e2e-rust/src/lib.rs` (~lines 108–120) — **injects
  `USE_PASSTHROUGH_ADAPTER=true` by default** into the deploy env unless the
  caller overrides it. This default injection is the main runtime removal in
  the e2e harness.
- `scripts/devnet/snapshot-fork.sh` — only mentions it in comments (does not
  set it); confirm no live use.
- `testing/smoke-test/src/lib.rs` (~lines 271, 954) and
  `testing/smoke-test/src/real_adapter_state.rs` (~lines 18–20) — comments
  noting `Fixture::new` **no longer** passes the env var; keep doc accuracy in
  sync when the var is deleted.
- `testing/smoke-test/tests/vault_deposit_redeem.rs` — deleted by issue #1311
  (its assertions were already covered by `smoke-test::fixture_meta` and
  `rmpc-fork-e2e::vault_deposit_redeem_smoke`, both of which genuinely execute
  today; `rmpc-fork-e2e::devnet_adapter_round_trip` does NOT count -- it
  unconditionally self-skips in CI because `RMPC_FORK_RPC_URL` is never set
  in suite-05, issue #1239); no longer a passthrough-reference removal target.

Test-only references (likely retained, decide in #901):

- `contracts/test/PassthroughAdapter.t.sol`, `contracts/test/Deploy.t.sol`,
  `contracts/test/RobotMoneyVault.t.sol`,
  `contracts/test/RobotMoneyVault4626Conformance.t.sol`,
  `contracts/test/AdapterDelegatecallGuard.t.sol`,
  `contracts/test/ERC4626PreconditionChecks.t.sol`,
  `contracts/test/DeployDemoExtraVaults.t.sol`.

Docs (update after code lands): `docs/architecture.md`,
`docs/technical/adapter-architecture.md`,
`docs/technical/security-hardening-seams.md`, the various `contracts/doc/...`
generated pages, and the audit/gap-analysis review docs.

### 2.5 suite-19 passthrough matrix axis

- `.github/workflows/suite-19-erc4626-demo-tvl-matrix.yml` runs
  `erc4626-precondition` (issue #814) — `contracts/test/ERC4626PreconditionChecks.t.sol`.
- The test asserts ERC-4626 preconditions over the matrix
  **exitFeeBps (0, 30, 100) × adapter {Passthrough, Aave, Compound, Morpho}**
  via the `AdapterKind` enum (~lines 69–104). `Passthrough` is the **first**
  axis member (`_assertPreconditions(AdapterKind.Passthrough, ...)`, ~line 101).
- Removing the passthrough deploy branch does **not** automatically remove this
  test axis: the precondition test constructs adapters directly with stub
  protocol immutables, independent of `Deploy.s.sol`. #912/#901 must decide
  whether the `Passthrough` matrix arm stays as a pure unit fixture or is
  dropped with the adapter. If `PassthroughAdapter.sol` is deleted, this axis
  (and the test imports at lines 9) must be removed too.

---

## 3. Discovered risks for downstream work

1. **AC #2/#3 are spec-stale (Section 1).** #912 should not target a
   `dev-phase-*` branch for heavy-suite coverage; target `dev`. suite-11b can
   only be validated via `workflow_dispatch`/nightly.
2. **suite-11b state-load is a real workflow edit (Section 2.2).** Removing
   passthrough requires pointing its Anvil boot at `CURRENT.anvil-state` with a
   version-compatible anvil; this is the highest-risk seam and is gated only by
   nightly/manual runs (no PR signal).
3. **Anvil dump/load version skew (Section 2.2).** The fixture is produced in
   the foundry Docker image; consumer anvil version must match or `--load-state`
   fails silently/partially.
4. **suite-19 Passthrough matrix arm (Section 2.5)** is independent of the
   deploy branch and must be handled explicitly if `PassthroughAdapter.sol` is
   deleted.
