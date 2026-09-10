# Robot Money — Environment Modes

This document is the single operator reference for every environment mode used
in development, testing, and production. It covers:

- [1. Local devnet (Geth + Lighthouse)](#1-local-devnet-geth--lighthouse)
- [2. Fork e2e (Anvil fork of Base mainnet)](#2-fork-e2e-anvil-fork-of-base-mainnet)
- [3. Full-stack staging (devnet + dapp + indexer)](#3-full-stack-staging-devnet--dapp--indexer)
- [4. Mainnet read-only (Base mainnet)](#4-mainnet-read-only-base-mainnet)

Each section lists: required env vars, startup command, contract address source,
data persistence behaviour, and teardown command.

Canonical: Plan tracking issue #109 (formerly `Plan tracking issue #109`). Related design docs:
`docs/technical/full-stack-devnet.md`, `docs/development/smoke-test-design.md`,
`docs/development/testing-strategy-ethereum.md` (both test stacks — PoS devnet
and the forked Base harness). The principle these modes embody —
one production codebase, environments differing only by configuration and
seeded data — is `docs/development/single-production-codebase.md`.

---

## 1. Local devnet (Geth + Lighthouse)

A full Proof-of-Stake chain running locally in Docker. Chain id **918453**.
The genesis is seeded from a pinned Base mainnet state snapshot so canonical
Base contracts (USDC at `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`) are
present from block 0. Robot Money contracts are deployed fresh each time the
devnet boots.

### Services

| Service | Role |
|---------|------|
| `geth` | Execution layer — HTTP RPC on port 8545, WS on 8546 |
| `lighthouse` | Consensus layer — 12-second block time |
| `setup` | One-shot genesis builder and contract deployer |

Compose file: `testing/ethereum-testnet/config/docker-compose.yaml`

### Required env vars

None are required for a basic devnet boot. The following vars override defaults:

| Var | Default | Meaning |
|-----|---------|---------|
| `GETH_RPC_PORT` | `8545` | Host port for Geth HTTP RPC |
| `GETH_WS_PORT` | `8546` | Host port for Geth WebSocket |
| `GETH_AUTHRPC_PORT` | `8551` | Host port for Geth Engine API |
| `GENESIS_TIMESTAMP` | _(auto)_ | Override genesis block timestamp |
| `SMOKE_GENESIS_ALLOC_FILE` | _(unset)_ | Absolute host path to the genesis alloc JSON produced by `smoke-test-genesis-ingester`; required only when including `docker-compose.alloc.yaml` |

### Startup command

```bash
# Boot chain and deploy contracts — stays running until Ctrl-C.
cargo run -p smoke-test
```

Alternatively, bring up just the compose stack:

```bash
cd testing/ethereum-testnet/config
docker compose up -d
```

Then deploy contracts via the smoke-test binary:

```bash
cargo run -p smoke-test
```

### Contract address source

`deployments/devnet.json` — written by `Deploy.s.sol` at deploy time.
Read the addresses out with:

```bash
python3 -c "import json; d=json.load(open('deployments/devnet.json')); print(d['gateway'], d['vault'])"
```

### Data persistence

Chain state is stored in a Docker named volume (`testnet-data`). The volume
persists across `docker compose stop` / `docker compose start` cycles.
Contract addresses change on every fresh boot because `testnet-data` is
wiped by `docker compose down -v`.

### Teardown command

```bash
cd testing/ethereum-testnet/config
docker compose down -v   # -v removes the named volume (clean slate)
```

### CI suites that exercise this environment

| Suite | Workflow file |
|-------|---------------|
| Suite 7 — rmpc integration (Geth + Lighthouse) | `.github/workflows/suite-07-rmpc-integration.yml` |
| Suite 8 — explorer indexer | `.github/workflows/suite-08-explorer-indexer.yml` |
| Suite 10 — dapp E2E (Playwright) | `.github/workflows/suite-10-dapp-e2e.yml` |
| Suite 11b — OpenCode headless | `.github/workflows/suite-11b-opencode-headless.yml` |
| Suite 12 — OpenClaw | `.github/workflows/suite-12-openclaw.yml` |
| Suite 14 — smoke-test fixture | `.github/workflows/suite-14-smoke-test.yml` |

---

## 2. Fork e2e (Anvil fork of Base mainnet)

An Anvil instance forked from a pinned Base mainnet block. Chain id **8453**.
Real deployed contracts (USDC, Robot Money vault, adapters) are present at
their canonical addresses. Used to verify ABI encoding, adapter call paths,
and error handling against actual on-chain state without a live RPC at
test runtime.

### Services

Anvil only — no Docker required. The fork-state fixture
(`testing/fixtures/fork-state/CURRENT.anvil-state`) is loaded via
`anvil --load-state` so no upstream RPC is contacted during tests.

### Required env vars

The fork-state fixture (used in CI) needs no env vars. The optional
live-RPC path and manual fixture refresh use:

| Var | Required | Meaning |
|-----|----------|---------|
| `RMPC_FORK_RPC_URL` | No | Base mainnet endpoint. Used only for (a) **optional local** live-fork runs and (b) **fixture regeneration** (archive depth for a pinned historical block). It is **not** a CI secret and gates no merge (ADR-0011); merge-gating CI forks the checked-in fixture offline. The nightly live-drift alarm uses a free public default (`https://base-rpc.publicnode.com`), not a secret. |
| `RMPC_FORK_BLOCK` | No | Decimal block number pin. CI sets this in the workflow file. Unset → `eth_blockNumber - 50` against the upstream RPC. |

### Startup command

```bash
# Run fork e2e tests against the checked-in fixture (no RPC needed).
cargo test --manifest-path testing/fork-e2e-rust/Cargo.toml

# Run against a live Base mainnet fork (requires an archive RPC).
RMPC_FORK_RPC_URL=https://base-mainnet.g.alchemy.com/v2/<key> \
  cargo test --manifest-path testing/fork-e2e-rust/Cargo.toml
```

For the read-only OpenCode walkthrough, boot Anvil directly:

```bash
anvil --fork-url "$RMPC_FORK_RPC_URL" --port 8545 --silent &
```

See `docs/development/opencode-readonly-fork.md` for the full walkthrough.

### Contract address source

Real Base mainnet deployed addresses (hardcoded in `testing/fork-e2e-rust/src/addresses.rs`):

| Contract | Address |
|----------|---------|
| RobotMoneyVault | `0x4f835c9f54bcf17daf9040f60cb72951ccbb49dd` |
| MorphoAdapter | `0xa6ed7b03bc82d7c6d4ac4feb971a06550a7817e9` |
| AaveV3Adapter | `0x218695bdab0fe4f8d0a8ee590bc6f35820fc0bea` |
| CompoundV3Adapter | `0x8247da22a59fce074c102431048d0ce7294c2652` |
| USDC | `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913` |

For the read-only walkthrough the gateway address is a placeholder
(`0x000000000000000000000000000000000000dEaD`) — reads return a partial
envelope; no writes are attempted.

### Data persistence

Anvil state is ephemeral. Each test boots a fresh Anvil child process and
tears it down when the test exits (no `evm_snapshot`/`evm_revert`
orchestration — fork-restart-per-test isolation per ADR §3.5).

### Teardown command

Anvil stops automatically when the test process exits. For a manually started
Anvil:

```bash
pkill -f 'anvil --fork-url'
```

### Refreshing the fork-state fixture

Refresh is **developer-owned on change**, not scheduled (ADR-0011): whoever
adds an adapter, wires a new pool, or changes an integration regenerates the
fixture in the **same PR**, and a developer refreshes in response to a nightly
drift-alarm issue. There is no monthly cadence.

```bash
# Requires RMPC_FORK_RPC_URL (archive depth for the pinned block).
# Run when an integration change requires a new fixture, or in response to a
# nightly drift-alarm issue — not on a schedule (ADR-0011).
bash scripts/devnet/snapshot-fork.sh
# Or via the convenience wrapper:
bash scripts/devnet/refresh-fork-fixture.sh
```

Validates the fixture manifest before use — including the sha256 integrity
check (`state_sha256` in `CURRENT.json`, ADR-0011 addendum, issue #1152):
`snapshot-fork.sh` writes the digest, so a legitimate refresh always updates
it; a hand-edited blob with a stale or missing digest fails loudly here
instead of loading silently:

```bash
bash scripts/devnet/check-fork-manifest.sh
```

### CI suites that exercise this environment

| Suite | Workflow file |
|-------|---------------|
| Suite 5 — fork protocol-adapter integration | `.github/workflows/suite-05-fork-integration.yml` |

---

## 3. Full-stack staging (devnet + dapp + indexer)

The local devnet (§1) plus Postgres, a one-shot explorer-migrate schema
step, explorer-indexer, explorer-api, and the dapp all running together in
Docker Compose. Used to validate the complete Robot Money service graph
end-to-end.

### Services

Everything in §1 plus:

| Service | Role |
|---------|------|
| `postgres` | Explorer persistence |
| `explorer-migrate` | One-shot `indexer --migrate-only`: applies the explorer schema, then exits. `explorer-indexer` and `explorer-api` both wait on it with `service_completed_successfully`, so a failing migration stops the stack instead of leaving either service running against a half-migrated database (issue #1359) |
| `explorer-indexer` | Chain event indexer. Does **not** migrate on boot — instead it **refuses to start** unless the highest migration version embedded in the binary equals the highest version applied in `_sqlx_migrations`, and the refusal names both versions (issue #1392). A container that exits with `schema version mismatch` needs its migrate step run (or a binary matching the database), not a restart |
| `explorer-api` | REST API serving indexed data |
| `dapp` | Built Vite bundle served by nginx |

Compose files:
- `testing/ethereum-testnet/config/docker-compose.yaml` (chain)
- `testing/ethereum-testnet/config/docker-compose.dapp.yaml` (dapp overlay)

### Required env vars

Dapp overlay (`docker-compose.dapp.yaml`) requires:

| Var | Default | Meaning |
|-----|---------|---------|
| `VITE_GATEWAY_ADDRESS` | _(none — required)_ | Deployed gateway contract address |
| `VITE_VAULT_ADDRESS` | _(none — required)_ | Deployed vault contract address |
| `INDEXER_GATEWAY` | _(none — required)_ | Same as `VITE_GATEWAY_ADDRESS` |
| `INDEXER_VAULT` | _(none — required)_ | Same as `VITE_VAULT_ADDRESS` |
| `VITE_EXPLORER_API_URL` | `http://localhost:8080` | Explorer API base URL |
| `INDEXER_RPC_URL` | `http://host.docker.internal:8545` | Geth RPC URL for indexer |
| `INDEXER_CHAIN_ID` | `918453` | Chain id |
| `INDEXER_CHAIN_NAME` | `devnet` | Chain name label |
| `EXPLORER_API_CHAIN_ID` | `918453` | Chain id for the explorer API |
| `POSTGRES_PORT` | `5432` | Postgres host port |
| `EXPLORER_API_PORT` | `8080` | Explorer API host port |
| `DAPP_PORT` | `5173` | Dapp host port |
| `POSTGRES_USER` | `robotmoney` | Postgres user |
| `POSTGRES_PASSWORD` | `robotmoney` | Postgres password |
| `POSTGRES_DB` | `explorer` | Postgres database name |
| `VITE_ENV_CLASS` | `fork` | One of: `fork` \| `devnet` \| `testnet` \| `mainnet`. Set to `devnet` for this mode. |
| `VITE_GATEWAY_EXPECTED_CODE_HASH` | _(empty)_ | Keccak-256 of deployed gateway bytecode. The dapp refuses admin writes until this matches. Set from `deployments/devnet.json` field `gateway_runtime_hash`. |
| `VITE_RM_TOKEN_ADDRESS` | `0x0000000000000000000000000000000000000000` | Deployed RmToken ERC-20 address (issue #466). The smoke-test threads `Fixture::rm_token_hex()` here so the FaucetTab's RM balance read and RM drip point at the real token. Mainnet builds leave this at the zero-address default to keep the RM drip surface inert. |
| `VITE_FAUCET_DRIP_ETH_WEI` | `10000000000000000` (0.01 ETH) | Native Base ETH gas drip amount for the Faucet tab's "Get Base ETH" button (issue #466). Documented for parity with `FAUCET_DRIP_AMOUNT_ETH` in `clients/dapp/src/lib/chainClassifier.ts`. |

Additional optional dapp vars are documented in `clients/dapp/.env.example`.

### Startup command

```bash
# One command: boots chain, deploys contracts, starts dapp + indexer.
# Prints rpc_url, explorer_api_url, dapp_url, gateway_addr on stdout.
cargo run -p smoke-test -- --full-stack
```

Manual bring-up (after obtaining contract addresses from §1):

```bash
export VITE_GATEWAY_ADDRESS=<gateway>
export VITE_VAULT_ADDRESS=<vault>
export INDEXER_GATEWAY=$VITE_GATEWAY_ADDRESS
export INDEXER_VAULT=$VITE_VAULT_ADDRESS

cd testing/ethereum-testnet/config
docker compose -f docker-compose.yaml -f docker-compose.dapp.yaml up --build
```

### Per-service restart and rebuild

`explorer-indexer` and `explorer-api` are built as two **separate images**
from two targets of `docker/rust-services.Dockerfile` (issue #1354), so either
can be rebuilt or restarted on its own. Iterating on one explorer service does
not require rebuilding the chain, re-running the genesis build, or
re-deploying contracts, and Postgres keeps its data.

The explorer services live in the dapp overlay's own compose project
(`robotmoney-dapp`); the chain is a separate project (`ethereum-testnet`) and
none of these commands touch it.

```bash
cd testing/ethereum-testnet/config

# Restart one service in place — no rebuild, same image.
docker compose -f docker-compose.dapp.yaml restart explorer-api

# Rebuild one service's image from source and recreate only that container.
docker compose -f docker-compose.dapp.yaml up -d --build explorer-api

# Follow one service's logs.
docker compose -f docker-compose.dapp.yaml logs -f explorer-indexer
```

Swap `explorer-indexer` for `explorer-api` to iterate on the indexer instead.
The `dapp`, `postgres`, and `receipt-fixtures` containers stay up throughout.

Every `docker compose` invocation — `restart` included — re-evaluates the
compose file's `${VAR:?...}` substitutions, so the same required env vars the
initial bring-up needed must still be exported. If the stack was launched by
`cargo run -p smoke-test -- --full-stack`, the harness also chose the host
ports; re-export `EXPLORER_API_PORT` / `DAPP_PORT` / `POSTGRES_PORT` to match
the URLs it printed, or compose republishes on the defaults in the table
above.

**`restart` does not re-evaluate `depends_on`.** `docker compose restart`
restarts an existing container in place; it does not recreate it and does not
re-check the `explorer-migrate: service_completed_successfully` barrier that
`up` uses to order the schema migration ahead of both readers. What still
fails safe on that path is explorer-api's own `/health` probe, which `SELECT`s
from `indexer_runs` (`clients/explorer-api/src/routes.rs`) and therefore
cannot report healthy while the explorer schema is missing. If a schema change
is part of what you are testing, use `up -d`, which evaluates the migrator
gate; `restart` will not run the migrator for you.

### Contract address source

Same as §1: `deployments/devnet.json`. The smoke-test binary reads this file
and passes the addresses as Docker build args automatically.

### Data persistence

Same as §1: Docker named volume (`testnet-data`) for chain state; Postgres
data in a second named volume. Both are wiped by `docker compose down -v`.

### Teardown command

```bash
cd testing/ethereum-testnet/config
docker compose -f docker-compose.yaml -f docker-compose.dapp.yaml down -v
```

When started via `cargo run -p smoke-test -- --full-stack`, send SIGINT
(Ctrl-C) — the binary's SIGINT handler runs `docker compose down`.

### CI suites that exercise this environment

| Suite | Workflow file |
|-------|---------------|
| Suite 10 — dapp E2E (Playwright, full-stack) | `.github/workflows/suite-10-dapp-e2e.yml` |
| Suite 14 — smoke-test `--full-stack` CLI meta | `.github/workflows/suite-14-smoke-test.yml` |

---

## 4. Mainnet read-only (Base mainnet)

Connects `rmpc` to a live Base mainnet RPC for read-only portfolio inspection.
No transactions are signed. The OpenClaw harness uses this mode behind a
`RMPC_ALLOW_MAINNET=yes` guard.

Chain id: **8453**.

### Services

No local services. Operator provides a Base mainnet RPC endpoint.

### Required env vars

| Var | Required | Meaning |
|-----|----------|---------|
| `RMPC_CONFIG` | Yes | Path to an `rmpc` TOML config pointing at the mainnet RPC. See `BOOTSTRAP.md` §3 Profile B template. |
| `RMPC_ALLOW_MAINNET` | Yes (for OpenClaw harness) | Must be the literal string `yes` to pass the mainnet refusal guard in `testing/openclaw-config/openclaw_harness.sh`. |
| `RMPC_KEYSTORE_PASSPHRASE` | Yes | Keystore decryption passphrase. Never echoed; passed via environment only. |
| `RMPC_STATE_DIR` | Yes | State directory path. `rmpc` exits silently with code 3 if unset. |

Config template (save as `./rmpc-mainnet.toml`):

```toml
chain_id             = 8453
rpc_url              = "https://mainnet.base.org"   # replace with your archive endpoint
gateway_address      = "0x0000000000000000000000000000000000000000"  # not deployed; reads return partial envelope
usdc_address         = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
vault_address        = "0x4f835c9f54bcf17daf9040f60cb72951ccbb49dd"
state_dir            = "./rmpc-state"

[signer]
allow_software_fallback = true
keystore_path           = "./keystore.json"
```

### Startup command

```bash
# Read-only vault inspection.
RMPC_KEYSTORE_PASSPHRASE="<passphrase>" \
  rmpc get-vault --config ./rmpc-mainnet.toml --pretty

# OpenClaw bounded monitor (read-only, requires RMPC_ALLOW_MAINNET guard).
RMPC_CONFIG=./rmpc-mainnet.toml \
RMPC_NETWORK=mainnet \
RMPC_ALLOW_MAINNET=yes \
RMPC_MONITOR_COMMAND=get-vault \
  bash testing/openclaw-config/openclaw_harness.sh
```

See `docs/development/opencode-readonly-fork.md` for the fork-based
read-only walkthrough (no mainnet RPC required).

### Contract address source

Fixed Base mainnet addresses (see §2 table). Updated only when Robot Money
deploys new contracts. Authoritative record: `docs/technical/smart-contracts.md` §2.

### Data persistence

No local chain state. `rmpc` persists signer state to `RMPC_STATE_DIR`
between runs.

### Teardown command

No chain to tear down. Kill the `rmpc` process or let it exit naturally.

### CI suites that exercise this environment

| Suite | Workflow file |
|-------|---------------|
| Suite 11a — OpenCode smoke (mainnet gate) | `.github/workflows/suite-11a-opencode-smoke.yml` |
| Suite 12 — OpenClaw | `.github/workflows/suite-12-openclaw.yml` |

---

## Quick-reference table

| Mode | Chain id | Startup command | Address source | Persistent state |
|------|----------|-----------------|----------------|-----------------|
| Local devnet | 918453 | `cargo run -p smoke-test` | `deployments/devnet.json` | Docker volume (wiped on `down -v`) |
| Fork e2e | 8453 | `cargo test --manifest-path testing/fork-e2e-rust/Cargo.toml` | `testing/fork-e2e-rust/src/addresses.rs` | None (ephemeral per test) |
| Full-stack staging | 918453 | `cargo run -p smoke-test -- --full-stack` | `deployments/devnet.json` | Docker volumes (wiped on `down -v`) |
| Mainnet read-only | 8453 | `rmpc get-vault --config ./rmpc-mainnet.toml --pretty` | `docs/technical/smart-contracts.md` §2 | `RMPC_STATE_DIR` on disk |
