//! Integration tests for issue #177: persist cursor block headers and fix
//! `walk_back_to_match` reorg detection for no-event cursor blocks.
//!
//! Scenarios covered:
//!
//! 1. **cursor_header_persisted**: the first run indexes an event block below
//!    a no-event cursor target and records the cursor block header in `blocks`.
//! 2. **reorg_below_no_event_cursor_deletes_stale_rows**: a simulated reorg
//!    replaces the event block; the next run detects the mismatch via the
//!    persisted cursor header, walks back to the true matching root, and
//!    deletes stale rows above it.
//! 3. **walk_back_does_not_accept_missing_hash_as_root**: when the cursor
//!    block has no stored hash, `walk_back_to_match` keeps walking instead
//!    of treating the gap as a clean root — tested indirectly through
//!    `run_once` with a two-tick sequence where the first tick leaves a
//!    hash-less gap block.

mod common;

use alloy_primitives::Address;
use common::{pg_fixture, StubRpcServer};
use explorer_indexer::{db::CountTable, indexer::run_once, indexer::IndexerConfig, rpc::JsonRpc};

/// Build the minimal stub block JSON for a given number, hash, parent_hash,
/// and timestamp.  The stub server returns this for `eth_getBlockByNumber`.
fn stub_block(number: u64, hash: &str, parent_hash: &str, timestamp: u64) -> serde_json::Value {
    serde_json::json!({
        "number":     format!("0x{:x}", number),
        "hash":       hash,
        "parentHash": parent_hash,
        "timestamp":  format!("0x{:x}", timestamp),
        "transactions": []
    })
}

/// Configure stub server for a tick whose tip is `tip` and produces an event
/// block at `event_block` (below the safe cursor `tip - CONFIRMATIONS`).
///
/// The stub always returns an empty log array for simplicity; the caller can
/// override `eth_getLogs` afterwards.
fn set_happy_responses(
    stub: &StubRpcServer,
    tip: u64,
    event_block_number: u64,
    event_block_hash: &str,
    cursor_number: u64,
    cursor_hash: &str,
    cursor_parent_hash: &str,
) {
    stub.set(
        "eth_blockNumber",
        serde_json::Value::String(format!("0x{:x}", tip)),
    );
    stub.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    // 32-byte zero for vault reads.
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    // eth_getBlockByNumber is called for:
    //   - block_header(last_indexed) in reorg check
    //   - block_header(target) for cursor header persist
    // We set a single canned response; the stub returns the same value
    // regardless of params.  For tests where we need different hashes for
    // different blocks we override after calling this helper.
    let _ = event_block_number; // used by caller to build eth_getLogs body
    let _ = event_block_hash;
    // Default: return the cursor block for any block-by-number query.
    stub.set(
        "eth_getBlockByNumber",
        stub_block(
            cursor_number,
            cursor_hash,
            cursor_parent_hash,
            1_700_000_000,
        ),
    );
}

/// Scenario 1: cursor header is persisted even when the target block has no
/// watched events.
#[tokio::test]
async fn cursor_header_persisted_for_no_event_target() {
    let fx = pg_fixture().await;

    let stub = StubRpcServer::start().await;
    // tip=110, CONFIRMATIONS=5, so safe_head=105, target=105 (with end_block=105).
    // No events → event_blocks set is empty.
    // The indexer must still write a blocks row for block 105.
    let cursor_hash = format!("0x{}", "ab".repeat(32));
    let cursor_parent = format!("0x{}", "aa".repeat(32));
    set_happy_responses(&stub, 110, 0, "", 105, &cursor_hash, &cursor_parent);

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
        max_blocks_per_tick: 200,
        end_block: Some(105),
        feature_flags: 0,
    };

    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "run must succeed: {:?}",
        outcome.error
    );
    assert_eq!(
        outcome.last_indexed_block,
        Some(105),
        "last_indexed_block must advance to cursor"
    );

    // The cursor block header must be stored even though no event was emitted.
    let stored_hash = fx.db.get_block_hash(8453, 105).await.unwrap();
    assert!(
        stored_hash.is_some(),
        "blocks row for cursor block 105 must be present after tick"
    );
    // Verify the stored hash matches what the stub returned.
    let expected: [u8; 32] = [0xabu8; 32];
    assert_eq!(
        stored_hash.unwrap(),
        expected,
        "stored cursor hash must match the stub response"
    );

    stub.shutdown();
}

/// Scenario 2: a reorg below a no-event cursor block deletes stale rows.
///
/// Sequence:
///   Tick 1 — indexes event block 100 (hash_A) + cursor 105 (hash_C).
///   Tick 2 — chain shows cursor 105 with a new hash (hash_C2, reorged).
///             walk_back must find that block 100 now has hash_A2 ≠ stored
///             hash_A, walk all the way to -1 (fresh start), delete above -1
///             (everything), and re-index.
///
/// In this test we use the stub server to simulate the two chain states.
/// Because the stub cannot serve different responses for the same method at
/// different points in time, we use two separate stub server instances.
#[tokio::test]
async fn reorg_below_no_event_cursor_deletes_stale_rows() {
    let fx = pg_fixture().await;

    // ── Tick 1: pre-reorg chain ──────────────────────────────────────────
    // Block 100 hash = 0xaa…; block 105 (cursor) hash = 0xcc….
    // We manually insert an agent_deposit row at block 100 to verify deletion.

    let pre_reorg = StubRpcServer::start().await;
    pre_reorg.set(
        "eth_blockNumber",
        serde_json::Value::String("0x6e".into()), // 110
    );
    pre_reorg.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    pre_reorg.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    // Block header for any query → return cursor block 105 with hash 0xcc….
    pre_reorg.set(
        "eth_getBlockByNumber",
        stub_block(
            105,
            &format!("0x{}", "cc".repeat(32)),
            &format!("0x{}", "bb".repeat(32)),
            1_700_000_000,
        ),
    );

    let rpc1 = JsonRpc::new(&pre_reorg.url);
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
        max_blocks_per_tick: 200,
        end_block: Some(105),
        feature_flags: 0,
    };

    let o1 = run_once(&fx.db, &rpc1, &cfg).await.unwrap();
    assert!(
        o1.error.is_none(),
        "pre-reorg run must succeed: {:?}",
        o1.error
    );
    assert_eq!(o1.last_indexed_block, Some(105));

    // Manually seed an agent_deposit at block 100 so we can verify deletion.
    fx.db.upsert_chain(8453, "base", "stub").await.unwrap();
    let gw = [0xaau8; 20];
    fx.db
        .upsert_contract(8453, gw, "gateway", None)
        .await
        .unwrap();
    fx.db
        .insert_block(8453, 100, [0xaau8; 32], [0x99u8; 32], 1_699_000_000)
        .await
        .unwrap();
    fx.db
        .insert_agent_deposit(
            8453,
            100,
            0,
            [0x11u8; 32],
            [0x22u8; 32],
            [0x33u8; 32],
            gw,
            gw,
            alloy_primitives::U256::from(1_000_000u64),
            alloy_primitives::U256::from(1_000_000u64),
            1,
            None,
        )
        .await
        .unwrap();
    assert_eq!(fx.db.count(CountTable::AgentDeposits).await.unwrap(), 1);

    pre_reorg.shutdown();

    // ── Tick 2: post-reorg chain ─────────────────────────────────────────
    // Cursor block 105 now has a different hash (0xdd…) → reorg detected.
    // walk_back walks block 105 → hash mismatch; block 100 → stored 0xaa…
    // but chain returns 0xee… → mismatch; continues past 0 → returns -1.
    // delete_above_block(-1) wipes all rows; re-index from scratch.
    //
    // After deletion the agent_deposit row at block 100 must be gone.

    let post_reorg = StubRpcServer::start().await;
    post_reorg.set(
        "eth_blockNumber",
        serde_json::Value::String("0x6e".into()), // 110
    );
    post_reorg.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    post_reorg.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    // After the reorg every block has a new hash.
    post_reorg.set(
        "eth_getBlockByNumber",
        stub_block(
            105,
            &format!("0x{}", "dd".repeat(32)), // new hash ≠ stored 0xcc…
            &format!("0x{}", "ee".repeat(32)),
            1_700_000_100,
        ),
    );

    let rpc2 = JsonRpc::new(&post_reorg.url);
    let cfg2 = IndexerConfig {
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
        max_blocks_per_tick: 200,
        end_block: Some(105),
        feature_flags: 0,
    };

    let o2 = run_once(&fx.db, &rpc2, &cfg2).await.unwrap();
    assert!(
        o2.error.is_none(),
        "post-reorg run must succeed: {:?}",
        o2.error
    );
    assert!(o2.reorg_detected, "reorg must be detected on tick 2");

    // All stale rows above the root (-1 → root at -1 means wipe all) must
    // be deleted.  The agent_deposit seeded at block 100 must be gone.
    assert_eq!(
        fx.db.count(CountTable::AgentDeposits).await.unwrap(),
        0,
        "stale agent_deposit must be deleted after reorg"
    );

    post_reorg.shutdown();
}

/// Scenario 3: `walk_back_to_match` must not treat a block with no stored hash
/// as a clean root.
///
/// We arrange two ticks where:
///   Tick 1 — only a no-event cursor block 10 is stored (hash_10).
///             Block 8 (which will later have a synthetic event row) has
///             NO stored blocks entry (pre-fix this would have been exploitable).
///   Then we manually insert an agent_deposit at block 8.
///   Tick 2 — chain says block 10 now has a new hash → reorg.
///             walk_back must not stop at block 9 (no stored hash) as a "root".
///             It must continue past 9 until it has walked past 0, returning -1.
///             delete_above_block(-1) must wipe the agent_deposit at block 8.
#[tokio::test]
async fn walk_back_does_not_accept_missing_hash_as_root() {
    let fx = pg_fixture().await;

    // ── Tick 1: pre-reorg ────────────────────────────────────────────────
    let pre = StubRpcServer::start().await;
    pre.set(
        "eth_blockNumber",
        serde_json::Value::String("0x0f".into()), // 15
    );
    pre.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    pre.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    // Cursor block 10 hash = 0x10…
    pre.set(
        "eth_getBlockByNumber",
        stub_block(
            10,
            &format!("0x{}", "10".repeat(32)),
            &format!("0x{}", "09".repeat(32)),
            1_700_000_000,
        ),
    );

    let rpc1 = JsonRpc::new(&pre.url);
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
        max_blocks_per_tick: 50,
        end_block: Some(10),
        feature_flags: 0,
    };

    let o1 = run_once(&fx.db, &rpc1, &cfg).await.unwrap();
    assert!(o1.error.is_none(), "pre-reorg tick: {:?}", o1.error);
    assert_eq!(o1.last_indexed_block, Some(10));

    // Block 10 header stored; blocks 0–9 have NO stored header.
    let h10 = fx.db.get_block_hash(8453, 10).await.unwrap();
    assert!(h10.is_some(), "cursor block 10 must be stored");
    let h9 = fx.db.get_block_hash(8453, 9).await.unwrap();
    assert!(h9.is_none(), "block 9 must NOT be stored (no events)");

    // Manually insert an agent_deposit at block 8 (below cursor, no stored header).
    let gw = [0xaau8; 20];
    fx.db
        .upsert_contract(8453, gw, "gateway", None)
        .await
        .unwrap();
    fx.db
        .insert_block(8453, 8, [0x08u8; 32], [0x07u8; 32], 1_699_000_000)
        .await
        .unwrap();
    fx.db
        .insert_agent_deposit(
            8453,
            8,
            0,
            [0x55u8; 32],
            [0x66u8; 32],
            [0x77u8; 32],
            gw,
            gw,
            alloy_primitives::U256::from(500_000u64),
            alloy_primitives::U256::from(500_000u64),
            2,
            None,
        )
        .await
        .unwrap();
    assert_eq!(fx.db.count(CountTable::AgentDeposits).await.unwrap(), 1);

    pre.shutdown();

    // ── Tick 2: post-reorg — block 10 has new hash ───────────────────────
    let post = StubRpcServer::start().await;
    post.set(
        "eth_blockNumber",
        serde_json::Value::String("0x0f".into()), // 15
    );
    post.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    post.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    // Block 10 now has hash 0xee… (reorged) and block 8 also reorged.
    // Stub returns the same block shape for all block-by-number queries.
    post.set(
        "eth_getBlockByNumber",
        stub_block(
            10,
            &format!("0x{}", "ee".repeat(32)), // new hash ≠ stored 0x10…
            &format!("0x{}", "dd".repeat(32)),
            1_700_000_100,
        ),
    );

    let rpc2 = JsonRpc::new(&post.url);
    let cfg2 = IndexerConfig {
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
        max_blocks_per_tick: 50,
        end_block: Some(10),
        feature_flags: 0,
    };

    let o2 = run_once(&fx.db, &rpc2, &cfg2).await.unwrap();
    assert!(o2.error.is_none(), "post-reorg tick: {:?}", o2.error);
    assert!(
        o2.reorg_detected,
        "reorg must be detected when cursor block hash changes"
    );

    // The stale agent_deposit at block 8 must be deleted.  If the old
    // walk_back bug were present, walk_back would stop at block 9 (missing
    // hash → "clean root"), delete only blocks 9–10, and leave the deposit
    // at block 8 alive.
    assert_eq!(
        fx.db.count(CountTable::AgentDeposits).await.unwrap(),
        0,
        "stale agent_deposit at block 8 must be deleted — walk_back must not stop at missing-hash block 9"
    );

    post.shutdown();
}
