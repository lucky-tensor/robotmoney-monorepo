//! §11 acceptance criterion: indexer_runs surfaces failures with
//! actionable messages. We point the indexer at a stub RPC server,
//! flip its forced-failure switch, run a tick, and confirm the
//! `indexer_runs` row contains an `error` and a NULL `to_block`. We
//! then disable the failure, run a normal tick, and confirm
//! `last_indexed_block` advances (resume works).

mod common;

use alloy_primitives::Address;
use common::{pg_fixture, StubRpcServer};
use explorer_indexer::{indexer::run_once, indexer::IndexerConfig, rpc::JsonRpc};

#[tokio::test]
async fn rpc_failure_recorded_in_indexer_runs() {
    let fx = pg_fixture().await;

    let stub = StubRpcServer::start().await;
    // chain_id is queried in some downstream paths but run_once only
    // touches block_number / get_logs / get_block / eth_call. Set
    // canned responses for the happy path; we'll flip force_failure
    // before the first tick to confirm the error path.
    stub.set("eth_chainId", serde_json::Value::String("0x2105".into()));
    stub.set("eth_blockNumber", serde_json::Value::String("0x64".into()));
    stub.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    // 32-byte zero response decodes as U256::ZERO / false for the
    // vault state-snapshot reads (totalAssets, totalSupply, etc.).
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );

    let rpc = JsonRpc::new(&stub.url);
    let cfg = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xaau8; 20]),
        vault: Address::from([0xbbu8; 20]),
        registry: None,
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 100,
        end_block: None,
        feature_flags: 0,
    };

    // Forced failure → run_once must catch the RPC error and write
    // an `indexer_runs` row whose `error` column is non-NULL.
    stub.force_failure(true);
    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(outcome.error.is_some(), "outcome carries error string");
    let latest = fx.db.latest_run(8453).await.unwrap().unwrap();
    assert!(latest.error.is_some(), "indexer_runs.error is non-null");
    assert_eq!(latest.to_block, None, "to_block stays NULL on failure");

    // Resume: lift the failure, tick again, confirm the run records a
    // success and last_indexed_block advances to the safe head.
    stub.force_failure(false);
    let resume = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(resume.error.is_none(), "resume run is clean");
    let latest = fx.db.latest_run(8453).await.unwrap().unwrap();
    assert!(latest.error.is_none(), "successful run has no error");
    // tip 0x64 = 100, CONFIRMATIONS = 5 → safe head = 95.
    assert_eq!(
        latest.last_indexed_block,
        Some(95),
        "last_indexed_block reaches safe head after resume"
    );

    stub.shutdown();
}
