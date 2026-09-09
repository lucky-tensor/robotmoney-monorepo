//! Suite-08 — multi-vault schema migration tests (issue #315).
//!
//! Acceptance criteria exercised here:
//!
//! AC-2: vault_snapshots rows for the second registered vault are indexed
//!       independently (two registered vaults, each gets its own snapshot).
//!
//! AC-3: router_weight_snapshots populated on WeightsApplied events.
//!
//! AC-4: governance_proposals and governance_votes populated on
//!       ProposalCreated and VoteCast events.
//!
//! All tests skip cleanly when Docker is not available.
//!
//! Canonical: docs/technical/explorer-schema-decisions.md §3.4 / §3.5
//!            Plan tracking issue #109 §"Phase: Multi-vault explorer"
//!            docs/technical/governance-decisions.md §3.5

mod common;

use alloy_primitives::{Address, U256};
use common::{pg_fixture, StubRpcServer};
use explorer_indexer::{
    abi::{IRouterGovernanceEvents, IVaultRegistryEvents},
    db::CountTable,
    indexer::{run_once, IndexerConfig},
    rpc::JsonRpc,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn stub_block(number: u64, hash_byte: u8, parent_byte: u8) -> serde_json::Value {
    serde_json::json!({
        "number":     format!("0x{:x}", number),
        "hash":       format!("0x{}", hex::encode([hash_byte; 32])),
        "parentHash": format!("0x{}", hex::encode([parent_byte; 32])),
        "timestamp":  "0x65000000",
        "transactions": []
    })
}

/// Encode a `VaultRegistered` log for the stub server.
/// New signature (VaultRegistry.sol:67): `(address indexed vault, string name, address indexed asset)`.
fn encode_vault_registered_log(
    registry_addr: Address,
    vault_addr: Address,
    name: &str,
    asset_addr: Address,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IVaultRegistryEvents::VaultRegistered {
        vault: vault_addr,
        name: name.to_string(),
        asset: asset_addr,
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", registry_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xabu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

/// Encode a `WeightsApplied` log from RouterGovernance.
/// New signature (RouterGovernance.sol:132):
/// `(uint256 indexed proposalId, address[] vaults, uint256[] bps)`.
fn encode_weights_set_log(
    router_addr: Address,
    vaults: &[Address],
    bps: &[U256],
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IRouterGovernanceEvents::WeightsApplied {
        proposalId: U256::from(1u64),
        vaults: vaults.to_vec(),
        bps: bps.to_vec(),
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", router_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xbcu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

/// Encode a `ProposalCreated` log from RouterGovernance.
/// New signature (RouterGovernance.sol:106):
/// `(uint256 indexed proposalId, address indexed proposer, address[] vaults, uint256[] bps, uint64 votingDeadline)`.
#[allow(clippy::too_many_arguments)]
fn encode_proposal_created_log(
    gov_addr: Address,
    proposal_id: U256,
    proposer: Address,
    _description: &str,
    deadline_block: U256,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IRouterGovernanceEvents::ProposalCreated {
        proposalId: proposal_id,
        proposer,
        vaults: vec![],
        bps: vec![],
        votingDeadline: deadline_block.try_into().unwrap_or(u64::MAX),
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", gov_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xcdu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

/// Encode a `VoteCast` log from RouterGovernance.
/// New signature (RouterGovernance.sol:119):
/// `(uint256 indexed proposalId, address indexed voter, uint256 power, uint256 totalFor)`.
#[allow(clippy::too_many_arguments)]
fn encode_vote_cast_log(
    gov_addr: Address,
    proposal_id: U256,
    voter: Address,
    _support: bool,
    weight: U256,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IRouterGovernanceEvents::VoteCast {
        proposalId: proposal_id,
        voter,
        power: weight,
        totalFor: weight, // simplified: totalFor = power for single-voter tests
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", gov_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xdeu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC-2: Two registered vaults are snapshotted independently in one tick.
///
/// Sequence:
///   Tick 1 — register vault A and vault B via VaultRegistered events.
///             Both vaults must appear in the vaults table.
///   Tick 2 — no new events. Heartbeat fires for both vaults because no
///             prior snapshot exists. Each vault must have exactly one
///             vault_snapshots row after the tick.
///
/// The stub server returns identical (zero) state-read responses for
/// every eth_call so that the test does not need real vault state.
#[tokio::test]
async fn two_registered_vaults_indexed_independently() {
    let fx = pg_fixture().await;

    let registry_addr = Address::from([0xEEu8; 20]);
    let vault_a = Address::from([0xAAu8; 20]);
    let vault_b = Address::from([0xBBu8; 20]);
    let asset_addr = Address::from([0xDDu8; 20]);

    // Tick 1: register both vaults.
    let reg_a = encode_vault_registered_log(
        registry_addr,
        vault_a,
        "Vault A",
        asset_addr,
        50u64,
        [0x11u8; 32],
        0,
    );
    let reg_b = encode_vault_registered_log(
        registry_addr,
        vault_b,
        "Vault B",
        asset_addr,
        50u64,
        [0x11u8; 32],
        1,
    );

    let stub1 = StubRpcServer::start().await;
    stub1.set(
        "eth_blockNumber",
        serde_json::Value::String("0x46".into()), // 70
    );
    stub1.set("eth_getLogs", serde_json::Value::Array(vec![reg_a, reg_b]));
    // Zero-state eth_call response (32 zero bytes).
    stub1.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub1.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc1 = JsonRpc::new(&stub1.url);
    let cfg = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xCCu8; 20]),
        vault: vault_a,
        registry: Some(registry_addr),
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 4,
    };

    let o1 = run_once(&fx.db, &rpc1, &cfg).await.unwrap();
    assert!(o1.error.is_none(), "tick 1 must succeed: {:?}", o1.error);
    stub1.shutdown();

    // Both vaults must be registered.
    let vault_count = fx.db.count(CountTable::Vaults).await.unwrap();
    assert_eq!(vault_count, 2, "both vaults must be in the vaults table");

    // Tick 2: no events, heartbeat fires for both vaults because no
    // prior snapshot exists.
    let stub2 = StubRpcServer::start().await;
    stub2.set(
        "eth_blockNumber",
        serde_json::Value::String("0x5e".into()), // 94
    );
    stub2.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    stub2.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub2.set("eth_getBlockByNumber", stub_block(89, 0xbc, 0xab));

    let rpc2 = JsonRpc::new(&stub2.url);
    let cfg2 = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xCCu8; 20]),
        vault: vault_a,
        registry: Some(registry_addr),
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(89),
        feature_flags: 4,
    };

    let o2 = run_once(&fx.db, &rpc2, &cfg2).await.unwrap();
    assert!(o2.error.is_none(), "tick 2 must succeed: {:?}", o2.error);
    stub2.shutdown();

    // Each vault must have its own vault_snapshots row.
    let snaps_a: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT FROM vault_snapshots WHERE chain_id = $1 AND contract = $2",
    )
    .bind(8453i64)
    .bind(&vault_a.into_array()[..])
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    assert!(snaps_a.0 >= 1, "vault A must have at least one snapshot");

    let snaps_b: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT FROM vault_snapshots WHERE chain_id = $1 AND contract = $2",
    )
    .bind(8453i64)
    .bind(&vault_b.into_array()[..])
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    assert!(snaps_b.0 >= 1, "vault B must have at least one snapshot");
}

/// AC-3: WeightsApplied event from RouterGovernance populates
/// router_weight_snapshots with one row per vault leg.
#[tokio::test]
async fn weights_set_event_populates_router_weight_snapshots() {
    let fx = pg_fixture().await;

    let router_addr = Address::from([0xFFu8; 20]);
    let vault_a = Address::from([0xAAu8; 20]);
    let vault_b = Address::from([0xBBu8; 20]);

    // WeightsSet: vaultA = 6000 bps, vaultB = 4000 bps.
    let weights_log = encode_weights_set_log(
        router_addr,
        &[vault_a, vault_b],
        &[U256::from(6000u64), U256::from(4000u64)],
        50u64,
        [0x22u8; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    stub.set(
        "eth_blockNumber",
        serde_json::Value::String("0x46".into()), // 70
    );
    stub.set("eth_getLogs", serde_json::Value::Array(vec![weights_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xCCu8; 20]),
        vault: Address::from([0xDDu8; 20]),
        registry: None,
        router_governance: Some(router_addr),
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 4,
    };

    let o = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(o.error.is_none(), "tick must succeed: {:?}", o.error);
    stub.shutdown();

    // One row per WeightsSet event, containing one vault/weight entry per leg.
    let count = fx
        .db
        .count(CountTable::RouterWeightSnapshots)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "router_weight_snapshots must have one row per WeightsSet event"
    );

    let row: (Vec<Vec<u8>>, Vec<i64>) = sqlx::query_as(
        "SELECT vault_addresses, bps_values FROM router_weight_snapshots \
         WHERE chain_id = $1 AND router_address = $2",
    )
    .bind(8453i64)
    .bind(&router_addr.into_array()[..])
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    assert_eq!(
        row.0,
        vec![vault_a.as_slice().to_vec(), vault_b.as_slice().to_vec()]
    );
    assert_eq!(row.1, vec![6000i64, 4000i64]);
}

/// AC-4a: ProposalCreated event populates governance_proposals.
#[tokio::test]
async fn proposal_created_event_populates_governance_proposals() {
    let fx = pg_fixture().await;

    let gov_addr = Address::from([0x99u8; 20]);
    let proposer = Address::from([0x11u8; 20]);
    let _vault_a = Address::from([0xAAu8; 20]);

    let proposal_log = encode_proposal_created_log(
        gov_addr,
        U256::from(1u64),
        proposer,
        "vault_a=10000bps",
        U256::from(48u64), // deadlineBlock
        50u64,
        [0x33u8; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    stub.set(
        "eth_blockNumber",
        serde_json::Value::String("0x46".into()), // 70
    );
    stub.set("eth_getLogs", serde_json::Value::Array(vec![proposal_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xCCu8; 20]),
        vault: Address::from([0xDDu8; 20]),
        registry: None,
        router_governance: Some(gov_addr),
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 4,
    };

    let o = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(o.error.is_none(), "tick must succeed: {:?}", o.error);
    stub.shutdown();

    let count = fx.db.count(CountTable::GovernanceProposals).await.unwrap();
    assert_eq!(
        count, 1,
        "governance_proposals must have one row after ProposalCreated"
    );
}

/// AC-4b: VoteCast event populates governance_votes.
///
/// A ProposalCreated row must exist before VoteCast because of the FK
/// constraint. We insert the proposal row directly, then fire a VoteCast
/// tick to verify the vote row is created.
#[tokio::test]
async fn vote_cast_event_populates_governance_votes() {
    let fx = pg_fixture().await;

    let gov_addr = Address::from([0x99u8; 20]);
    let voter = Address::from([0x44u8; 20]);

    // Insert prerequisite chain + contract rows and a governance_proposals
    // row so the FK in governance_votes is satisfied.
    fx.db.upsert_chain(8453, "base", "stub").await.unwrap();
    fx.db
        .upsert_contract(8453, gov_addr.into_array(), "router_governance", None)
        .await
        .unwrap();
    fx.db
        .insert_proposal(
            8453,
            1i64,
            40i64,
            0i32,
            [0x11u8; 32],
            gov_addr.into_array(),
            "seed proposal",
            1_748_000_000i64,
            100i64,
        )
        .await
        .unwrap();

    let vote_log = encode_vote_cast_log(
        gov_addr,
        U256::from(1u64),
        voter,
        true,
        U256::from(500_000u64),
        55u64,
        [0x44u8; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    stub.set(
        "eth_blockNumber",
        serde_json::Value::String("0x46".into()), // 70
    );
    stub.set("eth_getLogs", serde_json::Value::Array(vec![vote_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xCCu8; 20]),
        vault: Address::from([0xDDu8; 20]),
        registry: None,
        router_governance: Some(gov_addr),
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 4,
    };

    let o = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(o.error.is_none(), "tick must succeed: {:?}", o.error);
    stub.shutdown();

    let count = fx.db.count(CountTable::GovernanceVotes).await.unwrap();
    assert_eq!(
        count, 1,
        "governance_votes must have one row after VoteCast"
    );

    // Verify voter address.
    let row: (Vec<u8>,) = sqlx::query_as("SELECT voter FROM governance_votes WHERE chain_id = $1")
        .bind(8453i64)
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
    assert_eq!(row.0, voter.as_slice(), "voter address must match");
}
