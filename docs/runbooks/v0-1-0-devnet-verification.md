# v0.1.0 — Robot Money Devnet verification runbook

**Release identity:** `v0.1.0` (Robot Money Devnet, chain id `918453`) — the
default target per [ADR-0013](../adr/ADR-0013-twin-devnet-over-base-sepolia-for-testnet-verification.md).

**Delta this release verifies:** first end-to-end proof that the full
contract deploy ceremony (vault, all three yield adapters, gateway, vault
registry, portfolio router, router governance, IC policy + consensus
receipt) completes cleanly against production-parity protocol state, and
that a browser wallet can connect to the resulting stack and see correct
data. This is a **verification runbook**, not a deployment with a lasting
address record — the Devnet's containers are torn down on exit, so there is
no persisted `deployments/devnet.json` the way there is for Base Sepolia or
mainnet.

**Executed at commit:** `755e9b28` on `feat/testnet-verification-tool`
(carries the `SEED_DEPOSIT_AMOUNT` -> 1 USDC change from
[`docs/future/review-usdc-seed.md`](../future/review-usdc-seed.md) — every
address and balance below reflects that temporary value, not the production
1,000 USDC).

This runbook follows [`docs/operations/contract-release-runbooks.md`](../operations/contract-release-runbooks.md).
Read that first for what each gate below is actually proving.

## Preconditions

- `docker` on `PATH` (for `docker compose`)
- `forge` and `cast` on `PATH` (Foundry)
- `cargo` on `PATH` (Rust, for the `smoke-test` harness)

`smoke_test::prerequisites_available()` (`testing/smoke-test/src/lib.rs`)
checks all three and returns `false` if any is missing — the boot step below
fails loudly rather than silently skipping if a prerequisite is absent.

## 4.1 — Code-readiness gate

```bash
git status --short          # expect: clean
forge build                 # expect: success (contracts/)
```

## 4.2 — Preflight

The Devnet's own boot sequence *is* the preflight for this runbook: its
genesis is seeded from a pinned Base-mainnet state snapshot, so the
canonical Base mainnet addresses `Deploy.s.sol` hardcodes
(`AAVE_V3_POOL`, `COMPOUND_V3_COMET`, `MORPHO_GAUNTLET_USDC_PRIME`,
`AAVE_V3_A_TOKEN`) already have real, correct bytecode at chain id `918453`
— there is no separate address-validity check to run before boot the way
there is for Base Sepolia.

```bash
scripts/base-sepolia-rehearsal/preflight-guards.sh
```

runs the EIP-170 size gate and env-default guard against the current build
artifacts (network-agnostic despite the directory name).

## 4.3 — Cutover: boot the full stack

```bash
cargo run -p smoke-test -- --full-stack
```

**What this does.** Boots `docker compose` (Geth + Lighthouse), waits for
chain RPC readiness and real block production, then runs the same
`forge script` ceremony `docs/operations/base-sepolia-deployment.md`
documents (`Deploy.s.sol` → `DeployVaultRegistry.s.sol` →
`DeployPortfolioRouter.s.sol` → `DeployRouterGovernance.s.sol` →
`DeployInvestmentCommitteePolicy.s.sol`), seeds four demo depositors, then
boots the dapp, explorer-api, explorer-indexer, and Postgres containers.
Takes roughly 10-15 minutes end to end — most of it `--slow`-paced broadcasts
against the Devnet's 12-second block time, not compute.

**Not marked destructive.** Every address below is fresh on a throwaway
local chain that gets torn down on exit (§4.6) — nothing here is
irreversible the way a Base Sepolia or mainnet broadcast is.

It prints a structured endpoint summary once ready. From the run this
runbook is based on:

```
rpc_url=http://127.0.0.1:46583
dapp_url=http://localhost:40711
explorer_api_url=http://localhost:45847
chain_id=918453
gateway_addr=0x9f9f5fd89ad648f2c000c954d8d9c87743243ec5
vault_addr=0x17435cce3d1b4fa2e5f8a08ed921d57c6762a180
usdc_addr=0x833589fcd6edb6e08f4c7c32d4f71b54bda02913
registry_addr=0x3a8c1bd531b5c1aefbb9ebc3e021c1251cf4ccb1
router_addr=0x1430c9c2143f97aae765197e744baba7e78acaf0
governance_addr=0x2a3365c575a5fc8fd2842b82d29f8035e7f71cec
ic_policy_addr=0x6b3342821680031732bc7d4e88a6528478af9e38
consensus_receipt_addr=0x4004e893e863eca264ffa895566f0587b5f53b80
aave_adapter_addr=0x703848f4c85f18e3acd8196c8ec91eb0b7bd0797
compound_adapter_addr=0x422a3492e218383753d8006c7bfa97815b44373f
morpho_adapter_addr=0x0643d39d47cf0ea95dbea69bf11a7f8c4bc34968
admin_addr=0x8943545177806ED17B9F23F0a21ee5948eCaa776
```

Addresses (and the RPC/dapp/explorer ports) are re-derived fresh every boot —
**re-run the commands below against your own run's printed values**, not the
ones pasted here. `admin_addr` is the one exception: it is a fixed
`HARNESS_ADMIN` constant (`testing/smoke-test/src/lib.rs`), stable across
boots.

## 4.4 — Postflight: manual QA

Run these against **your own boot's** printed `rpc_url` and addresses.

**1. Role wiring.**

```bash
cast chain-id --rpc-url <rpc_url>
# expect: 918453

cast call <gateway_addr> "hasRole(bytes32,address)(bool)" \
  "$(cast keccak 'ADMIN_ROLE')" <admin_addr> --rpc-url <rpc_url>
# expect: true
```

**2. Functional smoke test — vault has real assets.**

```bash
cast call <vault_addr> "totalAssets()(uint256)" --rpc-url <rpc_url>
# expect: > 0 (this run: 7401002615, i.e. ~7,401 USDC — the 1 USDC seed
# deposit plus four seeded demo depositors)
```

For a from-scratch deposit/withdrawal round trip beyond the harness's own
demo seeding, use the dapp (step 3) rather than raw `cast send` — the
gateway's `AGENT_ROLE` path requires a signed agent policy, which the dapp's
flow constructs for you.

**3. Dapp integration.**

- Open `<dapp_url>` in a browser.
- Add a custom network to your wallet: RPC `<rpc_url>`, chain id `918453`.
- Connect. Confirm the dapp shows the vault at `<vault_addr>` with a
  non-zero balance matching step 2's `totalAssets()`.
- Confirm the three adapters (Aave, Compound, Morpho) show as registered
  with non-zero allocations — this is the concrete, browser-visible proof
  of the finding in ADR-0013: all three resolve correctly here, unlike on
  Base Sepolia.

**4. Explorer.**

- Open `<explorer_api_url>` — expect a live JSON API (a bare `curl` to `/`
  may 404; that's a missing root route, not a down service — check a real
  route such as its documented health or address-activity endpoint).
- Confirm recent transactions from the deploy ceremony and demo seeding are
  indexed.

## 4.5 — Fix loop

**Explorer-api or explorer-indexer only (step 4 failed, steps 1–3 passed).**
Since issue #1354 these two ship as separate images, so rebuild and recreate
just the one you fixed. Do **not** rebuild the chain: it, the deployed
contracts, and the indexed Postgres data all stay up.

```bash
cd testing/ethereum-testnet/config
docker compose -f docker-compose.dapp.yaml up -d --build explorer-api
```

Export the same values the bring-up used first — contract addresses from
`deployments/devnet.json`, and `EXPLORER_API_PORT` / `DAPP_PORT` /
`POSTGRES_PORT` matching the URLs the harness printed in §4.1 — because
compose re-evaluates the file's required substitutions on every invocation.
Re-run step 4 once the container reports healthy. Full command set and the
`restart` caveat: [`docs/development/environments.md` §3 — Per-service restart
and rebuild](../development/environments.md#per-service-restart-and-rebuild).

**Any other failure** (chain, deploy ceremony, contracts, dapp bundle): fix on
`dev`, `git checkout` the fixed commit in this worktree, and restart from
§4.1. There is nothing to clean up first — the next
`cargo run -p smoke-test -- --full-stack` tears down and rebuilds the compose
stacks itself.

## 4.6 — Teardown (not a rollback — routine)

```bash
# In the terminal running smoke-test:
<Ctrl-C>
```

`Fixture`'s `Drop` impl tears down both compose stacks on any exit path
(clean Ctrl-C, panic, or process kill). No manual `docker compose down` is
required, though `docker compose -p ethereum-testnet down` is safe to run
if a prior run was killed uncleanly (e.g. `kill -9`, which bypasses `Drop`).

## 4.7 — Report

This runbook itself **is** the report for this pass: preflight passed
(`forge build` clean at `755e9b28`), cutover completed (endpoint summary
above), postflight passed (role check `true`, `totalAssets()` non-zero, dapp
reachable `HTTP 200`, explorer-api reachable). No version tag is cut — per
`docs/operations/contract-release-runbooks.md` §2, a Devnet verification
pass with no lasting deployment record documents readiness, it does not by
itself consume a `vA.B.C` mainnet/testnet slot.

## Related

- [`docs/operations/contract-release-runbooks.md`](../operations/contract-release-runbooks.md) — the policy this runbook implements.
- [ADR-0013](../adr/ADR-0013-twin-devnet-over-base-sepolia-for-testnet-verification.md) — why the Devnet, not Base Sepolia, is the default target.
- [`docs/technical/full-stack-devnet.md`](../technical/full-stack-devnet.md) — the Devnet mechanism this runbook drives.
- [`docs/operations/base-sepolia-deployment.md`](../operations/base-sepolia-deployment.md) — the sibling runbook for a real network, same ceremony order.
