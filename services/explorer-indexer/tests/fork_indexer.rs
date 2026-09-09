//! Full-stack integration test: boot Postgres in a container, connect to a
//! shared Geth devnet via `RMPC_TESTNET_RPC_URL` (or fall back to the
//! `rmpc-fork-e2e` anvil harness when the env var is unset), point the
//! indexer at both, run a bounded range, and assert the contract of issue #57:
//!
//! - `indexer_runs` records a successful run.
//! - All 9 minimum tables are reachable by COUNT(*) (i.e. every
//!   migration applied cleanly under load).
//! - At least one `vault_snapshots` row is produced (heartbeat or
//!   event-driven; the live vault on Base always has totalAssets
//!   readable, so the snapshot succeeds).
//! - Re-running the same range produces 0 net inserts (idempotency).
//!
//! Preferred: set `RMPC_TESTNET_RPC_URL=http://localhost:8545` (shared Geth
//! devnet). Falls back to anvil via `RMPC_FORK_RPC_URL` or the checked-in
//! fixture. Skips when neither is available.

mod common;

use alloy_primitives::Address;
use common::pg_fixture;
use explorer_indexer::{db::CountTable, indexer::run_once, indexer::IndexerConfig, rpc::JsonRpc};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn populates_nine_tables_and_reindex_is_idempotent() {
    let has_testnet = std::env::var("RMPC_TESTNET_RPC_URL")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let has_fork = std::env::var("RMPC_FORK_RPC_URL")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !has_testnet && !has_fork {
        panic!(
            "[explorer-indexer-tests] RPC endpoint REQUIRED here but unavailable. \
             Set RMPC_TESTNET_RPC_URL or RMPC_FORK_RPC_URL to run against the shared Geth \
             devnet or an Anvil fork."
        );
    }
    let fx = pg_fixture().await;

    // Boot fork-anvil on a blocking thread (the harness uses
    // blocking reqwest + std::process). We hold the fixture for the
    // duration of the test.
    let fork = tokio::task::spawn_blocking(rmpc_fork_e2e::ForkFixture::new)
        .await
        .unwrap()
        .expect("ForkFixture boots");
    let rpc_url = fork.rpc_url.clone();

    let rpc = JsonRpc::new(&rpc_url);
    // The vault is real on Base mainnet (present in the devnet genesis alloc via
    // genesis-alloc.json); the gateway hasn't deployed yet so we use a zero-address
    // placeholder. eth_getLogs against an empty-event address is allowed.
    // On the shared Geth devnet the chain_id is 918453 (the local testnet network
    // id), but we record it as BASE_CHAIN_ID in the DB since the contract addresses
    // are the same as Base mainnet — the chain_id field is metadata only.
    let cfg = IndexerConfig {
        chain_id: rmpc_fork_e2e::BASE_CHAIN_ID as i64,
        chain_name: "base".into(),
        rpc_label: fork.rpc_label.clone(),
        gateway: Address::ZERO,
        vault: rmpc_fork_e2e::addresses::VAULT,
        registry: None,
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        // Cap the run at the safe head so the heartbeat snapshot lands at a known
        // block. Use saturating_sub so a fresh devnet (pin.block < CONFIRMATIONS)
        // doesn't panic — the indexer processes the genesis range and the heartbeat
        // fires because there is no previous vault snapshot in the DB.
        end_block: Some(
            fork.pin
                .block
                .saturating_sub(explorer_indexer::CONFIRMATIONS),
        ),
        feature_flags: 0,
    };

    // First run.
    let o1 = run_once(&fx.db, &rpc, &cfg).await.expect("run_once 1");
    assert!(o1.error.is_none(), "first run clean: {:?}", o1.error);
    assert!(
        o1.last_indexed_block.is_some(),
        "first run advances last_indexed_block"
    );

    // All nine tables addressable.
    for t in [
        CountTable::Chains,
        CountTable::Contracts,
        CountTable::Blocks,
        CountTable::Transactions,
        CountTable::AgentDeposits,
        CountTable::AgentPolicies,
        CountTable::VaultSnapshots,
        CountTable::WalletPositions,
        CountTable::IndexerRuns,
    ] {
        let _ = fx.db.count(t).await.unwrap_or_else(|e| panic!("{e}"));
    }
    // Heartbeat snapshot must have landed at least once.
    assert!(
        fx.db.count(CountTable::VaultSnapshots).await.unwrap() >= 1,
        "at least one vault_snapshots row from heartbeat"
    );
    // Bookkeeping rows present.
    assert_eq!(fx.db.count(CountTable::Chains).await.unwrap(), 1);
    assert_eq!(fx.db.count(CountTable::Contracts).await.unwrap(), 2);
    assert!(fx.db.count(CountTable::IndexerRuns).await.unwrap() >= 1);

    // Second run — re-entering the same `last_indexed_block` produces
    // no net inserts beyond a fresh `indexer_runs` audit row.
    let snap_before = fx.db.count(CountTable::VaultSnapshots).await.unwrap();
    let dep_before = fx.db.count(CountTable::AgentDeposits).await.unwrap();
    let pol_before = fx.db.count(CountTable::AgentPolicies).await.unwrap();
    let blk_before = fx.db.count(CountTable::Blocks).await.unwrap();
    let tx_before = fx.db.count(CountTable::Transactions).await.unwrap();

    let o2 = run_once(&fx.db, &rpc, &cfg).await.expect("run_once 2");
    assert!(o2.error.is_none());

    assert_eq!(
        fx.db.count(CountTable::VaultSnapshots).await.unwrap(),
        snap_before
    );
    assert_eq!(
        fx.db.count(CountTable::AgentDeposits).await.unwrap(),
        dep_before
    );
    assert_eq!(
        fx.db.count(CountTable::AgentPolicies).await.unwrap(),
        pol_before
    );
    assert_eq!(fx.db.count(CountTable::Blocks).await.unwrap(), blk_before);
    assert_eq!(
        fx.db.count(CountTable::Transactions).await.unwrap(),
        tx_before
    );

    // Tear down the anvil child.
    drop(fork);
}
