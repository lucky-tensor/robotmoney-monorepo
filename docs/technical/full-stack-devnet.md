# Devnet runbook

> **Canonical:** `Plan tracking issue #109` §2 (Phase 1 gateway + vault).

The devnet is a local Geth + Lighthouse proof-of-stake chain managed by
the `testing/smoke-test` crate. All integration tests that need a live
chain boot through `Fixture::new()`, which starts the compose stack,
deploys contracts, and tears down on drop.

## Starting the devnet

```bash
# Boot Geth + Lighthouse and deploy gateway/vault contracts.
# Stays running until you Ctrl-C.
cargo run -p smoke-test
```

Or from a Rust test:

```rust
let fixture = smoke_test::Fixture::new()?;
// fixture tears down when dropped
```

## Prerequisites

- `docker` on PATH (for `docker compose`)
- `forge` and `cast` on PATH (Foundry)

`smoke_test::prerequisites_available()` checks all three and returns
`false` if any is missing.

## Compose stack

The compose file is `testing/ethereum-testnet/config/docker-compose.yaml`.
It defines:

- `geth` — execution layer (chain-id 918453), genesis seeded from a
  pinned Base mainnet block (`alloc` populated from a Base state snapshot,
  not empty). Real Base contracts (USDC, WETH, …) are present at their
  canonical addresses from block 0 of the devnet.
- `lighthouse` — consensus layer (12-second blocks)
- `setup` — one-shot service that (a) patches token balance storage
  in genesis to grant a clean-history harness EOA a large balance of
  each test-relevant token (USDC at minimum), and (b) deploys Robot
  Money contracts via `forge script`. See
  `docs/development/smoke-test-design.md` for the genesis-time balance
  grant faucet design and the rationale for not impersonating a real
  Base whale.

## Fork-state fixture

`testing/fork-e2e-rust` loads a separate checked-in Anvil fork-state fixture
(`testing/fixtures/fork-state/CURRENT.anvil-state`) via `anvil --load-state`,
requiring no live RPC at test time. For the fixture's purpose, the
`RMPC_FORK_RPC_URL` regeneration variable, and the developer-owned-on-change
refresh command (`scripts/devnet/snapshot-fork.sh`), see
`docs/development/environments.md` §2 ("Fork e2e") and ADR-0011.

### Pin age (issue #1386)

The devnet's chain clock is wall-clock `now` — `generate.sh` falls back to
`date +%s`, and the smoke-test harness sets `GENESIS_TIMESTAMP` to now + 15s.
The Aave V3 / Compound V3 / Morpho state the three adapters call is frozen at
the pinned Base block. Those protocols accrue interest as a function of
`block.timestamp - lastUpdateTimestamp`, so the *simulated* interval between
the snapshot and the devnet's present grows by one day per day the pin is not
refreshed. The fixture was historically refreshed every one to four weeks; in
2026 it went 48 days with nothing in CI reporting the fact.

`scripts/devnet/check-fork-pin-age.sh` makes the age visible:

- `scripts/devnet/check-fork-manifest.sh` calls it on every run, so the age is
  printed on the pull-request path and annotated as a `::warning::` once the
  pin passes the 21-day cadence. It never fails there — a stale pin is a
  maintenance signal, not a reason to red the merge queue.
- The nightly `live-base-fork-drift` job calls it with `--max-age-days 30`,
  where a hard failure is affordable and creates real pressure to refresh.
- `scripts/devnet/check-fork-pin-age-selftest.sh` drives every branch of the
  gate offline; `suite-01-02-forge-tests.yml` runs it before the real fixture
  is judged.

Measured, deliberately, from `CURRENT.json`'s `captured_at` rather than the
block's own timestamp: `snapshot-fork.sh` advances the fork clock to wall-clock
now *before* warming the adapters, so the protocol `lastUpdateTimestamp` values
baked into the fixture are the capture wall-clock, not the fork block's
timestamp.

That last point also rules out "set `GENESIS_TIMESTAMP` to the forked block's
timestamp" as a way to hold the delta at zero: the fixture's protocol
timestamps are *later* than the fork block's, so booting the devnet at the fork
block's timestamp makes `block.timestamp - lastUpdateTimestamp` underflow and
reverts every adapter call — the same failure `snapshot-fork.sh` step "3-pre"
already documents and works around. Anchoring genesis to `captured_at` instead
avoids the underflow but puts the beacon genesis in the past by the pin's full
age, which Lighthouse would have to traverse as empty slots before producing a
block. Refreshing the pin is the supported way to keep the delta small.

### Refreshing the pin

```bash
RMPC_FORK_RPC_URL=<Base archive RPC> scripts/devnet/snapshot-fork.sh
```

then realign `testing/ethereum-testnet/config/fork-block.json`
(`block_number`, `block_hash`), regenerate
`testing/fixtures/fork-state/genesis-alloc.json` with
`smoke-test-genesis-ingester`, and recapture
`testing/ethereum-testnet/config/expected-prices.json`.

`RMPC_FORK_RPC_URL` is not optional in practice. The script's default,
`https://base-rpc.publicnode.com`, is a pruned node: it serves state for only
about 128 blocks (~4 minutes on Base) and answers anything older with
"Archive requests require a personal token". A capture session runs far longer
than that against a fixed pinned block, so the default endpoint cannot finish
one. Several public Base endpoints do serve archive state — `mainnet.base.org`
and `base-mainnet.public.blastapi.io` were both verified to fork a 48-day-old
block under Anvil — so a refresh does not strictly require a keyed provider,
though issue #1239 remains the right fix for CI.

## Troubleshooting

- **Port 8545 already in use.** Another devnet instance is running.
  Stop it with `docker compose -f testing/ethereum-testnet/config/docker-compose.yaml down`.
- **`forge` or `cast` not found.** Install Foundry:
  `curl -L https://foundry.paradigm.xyz | bash && foundryup`.
- **`anvil --load-state` parse error.** The fixture is stale or was
  written by a different Anvil version. Regenerate with
  `bash scripts/devnet/snapshot-fork.sh`.
