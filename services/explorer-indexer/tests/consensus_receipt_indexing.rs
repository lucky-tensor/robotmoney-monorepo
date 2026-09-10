//! Canonical: docs/architecture.md §4.9 — Consensus Recommendation Receipt Contract
//! Implements: issue #1247 — consensus receipt event indexing acceptance tests.
//!
//! Five cases, modelled on `tests/committee_indexing.rs` (hand-encoded logs
//! served by a `StubRpcServer`):
//!   1. A `ReceiptRecorded` log creates a `consensus_receipts` row with the
//!      right digest / submitter / uri / index.
//!   2. Digest verification is TRUE when the served payload hashes to
//!      `payloadDigest`.
//!   3. Digest verification is FALSE on a hash mismatch — and the row still
//!      exists. A receipt is never dropped silently: an unverifiable
//!      commitment must be visible as unverified, not absent.
//!   4. A `ReceiptReleased` log flips `released` / `released_at` on the
//!      existing row.
//!   5. A reorg at the safe head rewrites receipt rows: rows above the fork
//!      root are DELETED by `Db::delete_above_block`, rows at/below survive,
//!      and a release that landed above the root is rolled back in place.
//!
//! All tests skip cleanly when Docker is not available (the shared
//! `pg_fixture` convention).

mod common;

use alloy_primitives::{Address, FixedBytes, LogData, U256};
use alloy_sol_types::SolEvent as _;
use common::{pg_fixture, StubRpcServer};
use explorer_indexer::{
    abi::IConsensusRecommendationReceiptEvents,
    db::CountTable,
    indexer::{run_once, IndexerConfig},
    rpc::JsonRpc,
};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CHAIN: i64 = 8453;

/// Every column of a `consensus_receipts` row, in SELECT order:
/// receipt_id, receipt_index, submitter, payload_digest, payload_uri,
/// recorded_at, block_number, log_index, tx_hash, verified, payload_bytes,
/// released, released_at.
type FullReceiptRow = (
    Vec<u8>,
    i64,
    Vec<u8>,
    Vec<u8>,
    String,
    i64,
    i64,
    i32,
    Vec<u8>,
    bool,
    Option<i64>,
    bool,
    Option<i64>,
);

// ─── helpers ─────────────────────────────────────────────────────────────────

fn receipt_addr() -> Address {
    Address::from([0xC7u8; 20])
}
fn gateway_addr() -> Address {
    Address::from([0x11u8; 20])
}
fn vault_addr() -> Address {
    Address::from([0x22u8; 20])
}
fn submitter_addr() -> Address {
    Address::from([0x5Au8; 20])
}
fn timelock_addr() -> Address {
    Address::from([0x7Bu8; 20])
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

fn log_json(
    address: Address,
    log_data: &LogData,
    block_number: u64,
    block_hash_byte: u8,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{t:#x}"))
        .collect();
    serde_json::json!({
        "address":          format!("{address:#x}"),
        "topics":           topics,
        "data":             format!("0x{}", hex::encode(log_data.data.as_ref())),
        "blockNumber":      format!("0x{block_number:x}"),
        "blockHash":        format!("0x{}", hex::encode([block_hash_byte; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{log_index:x}"),
    })
}

#[allow(clippy::too_many_arguments)]
fn encode_receipt_recorded_log(
    contract: Address,
    receipt_id: [u8; 32],
    submitter: Address,
    index: u64,
    payload_digest: [u8; 32],
    payload_uri: &str,
    recorded_at: u64,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    let event = IConsensusRecommendationReceiptEvents::ReceiptRecorded {
        receiptId: FixedBytes(receipt_id),
        submitter,
        index: U256::from(index),
        payloadDigest: FixedBytes(payload_digest),
        payloadUri: payload_uri.to_string(),
        recordedAt: recorded_at,
    };
    log_json(
        contract,
        &event.encode_log_data(),
        block_number,
        0xcc,
        tx_hash,
        log_index,
    )
}

fn encode_receipt_released_log(
    contract: Address,
    receipt_id: [u8; 32],
    released_by: Address,
    released_at: u64,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    let event = IConsensusRecommendationReceiptEvents::ReceiptReleased {
        receiptId: FixedBytes(receipt_id),
        releasedBy: released_by,
        releasedAt: released_at,
    };
    log_json(
        contract,
        &event.encode_log_data(),
        block_number,
        0xdd,
        tx_hash,
        log_index,
    )
}

fn base_cfg(end_block: u64) -> IndexerConfig {
    IndexerConfig {
        chain_id: CHAIN,
        chain_name: "base".to_string(),
        rpc_label: "stub".to_string(),
        gateway: gateway_addr(),
        vault: vault_addr(),
        registry: None,
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: Some(receipt_addr()),
        max_blocks_per_tick: 100,
        end_block: Some(end_block),
        feature_flags: 0,
    }
}

/// Program the stub for a single-tick run that indexes up to `block`.
/// `eth_blockNumber` is set well above `block + CONFIRMATIONS`.
fn program_stub(stub: &StubRpcServer, block: u64, hash_byte: u8, parent_byte: u8) {
    stub.set(
        "eth_blockNumber",
        serde_json::json!(format!("0x{:x}", block + 10)),
    );
    stub.set(
        "eth_getBlockByNumber",
        stub_block(block, hash_byte, parent_byte),
    );
    // eth_call backs the vault heartbeat snapshot (totalAssets / totalSupply).
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
}

/// A tiny HTTP server that serves a fixed body on any request — stands in for
/// the `payloadUri` route serving the receipt's canonical bytes.
struct PayloadServer {
    pub url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl PayloadServer {
    async fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/api/swarm/receipts/session-1");
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    accept = listener.accept() => {
                        let Ok((mut stream, _)) = accept else { continue };
                        let body = body.clone();
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
        PayloadServer { url, _shutdown: tx }
    }
}

/// The canonical preimage shape pinned by
/// `tests/fixtures/consensus-receipt.canonicalization.json`: the domain tag,
/// the compact JSON, and a trailing newline. The exact JSON does not matter to
/// the indexer — only that keccak256(served bytes) == payloadDigest.
fn canonical_payload() -> Vec<u8> {
    let mut v = b"robotmoney:consensus-receipt:v1\n".to_vec();
    v.extend_from_slice(br#"{"session_id":"session-1","subject_id":"vault-a"}"#);
    v.push(b'\n');
    v
}

// ─── AC-1 / AC-2: ReceiptRecorded creates a verified row ─────────────────────

#[tokio::test]
async fn receipt_recorded_creates_row_with_digest_verified() {
    let fx = pg_fixture().await;

    let payload = canonical_payload();
    let digest = alloy_primitives::keccak256(&payload);
    let srv = PayloadServer::start(payload.clone()).await;

    let receipt_id = [0x9au8; 32];
    let tx: [u8; 32] = [0x01; 32];
    let rec_log = encode_receipt_recorded_log(
        receipt_addr(),
        receipt_id,
        submitter_addr(),
        7,
        digest.0,
        &srv.url,
        1_700_000_000,
        10,
        tx,
        3,
    );

    let stub = StubRpcServer::start().await;
    program_stub(&stub, 10, 0xaa, 0x00);
    stub.set("eth_getLogs", serde_json::json!([rec_log]));

    let rpc = JsonRpc::new(&stub.url);
    let outcome = run_once(&fx.db, &rpc, &base_cfg(10)).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer error: {:?}",
        outcome.error
    );

    assert_eq!(
        fx.db.count(CountTable::ConsensusReceipts).await.unwrap(),
        1,
        "expected 1 consensus_receipts row"
    );

    let row: FullReceiptRow = sqlx::query_as(
        "SELECT receipt_id, receipt_index, submitter, payload_digest, payload_uri, \
                recorded_at, block_number, log_index, tx_hash, verified, payload_bytes, \
                released, released_at \
         FROM consensus_receipts WHERE chain_id = $1",
    )
    .bind(CHAIN)
    .fetch_one(fx.db.pool())
    .await
    .unwrap();

    assert_eq!(row.0, receipt_id.to_vec(), "receipt_id mismatch");
    assert_eq!(row.1, 7, "receipt_index mismatch");
    assert_eq!(row.2, submitter_addr().as_slice(), "submitter mismatch");
    assert_eq!(row.3, digest.0.to_vec(), "payload_digest mismatch");
    assert_eq!(row.4, srv.url, "payload_uri mismatch");
    assert_eq!(row.5, 1_700_000_000, "recorded_at mismatch");
    assert_eq!(row.6, 10, "block_number mismatch");
    assert_eq!(row.7, 3, "log_index mismatch");
    assert_eq!(row.8, tx.to_vec(), "tx_hash mismatch");
    assert!(
        row.9,
        "verified must be true — the served body hashes to payload_digest"
    );
    assert_eq!(
        row.10,
        Some(payload.len() as i64),
        "payload_bytes must record the fetched body length"
    );
    assert!(!row.11, "a freshly recorded receipt must not be released");
    assert_eq!(row.12, None, "released_at must be NULL before release");
}

// ─── AC-3: digest mismatch stores the row with verified = false ──────────────

#[tokio::test]
async fn receipt_recorded_digest_mismatch_stores_unverified_row() {
    let fx = pg_fixture().await;

    // The URI serves bytes that do NOT hash to the on-chain digest.
    let served = b"{\"tampered\":true}".to_vec();
    let wrong_digest = [0xdeu8; 32];
    assert_ne!(alloy_primitives::keccak256(&served).0, wrong_digest);
    let srv = PayloadServer::start(served).await;

    let receipt_id = [0x9bu8; 32];
    let tx: [u8; 32] = [0x02; 32];
    let rec_log = encode_receipt_recorded_log(
        receipt_addr(),
        receipt_id,
        submitter_addr(),
        0,
        wrong_digest,
        &srv.url,
        1_700_000_100,
        10,
        tx,
        0,
    );

    let stub = StubRpcServer::start().await;
    program_stub(&stub, 10, 0xaa, 0x00);
    stub.set("eth_getLogs", serde_json::json!([rec_log]));

    let rpc = JsonRpc::new(&stub.url);
    let outcome = run_once(&fx.db, &rpc, &base_cfg(10)).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "a digest mismatch must not fail the tick: {:?}",
        outcome.error
    );

    // The row must STILL EXIST — a receipt is never dropped silently.
    assert_eq!(
        fx.db.count(CountTable::ConsensusReceipts).await.unwrap(),
        1,
        "an unverifiable receipt must still be stored, not dropped"
    );

    let (verified, digest, uri): (bool, Vec<u8>, String) = sqlx::query_as(
        "SELECT verified, payload_digest, payload_uri FROM consensus_receipts \
         WHERE chain_id = $1 AND receipt_id = $2",
    )
    .bind(CHAIN)
    .bind(&receipt_id[..])
    .fetch_one(fx.db.pool())
    .await
    .unwrap();

    assert!(!verified, "verified must be false on a digest mismatch");
    assert_eq!(
        digest,
        wrong_digest.to_vec(),
        "the on-chain commitment is still recorded verbatim"
    );
    assert_eq!(uri, srv.url, "payload_uri is still recorded verbatim");
}

/// An unreachable `payloadUri` is non-fatal: the commitment row is stored with
/// `verified = false` and a NULL `payload_bytes`.
#[tokio::test]
async fn receipt_recorded_unreachable_uri_stores_unverified_row() {
    let fx = pg_fixture().await;

    // Port 1 on loopback: nothing listens, so the GET fails fast.
    let dead_uri = "http://127.0.0.1:1/api/swarm/receipts/session-dead";
    let receipt_id = [0x9cu8; 32];
    let rec_log = encode_receipt_recorded_log(
        receipt_addr(),
        receipt_id,
        submitter_addr(),
        1,
        [0xefu8; 32],
        dead_uri,
        1_700_000_200,
        10,
        [0x03; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    program_stub(&stub, 10, 0xaa, 0x00);
    stub.set("eth_getLogs", serde_json::json!([rec_log]));

    let rpc = JsonRpc::new(&stub.url);
    let outcome = run_once(&fx.db, &rpc, &base_cfg(10)).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "a payload fetch failure must be non-fatal: {:?}",
        outcome.error
    );

    let (verified, payload_bytes): (bool, Option<i64>) = sqlx::query_as(
        "SELECT verified, payload_bytes FROM consensus_receipts \
         WHERE chain_id = $1 AND receipt_id = $2",
    )
    .bind(CHAIN)
    .bind(&receipt_id[..])
    .fetch_one(fx.db.pool())
    .await
    .unwrap();

    assert!(
        !verified,
        "an unfetchable payload must leave verified=false"
    );
    assert_eq!(
        payload_bytes, None,
        "payload_bytes must be NULL when the fetch failed"
    );
}

// ─── AC-4: ReceiptReleased flips the existing row ────────────────────────────

#[tokio::test]
async fn receipt_released_flips_released_columns() {
    let fx = pg_fixture().await;

    let payload = canonical_payload();
    let digest = alloy_primitives::keccak256(&payload);
    let srv = PayloadServer::start(payload).await;

    let receipt_id = [0x9du8; 32];
    let rec_log = encode_receipt_recorded_log(
        receipt_addr(),
        receipt_id,
        submitter_addr(),
        0,
        digest.0,
        &srv.url,
        1_700_000_000,
        9,
        [0x04; 32],
        0,
    );
    let rel_log = encode_receipt_released_log(
        receipt_addr(),
        receipt_id,
        timelock_addr(),
        1_700_000_500,
        10,
        [0x05; 32],
        1,
    );

    let stub = StubRpcServer::start().await;
    program_stub(&stub, 10, 0xaa, 0x00);
    stub.set("eth_getLogs", serde_json::json!([rec_log, rel_log]));

    let rpc = JsonRpc::new(&stub.url);
    let outcome = run_once(&fx.db, &rpc, &base_cfg(10)).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "indexer error: {:?}",
        outcome.error
    );

    // Release is an in-place flip, not a second row.
    assert_eq!(
        fx.db.count(CountTable::ConsensusReceipts).await.unwrap(),
        1,
        "ReceiptReleased must update the existing row, not append one"
    );

    let (released, released_at, released_block, released_by): (
        bool,
        Option<i64>,
        Option<i64>,
        Option<Vec<u8>>,
    ) = sqlx::query_as(
        "SELECT released, released_at, released_block_number, released_by \
         FROM consensus_receipts WHERE chain_id = $1 AND receipt_id = $2",
    )
    .bind(CHAIN)
    .bind(&receipt_id[..])
    .fetch_one(fx.db.pool())
    .await
    .unwrap();

    assert!(released, "released must be true after ReceiptReleased");
    assert_eq!(released_at, Some(1_700_000_500), "released_at mismatch");
    assert_eq!(
        released_block,
        Some(10),
        "released_block_number must record the release block for reorg rollback"
    );
    assert_eq!(
        released_by.as_deref(),
        Some(timelock_addr().as_slice()),
        "released_by must record the ADMIN_ROLE caller"
    );
}

// ─── AC-5: a reorg at the safe head rewrites receipt rows ────────────────────

/// Explicit acceptance criterion: `consensus_receipts` must be rolled back by
/// `Db::delete_above_block`, the reorg rewrite. Rows above the fork root are
/// deleted; rows at/below it survive. This test exercises the real
/// `delete_above_block` path (the same function `run_inner` calls on a reorg),
/// so a future edit that drops `consensus_receipts` from that table list turns
/// this test RED.
#[tokio::test]
async fn reorg_rollback_deletes_receipts_above_root_and_keeps_the_rest() {
    let fx = pg_fixture().await;
    let db = &fx.db;
    db.upsert_chain(CHAIN, "base", "stub").await.unwrap();

    // Three receipts: block 100 (below root), 100 is the fork root, and blocks
    // 101 / 102 above it.
    for (i, (id_byte, block)) in [(0xa1u8, 99i64), (0xa2u8, 100), (0xa3u8, 101), (0xa4u8, 102)]
        .into_iter()
        .enumerate()
    {
        db.insert_consensus_receipt(
            CHAIN,
            [id_byte; 32],
            i as i64,
            submitter_addr().into_array(),
            [0xd0u8; 32],
            "https://example.invalid/api/swarm/receipts/s",
            1_700_000_000 + block,
            block,
            0,
            [id_byte; 32],
            true,
            Some(64),
        )
        .await
        .unwrap();
    }
    assert_eq!(db.count(CountTable::ConsensusReceipts).await.unwrap(), 4);

    // The block-99 receipt was released at block 102 — above the fork root, so
    // the in-place release must be rolled back even though the row survives.
    db.mark_consensus_receipt_released(
        CHAIN,
        [0xa1u8; 32],
        timelock_addr().into_array(),
        1_700_000_102,
        102,
    )
    .await
    .unwrap();

    // ── The reorg rewrite. Fork root = 100. ──
    db.delete_above_block(CHAIN, 100).await.unwrap();

    // Rows above the root are gone.
    assert_eq!(
        db.count(CountTable::ConsensusReceipts).await.unwrap(),
        2,
        "receipts recorded above the fork root must be deleted by the reorg rewrite"
    );
    let surviving: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        "SELECT receipt_id, block_number FROM consensus_receipts \
         WHERE chain_id = $1 ORDER BY block_number",
    )
    .bind(CHAIN)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        surviving,
        vec![([0xa1u8; 32].to_vec(), 99), ([0xa2u8; 32].to_vec(), 100)],
        "exactly the receipts at/below the fork root must survive"
    );

    // The release that landed above the root is rolled back in place — the row
    // itself is never deleted (an unreleased receipt stays a public record).
    let (released, released_at, released_block): (bool, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT released, released_at, released_block_number FROM consensus_receipts \
         WHERE chain_id = $1 AND receipt_id = $2",
    )
    .bind(CHAIN)
    .bind(&[0xa1u8; 32][..])
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(
        !released,
        "a release recorded above the fork root must be un-released by the rewrite"
    );
    assert_eq!(released_at, None);
    assert_eq!(released_block, None);

    // Re-indexing the same range after the rollback is a no-op for a row that
    // survived (ON CONFLICT DO NOTHING), so recovery never duplicates receipts.
    let again = db
        .insert_consensus_receipt(
            CHAIN,
            [0xa2u8; 32],
            1,
            submitter_addr().into_array(),
            [0xd0u8; 32],
            "https://example.invalid/api/swarm/receipts/s",
            1_700_000_100,
            100,
            0,
            [0xa2u8; 32],
            true,
            Some(64),
        )
        .await
        .unwrap();
    assert_eq!(again, 0, "re-inserting a surviving receipt must be a no-op");
    assert_eq!(db.count(CountTable::ConsensusReceipts).await.unwrap(), 2);
}

/// End-to-end reorg through `run_once`: tick 1 indexes a receipt at the safe
/// head; tick 2 sees a different hash at the cursor height, walks back, and the
/// rollback removes the receipt. Re-indexing then re-inserts it idempotently.
#[tokio::test]
async fn run_once_reorg_at_safe_head_rewrites_receipt_rows() {
    let fx = pg_fixture().await;

    let payload = canonical_payload();
    let digest = alloy_primitives::keccak256(&payload);
    let srv = PayloadServer::start(payload).await;
    let receipt_id = [0xbeu8; 32];

    let rec_log = |block: u64| {
        encode_receipt_recorded_log(
            receipt_addr(),
            receipt_id,
            submitter_addr(),
            0,
            digest.0,
            &srv.url,
            1_700_000_000,
            block,
            [0x06; 32],
            0,
        )
    };

    // ── Tick 1: index block 10 (hash 0xaa…) carrying the receipt. ──
    let pre = StubRpcServer::start().await;
    program_stub(&pre, 10, 0xaa, 0x00);
    pre.set("eth_getLogs", serde_json::json!([rec_log(10)]));
    let o1 = run_once(&fx.db, &JsonRpc::new(&pre.url), &base_cfg(10))
        .await
        .unwrap();
    assert!(o1.error.is_none(), "tick 1 must succeed: {:?}", o1.error);
    assert_eq!(o1.last_indexed_block, Some(10));
    assert_eq!(
        fx.db.count(CountTable::ConsensusReceipts).await.unwrap(),
        1,
        "tick 1 must have indexed the receipt"
    );
    pre.shutdown();

    // ── Tick 2: block 10 now hashes differently → reorg → rollback. ──
    // `walk_back_to_match` finds no matching stored hash below 10 (only block 10
    // was persisted), so root = -1 and every block-numbered row is wiped.
    let post = StubRpcServer::start().await;
    program_stub(&post, 10, 0xee, 0x00);
    post.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    let o2 = run_once(&fx.db, &JsonRpc::new(&post.url), &base_cfg(10))
        .await
        .unwrap();
    assert!(o2.error.is_none(), "tick 2 must succeed: {:?}", o2.error);
    assert!(o2.reorg_detected, "tick 2 must detect the reorg");
    assert_eq!(
        fx.db.count(CountTable::ConsensusReceipts).await.unwrap(),
        0,
        "the reorg rewrite must delete the orphaned receipt — \
         consensus_receipts is in delete_above_block's table list"
    );
    // The rolled-back block is gone from `blocks` too, confirming the rewrite
    // ran the full cascade rather than only touching the receipt table.
    assert_eq!(
        fx.db.get_block_hash(CHAIN, 10).await.unwrap(),
        Some([0xeeu8; 32]),
        "block 10 must be re-persisted at its canonical hash after the rewrite"
    );
    post.shutdown();
}
