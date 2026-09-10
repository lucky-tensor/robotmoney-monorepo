//! Canonical: docs/architecture.md §5.4 — Explorer Indexer and API
//! Implements: issue #1053 — IC event indexing acceptance tests.
//!
//! Four cases as required by the issue acceptance criteria:
//!   1. AgentRegistered creates a committee_agents row.
//!   2. VoteSubmitted with a valid (matching) memo hash sets verified=true
//!      and populates tilt fields.
//!   3. VoteSubmitted with a mismatched memo hash sets verified=false and
//!      stores the commitment without tilt data.
//!   4. AgentRevoked sets active=false on the committee_agents row.
//!
//! All tests skip cleanly when Docker is not available.

mod common;

use alloy_primitives::Address;
use alloy_sol_types::SolEvent as _;
use common::{pg_fixture, StubRpcServer};
use explorer_indexer::{
    abi::IInvestmentCommitteePolicyEvents,
    db::CountTable,
    indexer::{run_once, IndexerConfig},
    rpc::JsonRpc,
};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn ic_addr() -> Address {
    Address::from([0xCCu8; 20])
}
fn gateway_addr() -> Address {
    Address::from([0x11u8; 20])
}
fn vault_addr() -> Address {
    Address::from([0x22u8; 20])
}
fn agent_addr() -> Address {
    Address::from([0xAAu8; 20])
}

fn stub_block(number: u64, hash_byte: u8, parent_byte: u8) -> serde_json::Value {
    serde_json::json!({
        "number":     format!("0x{:x}", number),
        "hash":       format!("0x{}", hex::encode([hash_byte; 32])),
        "parentHash": format!("0x{}", hex::encode([parent_byte; 32])),
        "timestamp":  "0x65000000",
        "transactions": []
    })
}

fn encode_agent_registered_log(
    ic: Address,
    agent: Address,
    agent_id: &str,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    let event = IInvestmentCommitteePolicyEvents::AgentRegistered {
        agent,
        agentId: agent_id.to_string(),
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));
    serde_json::json!({
        "address":          format!("{:#x}", ic),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xaau8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

fn encode_agent_revoked_log(
    ic: Address,
    agent: Address,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    let event = IInvestmentCommitteePolicyEvents::AgentRevoked { agent };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));
    serde_json::json!({
        "address":          format!("{:#x}", ic),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xbbu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

#[allow(clippy::too_many_arguments)]
fn encode_vote_submitted_log(
    ic: Address,
    vote_id: u64,
    agent: Address,
    vault: Address,
    stance: u8,
    target_weight_bps: u16,
    confidence: u8,
    rationale_uri: &str,
    vote_json_hash: [u8; 32],
    timestamp: u64,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::{FixedBytes, LogData, U256};
    let event = IInvestmentCommitteePolicyEvents::VoteSubmitted {
        voteId: U256::from(vote_id),
        agent,
        vault,
        stance,
        targetWeightBps: target_weight_bps,
        confidence,
        rationaleUri: rationale_uri.to_string(),
        voteJsonHash: FixedBytes(vote_json_hash),
        timestamp,
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));
    serde_json::json!({
        "address":          format!("{:#x}", ic),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xccu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

fn base_cfg(_stub: &StubRpcServer) -> IndexerConfig {
    IndexerConfig {
        chain_id: 8453,
        chain_name: "base".to_string(),
        rpc_label: "stub".to_string(),
        gateway: gateway_addr(),
        vault: vault_addr(),
        registry: None,
        router_governance: None,
        portfolio_router: None,
        investment_committee: Some(ic_addr()),
        consensus_receipt: None,
        max_blocks_per_tick: 100,
        end_block: Some(10),
        feature_flags: 0,
    }
}

fn program_stub_block10(stub: &StubRpcServer) {
    stub.set(
        "eth_blockNumber",
        serde_json::json!("0x14"), // 20 — safe head = 20 - 5 = 15 > 10
    );
    stub.set(
        "eth_getBlockByNumber",
        serde_json::json!(stub_block(10, 0xaa, 0x00)),
    );
    // eth_call is needed for vault heartbeat snapshot (totalAssets / totalSupply).
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
}

// ─── AC-1: AgentRegistered creates a row ─────────────────────────────────────

#[tokio::test]
async fn agent_registered_creates_row() {
    let fx = pg_fixture().await;

    let ic = ic_addr();
    let agent = agent_addr();
    let tx: [u8; 32] = [0x01; 32];

    let reg_log = encode_agent_registered_log(ic, agent, "athena-v1", 10, tx, 0);

    let stub = StubRpcServer::start().await;
    program_stub_block10(&stub);
    stub.set("eth_getLogs", serde_json::json!([reg_log]));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_cfg(&stub);

    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer error: {:?}",
        outcome.error
    );

    let count = fx.db.count(CountTable::CommitteeAgents).await.unwrap();
    assert_eq!(count, 1, "expected 1 committee_agents row");

    // Verify the row fields.
    let row: (Vec<u8>, String, bool) = sqlx::query_as(
        "SELECT agent_address, agent_id, active FROM committee_agents \
         WHERE chain_id = $1",
    )
    .bind(8453i64)
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    assert_eq!(row.0, agent.as_slice(), "agent_address mismatch");
    assert_eq!(row.1, "athena-v1", "agent_id mismatch");
    assert!(row.2, "active should be true");
}

// ─── AC-2: VoteSubmitted with valid memo sets verified=true ──────────────────

/// A tiny HTTP server that serves a fixed body on any request.
/// Used to simulate a valid rationaleUri endpoint.
struct MemoServer {
    pub url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MemoServer {
    async fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let url = format!("http://{}/memo.json", addr);
        let body_clone = body.clone();
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    accept = listener.accept() => {
                        let Ok((mut stream, _)) = accept else { continue };
                        let body = body_clone.clone();
                        tokio::spawn(async move {
                            let mut buf = [0u8; 4096];
                            let _ = stream.read(&mut buf).await;
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                            let _ = stream.write_all(&body).await;
                        });
                    }
                }
            }
        });
        MemoServer { url, _shutdown: tx }
    }
}

#[tokio::test]
async fn vote_submitted_valid_memo_sets_verified_true() {
    let fx = pg_fixture().await;

    // First register the agent.
    let reg_tx: [u8; 32] = [0x01; 32];
    let vote_tx: [u8; 32] = [0x02; 32];
    let ic = ic_addr();
    let agent = agent_addr();
    let vault = vault_addr();

    // Memo JSON body.
    let memo_body = br#"{"agent_id":"athena-v1","vault":"0x2222","stance":"Overweight"}"#;
    let memo_hash = alloy_primitives::keccak256(memo_body);

    // Start memo server.
    let memo_srv = MemoServer::start(memo_body.to_vec()).await;

    let reg_log = encode_agent_registered_log(ic, agent, "athena-v1", 9, reg_tx, 0);
    let vote_log = encode_vote_submitted_log(
        ic,
        1,
        agent,
        vault,
        0, // Overweight
        6000,
        80,
        &memo_srv.url,
        memo_hash.0,
        1_000_000,
        10,
        vote_tx,
        1,
    );

    let stub = StubRpcServer::start().await;
    program_stub_block10(&stub);
    stub.set("eth_getLogs", serde_json::json!([reg_log, vote_log]));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_cfg(&stub);

    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer error: {:?}",
        outcome.error
    );

    let (verified, stance, weight): (bool, i16, i16) = sqlx::query_as(
        "SELECT verified, stance, target_weight_bps FROM committee_votes \
         WHERE chain_id = $1 AND vote_id = 1",
    )
    .bind(8453i64)
    .fetch_one(fx.db.pool())
    .await
    .unwrap();

    assert!(verified, "verified should be true for valid memo hash");
    assert_eq!(stance, 0, "stance should be 0 (Overweight)");
    assert_eq!(weight, 6000, "target_weight_bps mismatch");

    // Regime snapshot should also exist.
    let snap_count = fx.db.count(CountTable::RegimeSnapshots).await.unwrap();
    assert_eq!(snap_count, 1, "expected 1 regime_snapshots row");
}

// ─── AC-3: VoteSubmitted with hash-mismatched memo sets verified=false ────────

#[tokio::test]
async fn vote_submitted_hash_mismatch_sets_verified_false() {
    let fx = pg_fixture().await;

    let ic = ic_addr();
    let agent = agent_addr();
    let vault = vault_addr();
    let reg_tx: [u8; 32] = [0x01; 32];
    let vote_tx: [u8; 32] = [0x03; 32];

    // Memo body that does NOT match the hash stored on-chain.
    let actual_body = b"different content that does not match";
    let wrong_hash = [0xddu8; 32]; // deliberately wrong

    let memo_srv = MemoServer::start(actual_body.to_vec()).await;

    let reg_log = encode_agent_registered_log(ic, agent, "athena-v1", 9, reg_tx, 0);
    let vote_log = encode_vote_submitted_log(
        ic,
        2,
        agent,
        vault,
        1, // Neutral
        5000,
        50,
        &memo_srv.url,
        wrong_hash,
        2_000_000,
        10,
        vote_tx,
        1,
    );

    let stub = StubRpcServer::start().await;
    program_stub_block10(&stub);
    stub.set("eth_getLogs", serde_json::json!([reg_log, vote_log]));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_cfg(&stub);

    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer error: {:?}",
        outcome.error
    );

    let (verified,): (bool,) =
        sqlx::query_as("SELECT verified FROM committee_votes WHERE chain_id = $1 AND vote_id = 2")
            .bind(8453i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();

    assert!(!verified, "verified should be false for hash mismatch");

    // No regime snapshot should be created for unverified votes.
    let snap_count = fx.db.count(CountTable::RegimeSnapshots).await.unwrap();
    assert_eq!(
        snap_count, 0,
        "expected 0 regime_snapshots for unverified vote"
    );
}

// ─── AC-4: AgentRevoked sets active=false ────────────────────────────────────

#[tokio::test]
async fn agent_revoked_sets_active_false() {
    let fx = pg_fixture().await;

    let ic = ic_addr();
    let agent = agent_addr();
    let reg_tx: [u8; 32] = [0x01; 32];
    let rev_tx: [u8; 32] = [0x04; 32];

    let reg_log = encode_agent_registered_log(ic, agent, "athena-v1", 9, reg_tx, 0);
    let rev_log = encode_agent_revoked_log(ic, agent, 10, rev_tx, 1);

    let stub = StubRpcServer::start().await;
    program_stub_block10(&stub);
    stub.set("eth_getLogs", serde_json::json!([reg_log, rev_log]));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_cfg(&stub);

    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer error: {:?}",
        outcome.error
    );

    let (active, revoked_at): (bool, Option<i64>) =
        sqlx::query_as("SELECT active, revoked_at FROM committee_agents WHERE chain_id = $1")
            .bind(8453i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();

    assert!(!active, "active should be false after AgentRevoked");
    assert_eq!(revoked_at, Some(10), "revoked_at should be block 10");
}
