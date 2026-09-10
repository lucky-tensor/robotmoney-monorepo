//! Issue #654 — account history multi-event indexing tests.
//!
//! Verifies that the indexer stores all five event types from
//! architecture §5.4 into `account_history_events`:
//!
//!   - AgentDeposit → kind="deposit"
//!   - ERC-4626 Withdraw → kind="withdrawal"
//!   - ExitFeeCharged → kind="fee_charged"
//!   - AgentAuthorized / AgentRevoked → kind="policy_change"
//!   - VoteCast → kind="governance_vote"
//!
//! Tests run against a real Postgres container (skipped if Docker
//! is unavailable) and use the stub RPC server from `common`.

mod common;

use alloy_primitives::{Address, U256};
use common::pg_fixture;
use explorer_indexer::{
    abi::{IGatewayEvents, IRouterGovernanceEvents, IVaultEvents},
    db::CountTable,
    indexer::{run_once, IndexerConfig},
    rpc::JsonRpc,
};

/// Helper: encode any SolEvent into the JSON shape the stub RPC expects.
fn encode_log<E: alloy_sol_types::SolEvent>(
    event: E,
    contract_addr: Address,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    let log_data = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));
    serde_json::json!({
        "address":          format!("{:#x}", contract_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xabu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

fn stub_block(number: u64) -> serde_json::Value {
    serde_json::json!({
        "number":     format!("0x{:x}", number),
        "hash":       format!("0x{}", hex::encode([0xabu8; 32])),
        "parentHash": format!("0x{}", hex::encode([0xaau8; 32])),
        "timestamp":  "0x65000000",
        "transactions": []
    })
}

/// Standard IndexerConfig for account_history tests.
fn history_cfg(gateway: Address, vault: Address, gov: Option<Address>) -> IndexerConfig {
    IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway,
        vault,
        registry: None,
        router_governance: gov,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(10),
        feature_flags: 0,
    }
}

/// AC-1: AgentDeposit event is stored as `kind="deposit"` in
/// account_history_events, keyed to the share_receiver address.
#[tokio::test]
async fn account_history_deposit_event_indexed() {
    let fx = pg_fixture().await;

    let gateway = Address::from([0x11u8; 20]);
    let vault = Address::from([0x22u8; 20]);
    let agent = Address::from([0x33u8; 20]);
    let share_receiver = Address::from([0x44u8; 20]);

    let deposit_log = encode_log(
        IGatewayEvents::AgentDeposit {
            paymentId: alloy_primitives::FixedBytes::from([0x55u8; 32]),
            orderId: alloy_primitives::FixedBytes::from([0x66u8; 32]),
            agent,
            shareReceiver: share_receiver,
            amount: U256::from(1_000_000u64),
            sharesMinted: U256::from(1_000_000u64),
            windowId: 1u64,
        },
        gateway,
        10u64,
        [0x77u8; 32],
        0,
    );

    let stub = common::StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x0f".into())); // 15
    stub.set("eth_getLogs", serde_json::Value::Array(vec![deposit_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(10));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = history_cfg(gateway, vault, None);
    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer run must succeed: {:?}",
        outcome.error
    );
    stub.shutdown();

    // account_history_events must have 1 deposit row for share_receiver.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM account_history_events WHERE chain_id = $1 AND kind = 'deposit'")
            .bind(8453_i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(count, 1, "one deposit history row expected, got {count}");

    // Also verify agent_deposits was populated (no regression).
    assert!(
        fx.db.count(CountTable::AgentDeposits).await.unwrap() >= 1,
        "agent_deposits must still be populated"
    );
}

/// AC-2: ERC-4626 Withdraw event is stored as `kind="withdrawal"` in
/// account_history_events, keyed to the owner address.
#[tokio::test]
async fn account_history_withdrawal_event_indexed() {
    let fx = pg_fixture().await;

    let gateway = Address::from([0x11u8; 20]);
    let vault = Address::from([0x22u8; 20]);
    let owner = Address::from([0x44u8; 20]);
    let caller = Address::from([0x33u8; 20]);
    let receiver = Address::from([0x55u8; 20]);

    let withdraw_log = encode_log(
        IVaultEvents::Withdraw {
            caller,
            receiver,
            owner,
            assets: U256::from(800_000u64),
            shares: U256::from(800_000u64),
        },
        vault,
        10u64,
        [0x77u8; 32],
        0,
    );

    let stub = common::StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x0f".into())); // 15
    stub.set("eth_getLogs", serde_json::Value::Array(vec![withdraw_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(10));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = history_cfg(gateway, vault, None);
    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer run must succeed: {:?}",
        outcome.error
    );
    stub.shutdown();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM account_history_events WHERE chain_id = $1 AND kind = 'withdrawal'")
            .bind(8453_i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(count, 1, "one withdrawal history row expected, got {count}");
}

/// AC-3: ExitFeeCharged event is stored as `kind="fee_charged"` in
/// account_history_events, keyed to the owner address.
#[tokio::test]
async fn account_history_fee_charged_event_indexed() {
    let fx = pg_fixture().await;

    let gateway = Address::from([0x11u8; 20]);
    let vault = Address::from([0x22u8; 20]);
    let owner = Address::from([0x44u8; 20]);
    let receiver = Address::from([0x55u8; 20]);

    let fee_log = encode_log(
        IVaultEvents::ExitFeeCharged {
            owner,
            receiver,
            grossAssets: U256::from(1_000_000u64),
            fee: U256::from(50_000u64),
            netAssets: U256::from(950_000u64),
        },
        vault,
        10u64,
        [0x77u8; 32],
        0,
    );

    let stub = common::StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x0f".into())); // 15
    stub.set("eth_getLogs", serde_json::Value::Array(vec![fee_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(10));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = history_cfg(gateway, vault, None);
    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer run must succeed: {:?}",
        outcome.error
    );
    stub.shutdown();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM account_history_events WHERE chain_id = $1 AND kind = 'fee_charged'")
            .bind(8453_i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "one fee_charged history row expected, got {count}"
    );
}

/// AC-4: AgentAuthorized event is stored as `kind="policy_change"` in
/// account_history_events, keyed to the agent address.
#[tokio::test]
async fn account_history_policy_change_authorized_indexed() {
    let fx = pg_fixture().await;

    let gateway = Address::from([0x11u8; 20]);
    let vault = Address::from([0x22u8; 20]);
    let agent = Address::from([0x33u8; 20]);
    let owner = Address::from([0x44u8; 20]);
    let share_receiver = Address::from([0x55u8; 20]);

    let auth_log = encode_log(
        IGatewayEvents::AgentAuthorized {
            agent,
            owner,
            validUntil: 2_000_000_000u64,
            maxPerPayment: U256::from(5_000_000u64),
            maxPerWindow: U256::from(50_000_000u64),
            shareReceiver: share_receiver,
        },
        gateway,
        10u64,
        [0x77u8; 32],
        0,
    );

    let stub = common::StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x0f".into())); // 15
    stub.set("eth_getLogs", serde_json::Value::Array(vec![auth_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(10));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = history_cfg(gateway, vault, None);
    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer run must succeed: {:?}",
        outcome.error
    );
    stub.shutdown();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM account_history_events WHERE chain_id = $1 AND kind = 'policy_change'")
            .bind(8453_i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "one policy_change history row expected, got {count}"
    );

    // agent_policies must also be populated (no regression).
    assert!(
        fx.db.count(CountTable::AgentPolicies).await.unwrap() >= 1,
        "agent_policies must still be populated"
    );
}

/// AC-5: VoteCast event is stored as `kind="governance_vote"` in
/// account_history_events, keyed to the voter address.
#[tokio::test]
async fn account_history_governance_vote_indexed() {
    let fx = pg_fixture().await;

    let gateway = Address::from([0x11u8; 20]);
    let vault = Address::from([0x22u8; 20]);
    let gov = Address::from([0x99u8; 20]);
    let voter = Address::from([0x33u8; 20]);
    let proposer = Address::from([0x44u8; 20]);

    // First seed a proposal so the governance_votes FK is satisfied.
    let proposal_log = encode_log(
        IRouterGovernanceEvents::ProposalCreated {
            proposalId: U256::from(1u64),
            proposer,
            vaults: vec![],
            bps: vec![],
            votingDeadline: 100u64,
        },
        gov,
        9u64,
        [0x66u8; 32],
        0,
    );
    let vote_log = encode_log(
        IRouterGovernanceEvents::VoteCast {
            proposalId: U256::from(1u64),
            voter,
            power: U256::from(1u64),
            totalFor: U256::from(1u64),
        },
        gov,
        10u64,
        [0x77u8; 32],
        0,
    );

    let stub = common::StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x0f".into())); // 15
    stub.set(
        "eth_getLogs",
        serde_json::Value::Array(vec![proposal_log, vote_log]),
    );
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(10));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = history_cfg(gateway, vault, Some(gov));
    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer run must succeed: {:?}",
        outcome.error
    );
    stub.shutdown();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM account_history_events WHERE chain_id = $1 AND kind = 'governance_vote'")
            .bind(8453_i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "one governance_vote history row expected, got {count}"
    );

    // governance_votes must also be populated (no regression).
    assert!(
        fx.db.count(CountTable::GovernanceVotes).await.unwrap() >= 1,
        "governance_votes must still be populated"
    );
}

/// AC-6: A deposit and withdrawal for the same account in the same block
/// range are both stored and appear in block-ascending order when queried.
#[tokio::test]
async fn account_history_deposit_and_withdrawal_chronological() {
    let fx = pg_fixture().await;

    let gateway = Address::from([0x11u8; 20]);
    let vault = Address::from([0x22u8; 20]);
    let agent = Address::from([0x33u8; 20]);
    let owner = Address::from([0x44u8; 20]); // same address for deposit share_receiver and withdrawal owner

    let deposit_log = encode_log(
        IGatewayEvents::AgentDeposit {
            paymentId: alloy_primitives::FixedBytes::from([0x55u8; 32]),
            orderId: alloy_primitives::FixedBytes::from([0x66u8; 32]),
            agent,
            shareReceiver: owner,
            amount: U256::from(1_000_000u64),
            sharesMinted: U256::from(1_000_000u64),
            windowId: 1u64,
        },
        gateway,
        8u64, // lower block
        [0x77u8; 32],
        0,
    );
    let withdraw_log = encode_log(
        IVaultEvents::Withdraw {
            caller: agent,
            receiver: agent,
            owner,
            assets: U256::from(800_000u64),
            shares: U256::from(800_000u64),
        },
        vault,
        10u64, // higher block
        [0x88u8; 32],
        0,
    );

    let stub = common::StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x0f".into())); // 15
    stub.set(
        "eth_getLogs",
        serde_json::Value::Array(vec![deposit_log, withdraw_log]),
    );
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(10));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = history_cfg(gateway, vault, None);
    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer run must succeed: {:?}",
        outcome.error
    );
    stub.shutdown();

    // Both events must be stored.
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM account_history_events WHERE chain_id = $1 AND account = $2",
    )
    .bind(8453_i64)
    .bind(&owner.into_array()[..])
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    assert_eq!(
        total, 2,
        "deposit and withdrawal must both appear in account_history_events"
    );

    // Chronological order: deposit (block 8) before withdrawal (block 10).
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT block_number, kind FROM account_history_events \
         WHERE chain_id = $1 AND account = $2 \
         ORDER BY block_number ASC, log_index ASC",
    )
    .bind(8453_i64)
    .bind(&owner.into_array()[..])
    .fetch_all(fx.db.pool())
    .await
    .unwrap();

    assert_eq!(
        rows[0].1, "deposit",
        "first event must be deposit at block 8"
    );
    assert_eq!(
        rows[1].1, "withdrawal",
        "second event must be withdrawal at block 10"
    );
    assert!(rows[0].0 < rows[1].0, "block order must be ascending");
}

/// AC-7 (test-plan item): idempotency — re-indexing the same blocks
/// must not insert duplicate account_history_events rows.
#[tokio::test]
async fn account_history_insert_is_idempotent() {
    let fx = pg_fixture().await;

    fx.db.upsert_chain(8453, "base", "test").await.unwrap();

    let first = fx
        .db
        .insert_history_event(
            8453,
            100,
            0,
            [0x22u8; 32],
            [0x55u8; 20],
            "deposit",
            Some([0x66u8; 20]),
            Some([0x77u8; 20]),
            Some(U256::from(1_000_000u64)),
        )
        .await
        .unwrap();
    assert_eq!(first, 1, "first insert must land");

    let second = fx
        .db
        .insert_history_event(
            8453,
            100,
            0,
            [0x22u8; 32],
            [0x55u8; 20],
            "deposit",
            Some([0x66u8; 20]),
            Some([0x77u8; 20]),
            Some(U256::from(1_000_000u64)),
        )
        .await
        .unwrap();
    assert_eq!(
        second, 0,
        "second insert must be a no-op (ON CONFLICT DO NOTHING)"
    );

    let count = fx.db.count(CountTable::AccountHistoryEvents).await.unwrap();
    assert_eq!(
        count, 1,
        "exactly one row must be in account_history_events"
    );
}
