//! Integration tests for issue #1021 (IDX-2 / IDX-8): make the indexer's run
//! cursor and vault status rollback reorg-safe.
//!
//! - **IDX-2** — `delete_above_block(root)` must cap any successful run's
//!   `last_indexed_block` down to `root`, so a reorg that rolls back to `root`
//!   and then fails in the same run leaves the next run resuming at `root + 1`
//!   with no block gap (the failed run row is excluded from the
//!   `error IS NULL` MAX, so without the cap the next `last_indexed_block()`
//!   would still report the pre-reorg cursor).
//! - **IDX-8** — `delete_above_block(root)` must revert an in-place
//!   `vaults.status` change written at a block above `root`, by pruning the
//!   block-numbered `vault_status_events` history and re-deriving the live
//!   `vaults.status` from the surviving event `<= root`.
//!
//! Both assertions are exercised end-to-end through `run_once` as well as at
//! the db layer for deterministic root values.

mod common;

use alloy_primitives::{Address, U256};
use common::{pg_fixture, StubRpcServer};
use explorer_indexer::db::{CountTable, REORG_ROLLBACK_EXCLUSIONS};
use explorer_indexer::{indexer::run_once, indexer::IndexerConfig, rpc::JsonRpc};
use sqlx::postgres::PgPool;
use std::collections::BTreeSet;

const CHAIN: i64 = 8453;

/// Minimal stub block JSON for `eth_getBlockByNumber`.
fn stub_block(number: u64, hash: &str, parent_hash: &str, timestamp: u64) -> serde_json::Value {
    serde_json::json!({
        "number":     format!("0x{:x}", number),
        "hash":       hash,
        "parentHash": parent_hash,
        "timestamp":  format!("0x{:x}", timestamp),
        "transactions": []
    })
}

// ─── IDX-2 ───────────────────────────────────────────────────────────────────

/// IDX-2 (db layer): a successful run recorded a cursor above the reorg root.
/// After `delete_above_block(root)`, the durable cursor
/// (`MAX(last_indexed_block) WHERE error IS NULL`) must resolve to `root`, even
/// though a *separate* failed run row still records the (pre-reorg) cursor.
#[tokio::test]
async fn delete_above_block_caps_successful_run_cursor_to_root() {
    let fx = pg_fixture().await;
    let db = &fx.db;
    db.upsert_chain(CHAIN, "base", "stub").await.unwrap();

    // A prior successful run advanced the cursor to block 105.
    let run_ok = db.start_run(CHAIN, 100).await.unwrap();
    db.finish_run(run_ok, Some(105), Some(105), 0, 0, None)
        .await
        .unwrap();
    assert_eq!(db.last_indexed_block(CHAIN).await.unwrap(), Some(105));

    // A later run rolls a reorg back to root=100, then fails — recording the
    // pre-reorg cursor on its (error IS NOT NULL) row. This must NOT keep the
    // durable cursor above root.
    let run_fail = db.start_run(CHAIN, 106).await.unwrap();
    db.delete_above_block(CHAIN, 100).await.unwrap();
    db.finish_run(run_fail, None, Some(105), 0, 0, Some("rpc outage"))
        .await
        .unwrap();

    // The durable cursor now reflects the rollback root, so the next run
    // resumes from root + 1 = 101.
    assert_eq!(
        db.last_indexed_block(CHAIN).await.unwrap(),
        Some(100),
        "successful-run cursor must be capped to the reorg root"
    );
}

/// IDX-2 (db layer): a full wipe (`root = -1`) must drop the cursor to NULL so
/// the next run re-indexes from block 0.
#[tokio::test]
async fn delete_above_block_full_wipe_clears_cursor() {
    let fx = pg_fixture().await;
    let db = &fx.db;
    db.upsert_chain(CHAIN, "base", "stub").await.unwrap();

    let run_ok = db.start_run(CHAIN, 0).await.unwrap();
    db.finish_run(run_ok, Some(50), Some(50), 0, 0, None)
        .await
        .unwrap();
    assert_eq!(db.last_indexed_block(CHAIN).await.unwrap(), Some(50));

    db.delete_above_block(CHAIN, -1).await.unwrap();

    assert_eq!(
        db.last_indexed_block(CHAIN).await.unwrap(),
        None,
        "full wipe (root=-1) must clear the durable cursor"
    );
}

/// IDX-2 (end-to-end): a reorg rollback followed by a same-run RPC failure must
/// leave the next run resuming at root + 1 with the previously-deleted blocks
/// re-indexed (no silent gap).
///
/// Tick 1 indexes cursor block 105. Tick 2 sees a reorged cursor hash → walks
/// back past 0 (root = -1, full wipe) → `delete_above_block(-1)` → then
/// `eth_blockNumber` is unset so `block_number()` errors *after* the rollback.
/// The next run must restart from block 0 and re-index, not resume above the
/// wiped blocks.
#[tokio::test]
async fn run_once_reorg_then_failure_resumes_from_root() {
    let fx = pg_fixture().await;

    let cfg = IndexerConfig {
        chain_id: CHAIN,
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

    // ── Tick 1: index cursor block 105 (hash 0xcc…). ──
    let pre = StubRpcServer::start().await;
    pre.set("eth_blockNumber", serde_json::json!("0x6e")); // 110
    pre.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    pre.set(
        "eth_call",
        serde_json::json!(format!("0x{}", "00".repeat(32))),
    );
    pre.set(
        "eth_getBlockByNumber",
        stub_block(
            105,
            &format!("0x{}", "cc".repeat(32)),
            &format!("0x{}", "bb".repeat(32)),
            1_700_000_000,
        ),
    );
    let rpc1 = JsonRpc::new(&pre.url);
    let o1 = run_once(&fx.db, &rpc1, &cfg).await.unwrap();
    assert!(o1.error.is_none(), "tick 1 must succeed: {:?}", o1.error);
    assert_eq!(o1.last_indexed_block, Some(105));
    // Seed a deletable row above the wipe root to prove re-index.
    fx.db
        .upsert_contract(CHAIN, [0xaau8; 20], "gateway", None)
        .await
        .unwrap();
    fx.db
        .insert_block(CHAIN, 100, [0xa1u8; 32], [0xa0u8; 32], 1_699_000_000)
        .await
        .unwrap();
    pre.shutdown();

    // ── Tick 2: reorged cursor hash + eth_blockNumber unset → fail after rollback. ──
    let post = StubRpcServer::start().await;
    // NOTE: eth_blockNumber intentionally NOT set → null result → decode error,
    // raised by block_number() which runs *after* delete_above_block.
    post.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    post.set(
        "eth_call",
        serde_json::json!(format!("0x{}", "00".repeat(32))),
    );
    post.set(
        "eth_getBlockByNumber",
        stub_block(
            105,
            &format!("0x{}", "dd".repeat(32)), // new hash ≠ stored 0xcc…
            &format!("0x{}", "ee".repeat(32)),
            1_700_000_100,
        ),
    );
    let rpc2 = JsonRpc::new(&post.url);
    let o2 = run_once(&fx.db, &rpc2, &cfg).await.unwrap();
    assert!(
        o2.error.is_some(),
        "tick 2 must fail after the rollback (block_number errors)"
    );
    // The rolled-back blocks are gone …
    assert_eq!(
        fx.db.get_block_hash(CHAIN, 100).await.unwrap(),
        None,
        "block 100 must be deleted by the reorg rollback"
    );
    // … and the durable cursor dropped to the wipe root (root=-1 → None), so the
    // next run resumes from block 0 rather than above the wiped blocks.
    assert_eq!(
        fx.db.last_indexed_block(CHAIN).await.unwrap(),
        None,
        "durable cursor must reflect the rollback, not the pre-reorg cursor"
    );
    post.shutdown();

    // ── Tick 3: a healthy chain re-indexes from block 0 (no gap). ──
    let heal = StubRpcServer::start().await;
    heal.set("eth_blockNumber", serde_json::json!("0x6e")); // 110
    heal.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    heal.set(
        "eth_call",
        serde_json::json!(format!("0x{}", "00".repeat(32))),
    );
    heal.set(
        "eth_getBlockByNumber",
        stub_block(
            105,
            &format!("0x{}", "dd".repeat(32)),
            &format!("0x{}", "ee".repeat(32)),
            1_700_000_200,
        ),
    );
    let rpc3 = JsonRpc::new(&heal.url);
    let o3 = run_once(&fx.db, &rpc3, &cfg).await.unwrap();
    assert!(o3.error.is_none(), "tick 3 must succeed: {:?}", o3.error);
    assert_eq!(
        o3.from_block, 0,
        "next run must resume from root + 1 = 0, re-indexing the wiped range"
    );
    assert_eq!(o3.last_indexed_block, Some(105));
    heal.shutdown();
}

// ─── IDX-8 ───────────────────────────────────────────────────────────────────

/// Register a vault at `registered_block` with status 0 (Active).
async fn seed_vault(db: &explorer_indexer::Db, vault: [u8; 20], registered_block: i64) {
    db.upsert_chain(CHAIN, "base", "stub").await.unwrap();
    db.upsert_vault(
        CHAIN,
        vault,
        "Test Vault",
        "balanced",
        alloy_primitives::U256::ZERO,
        0, // Active
        registered_block,
        registered_block,
        [0x01u8; 32],
    )
    .await
    .unwrap();
}

/// Read the live status of a vault.
async fn vault_status(db: &explorer_indexer::Db, vault: [u8; 20]) -> i16 {
    let row: (i16,) =
        sqlx::query_as("SELECT status FROM vaults WHERE chain_id = $1 AND vault_address = $2")
            .bind(CHAIN)
            .bind(&vault[..])
            .fetch_one(db.pool())
            .await
            .unwrap();
    row.0
}

/// IDX-8: a status change written above `root` is reverted by
/// `delete_above_block(root)` — both the live `vaults.status` and the history
/// event roll back.
#[tokio::test]
async fn delete_above_block_reverts_vault_status_above_root() {
    let fx = pg_fixture().await;
    let db = &fx.db;
    let vault = [0x11u8; 20];
    seed_vault(db, vault, 50).await;

    // Block 60: Active → Paused (status 1). Block 70: Paused → Retired (2).
    db.update_vault_status(CHAIN, vault, 60, 0, 1, 1_700_000_060)
        .await
        .unwrap();
    db.update_vault_status(CHAIN, vault, 70, 0, 2, 1_700_000_070)
        .await
        .unwrap();
    assert_eq!(vault_status(db, vault).await, 2, "live status is Retired");
    assert_eq!(db.count(CountTable::VaultStatusEvents).await.unwrap(), 2);

    // A reorg rolls back to root=65 → the Retired transition at block 70 is
    // orphaned. Status must revert to Paused (the surviving event at block 60).
    db.delete_above_block(CHAIN, 65).await.unwrap();
    assert_eq!(
        vault_status(db, vault).await,
        1,
        "status must revert to the surviving (block 60) Paused event"
    );
    assert_eq!(
        db.count(CountTable::VaultStatusEvents).await.unwrap(),
        1,
        "the orphaned block-70 status event must be pruned"
    );

    // A deeper reorg (root=55) orphans the block-60 event too → status reverts
    // to the registration default (Active = 0) and history is empty.
    db.delete_above_block(CHAIN, 55).await.unwrap();
    assert_eq!(
        vault_status(db, vault).await,
        0,
        "status must revert to the registration default (Active) when no event survives"
    );
    assert_eq!(db.count(CountTable::VaultStatusEvents).await.unwrap(), 0);
}

/// IDX-8: a vault whose status never changed is untouched by a rollback (the
/// re-derivation must not clobber an unchanged Active vault).
#[tokio::test]
async fn delete_above_block_leaves_unchanged_vault_status() {
    let fx = pg_fixture().await;
    let db = &fx.db;
    let vault = [0x22u8; 20];
    seed_vault(db, vault, 10).await;

    db.delete_above_block(CHAIN, 5).await.unwrap();
    assert_eq!(
        vault_status(db, vault).await,
        0,
        "an unchanged vault must remain Active after a rollback"
    );
}

// ─── #1283: block-scoped rollback coverage ───────────────────────────────────
//
// The rollback used to iterate a literal list of table names. Migration 0014
// added `committee_votes` and `regime_snapshots` and nobody edited the literal,
// so a reorg past a `VoteSubmitted` left both tables serving orphaned rows as
// current state — the exact drift `src/schema.rs` had predicted in a comment.
// The fix derives the set from the live schema; these tests assert the derived
// set is right, that it stays right when a migration adds a table, and that its
// deletion order respects the live foreign keys.

/// Every table the live schema marks block-scoped (`block_number` column on a
/// base table), derived independently of `Db` so the assertions below compare
/// two derivations rather than one derivation against a copy of itself.
async fn live_block_scoped_tables(pool: &PgPool) -> BTreeSet<String> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT c.table_name::text \
         FROM information_schema.columns c \
         JOIN information_schema.tables t \
           ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
         WHERE c.table_schema = current_schema() \
           AND t.table_type = 'BASE TABLE' \
           AND c.column_name = 'block_number'",
    )
    .fetch_all(pool)
    .await
    .expect("catalog query");
    rows.into_iter().map(|(t,)| t).collect()
}

async fn raw_pool(fx: &common::PgFixture) -> PgPool {
    PgPool::connect(&fx.url).await.expect("raw pool")
}

async fn row_count(pool: &PgPool, table: &str) -> i64 {
    let q = format!("SELECT COUNT(*)::BIGINT FROM \"{table}\"");
    let (n,): (i64,) = sqlx::query_as(&q).fetch_one(pool).await.expect("count");
    n
}

/// Seed a committee agent (not block-scoped — it survives the rollback and
/// keeps the `committee_votes` foreign key satisfiable) and one vote plus one
/// regime snapshot at `block`.
async fn seed_committee_activity(
    db: &explorer_indexer::Db,
    agent: [u8; 20],
    vault: [u8; 20],
    block: i64,
    vote_id: i64,
) {
    db.upsert_committee_agent(CHAIN, agent, "agent-1283", 1)
        .await
        .unwrap();
    db.insert_committee_vote(
        CHAIN,
        vote_id,
        block,
        0,
        [0xAAu8; 32],
        agent,
        vault,
        0,
        7_500,
        90,
        "ipfs://memo",
        [0xBBu8; 32],
        1_700_000_000,
        true,
    )
    .await
    .unwrap();
    db.upsert_regime_snapshot(CHAIN, vault, block, 7_500.0, 1)
        .await
        .unwrap();
}

/// AC1 — the shipped defect. A reorg past a `VoteSubmitted` must clear both
/// tables migration 0014 added. Red before the fix: the hard-coded list did not
/// name `committee_votes` or `regime_snapshots`, so both rows survived and the
/// committee / regime-feed endpoints kept serving them as current state.
#[tokio::test]
async fn delete_above_block_clears_committee_votes_and_regime_snapshots() {
    let fx = pg_fixture().await;
    let db = &fx.db;
    db.upsert_chain(CHAIN, "base", "stub").await.unwrap();

    let agent = [0x51u8; 20];
    let vault_a = [0x61u8; 20];
    let vault_b = [0x62u8; 20];

    // One vote + snapshot below the reorg root (must survive) and one above it
    // (must be rolled back).
    seed_committee_activity(db, agent, vault_a, 100, 1).await;
    seed_committee_activity(db, agent, vault_b, 105, 2).await;
    assert_eq!(db.count(CountTable::CommitteeVotes).await.unwrap(), 2);
    assert_eq!(db.count(CountTable::RegimeSnapshots).await.unwrap(), 2);

    db.delete_above_block(CHAIN, 100).await.unwrap();

    assert_eq!(
        db.count(CountTable::CommitteeVotes).await.unwrap(),
        1,
        "the vote at block 105 must be rolled back by a reorg to root=100"
    );
    assert_eq!(
        db.count(CountTable::RegimeSnapshots).await.unwrap(),
        1,
        "the regime snapshot at block 105 must be rolled back by a reorg to root=100"
    );

    // A deeper reorg orphans everything: both tables must be empty.
    db.delete_above_block(CHAIN, 99).await.unwrap();
    assert_eq!(
        db.count(CountTable::CommitteeVotes).await.unwrap(),
        0,
        "no committee vote may survive a reorg below its block"
    );
    assert_eq!(
        db.count(CountTable::RegimeSnapshots).await.unwrap(),
        0,
        "no regime snapshot may survive a reorg below its block"
    );
    // The agent registry is not block-scoped and is intentionally untouched.
    assert_eq!(db.count(CountTable::CommitteeAgents).await.unwrap(), 1);
}

/// AC2 (part 1) — the rollback set is the live schema's block-scoped set minus
/// the documented exclusions. Nothing is covered by being remembered.
#[tokio::test]
async fn rollback_set_equals_live_block_scoped_schema() {
    let fx = pg_fixture().await;
    let pool = raw_pool(&fx).await;

    let live = live_block_scoped_tables(&pool).await;
    let rollback: BTreeSet<String> = fx
        .db
        .block_scoped_rollback_tables()
        .await
        .unwrap()
        .into_iter()
        .collect();
    let excluded: BTreeSet<String> = REORG_ROLLBACK_EXCLUSIONS
        .iter()
        .map(|(t, _)| (*t).to_string())
        .collect();

    let uncovered: Vec<&String> = live
        .iter()
        .filter(|t| !rollback.contains(*t) && !excluded.contains(*t))
        .collect();
    assert!(
        uncovered.is_empty(),
        "block-scoped tables neither rolled back nor on the documented exclusion \
         list: {uncovered:?}. Add them to the rollback, or add an entry with a \
         reason to db::REORG_ROLLBACK_EXCLUSIONS."
    );

    let phantom: Vec<&String> = rollback.iter().filter(|t| !live.contains(*t)).collect();
    assert!(
        phantom.is_empty(),
        "rollback names tables the live schema does not mark block-scoped: {phantom:?}"
    );

    // The two tables issue #1283 is about, named explicitly so a regression that
    // re-hard-codes the list cannot pass this file.
    for required in ["committee_votes", "regime_snapshots"] {
        assert!(
            rollback.contains(required),
            "{required} (migration 0014) must be rolled back by a reorg"
        );
    }
    // Every table the pre-#1283 literal named must still be covered.
    for required in [
        "wallet_positions",
        "vault_snapshots",
        "vault_status_events",
        "agent_deposits",
        "agent_policies",
        "governance_votes",
        "governance_proposals",
        "router_weight_snapshots",
        "router_deposit_legs",
        "adapter_allocations",
        "vault_fee_events",
        "vault_transfer_events",
        "account_history_events",
        "consensus_receipts",
        "transactions",
        "blocks",
    ] {
        assert!(
            rollback.contains(required),
            "{required} was covered before #1283 and must still be covered"
        );
    }
}

/// AC2 (part 2) — the negative self-test. A synthetic block-scoped table stands
/// in for "the next migration": it is created after the crate was compiled, so
/// no literal in the source can name it. The rollback must pick it up from the
/// schema and clear its orphaned rows.
///
/// This is the assertion that fails against the pre-#1283 implementation, and
/// it fails for the same reason `committee_votes` was missed.
#[tokio::test]
async fn rollback_covers_a_block_scoped_table_added_after_compile_time() {
    let fx = pg_fixture().await;
    let pool = raw_pool(&fx).await;
    fx.db.upsert_chain(CHAIN, "base", "stub").await.unwrap();

    sqlx::query(
        "CREATE TABLE synthetic_block_scoped_probe ( \
             chain_id     BIGINT NOT NULL, \
             block_number BIGINT NOT NULL, \
             PRIMARY KEY (chain_id, block_number) \
         )",
    )
    .execute(&pool)
    .await
    .expect("create synthetic table");
    sqlx::query("INSERT INTO synthetic_block_scoped_probe (chain_id, block_number) VALUES ($1, 100), ($1, 105)")
        .bind(CHAIN)
        .execute(&pool)
        .await
        .expect("seed synthetic rows");

    let rollback: BTreeSet<String> = fx
        .db
        .block_scoped_rollback_tables()
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert!(
        rollback.contains("synthetic_block_scoped_probe"),
        "a block-scoped table added by a migration must enter the rollback set \
         without anyone editing a list in db.rs"
    );

    fx.db.delete_above_block(CHAIN, 100).await.unwrap();
    assert_eq!(
        row_count(&pool, "synthetic_block_scoped_probe").await,
        1,
        "the row above the reorg root must be deleted from a newly added \
         block-scoped table"
    );

    fx.db.delete_above_block(CHAIN, 99).await.unwrap();
    assert_eq!(
        row_count(&pool, "synthetic_block_scoped_probe").await,
        0,
        "a deeper reorg must clear the newly added block-scoped table entirely"
    );
}

/// Validate one exclusion table against the live schema. Returned as a `Result`
/// rather than asserted inline so the check itself can be exercised on inputs
/// the real (currently empty) list does not contain — otherwise AC3 would be
/// enforced by a rule no test ever ran.
fn check_exclusion(
    entry: (&str, &str),
    live: &BTreeSet<String>,
    rollback: &BTreeSet<String>,
) -> Result<(), String> {
    let (table, reason) = entry;
    if reason.trim().len() < 10 {
        return Err(format!(
            "exclusion {table:?} has no stated reason; every exemption from reorg \
             rollback must say why those rows legitimately survive an orphaned block"
        ));
    }
    if !live.contains(table) {
        return Err(format!(
            "exclusion {table:?} is stale: the live schema has no block-scoped \
             table by that name"
        ));
    }
    if rollback.contains(table) {
        return Err(format!(
            "exclusion {table:?} is contradicted: the rollback covers it anyway"
        ));
    }
    Ok(())
}

/// AC3 — every exclusion carries a reason and matches a live block-scoped table.
/// The list is empty today, so the test also runs the check on synthetic bad
/// entries: an empty list must not make this a rule that has never executed.
#[tokio::test]
async fn reorg_rollback_exclusions_are_documented_and_live() {
    let fx = pg_fixture().await;
    let pool = raw_pool(&fx).await;
    let live = live_block_scoped_tables(&pool).await;
    let rollback: BTreeSet<String> = fx
        .db
        .block_scoped_rollback_tables()
        .await
        .unwrap()
        .into_iter()
        .collect();

    for entry in REORG_ROLLBACK_EXCLUSIONS {
        if let Err(e) = check_exclusion(*entry, &live, &rollback) {
            panic!("db::REORG_ROLLBACK_EXCLUSIONS: {e}");
        }
    }

    // The rule itself, exercised. An undocumented exemption is rejected...
    assert!(
        check_exclusion(("committee_votes", ""), &live, &rollback).is_err(),
        "an exclusion with no stated reason must be rejected"
    );
    // ...as is one naming a table the live schema does not have...
    assert!(
        check_exclusion(
            ("table_that_does_not_exist", "kept for a stated reason"),
            &live,
            &rollback
        )
        .is_err(),
        "a stale exclusion naming a non-existent table must be rejected"
    );
    // ...and one that contradicts what the rollback actually does.
    assert!(
        check_exclusion(
            ("committee_votes", "kept for a stated reason"),
            &live,
            &rollback
        )
        .is_err(),
        "an exclusion for a table the rollback still covers must be rejected"
    );
}

/// The derived deletion order must respect the live foreign keys.
/// `governance_votes` references `governance_proposals` and both are
/// block-scoped: deleting the proposals first raises a foreign-key violation.
/// A set derived in name order would do exactly that, so this asserts the
/// FK-aware ordering, not just the membership.
#[tokio::test]
async fn rollback_order_deletes_children_before_parents() {
    let fx = pg_fixture().await;
    let db = &fx.db;
    db.upsert_chain(CHAIN, "base", "stub").await.unwrap();

    db.insert_proposal(
        CHAIN,
        1,
        105,
        0,
        [0xC1u8; 32],
        [0x71u8; 20],
        "orphaned proposal",
        1_700_000_000,
        200,
    )
    .await
    .unwrap();
    db.insert_vote(
        CHAIN,
        1,
        [0x72u8; 20],
        106,
        0,
        [0xC2u8; 32],
        true,
        U256::from(42u64),
    )
    .await
    .unwrap();

    let order = db.block_scoped_rollback_tables().await.unwrap();
    let pos = |name: &str| {
        order
            .iter()
            .position(|t| t == name)
            .unwrap_or_else(|| panic!("{name} missing from rollback order: {order:?}"))
    };
    assert!(
        pos("governance_votes") < pos("governance_proposals"),
        "governance_votes references governance_proposals, so it must be deleted \
         first; order was {order:?}"
    );

    // The rollback itself must therefore succeed rather than raise 23503.
    db.delete_above_block(CHAIN, 100)
        .await
        .expect("rollback must not violate a foreign key");
    assert_eq!(db.count(CountTable::GovernanceVotes).await.unwrap(), 0);
    assert_eq!(db.count(CountTable::GovernanceProposals).await.unwrap(), 0);
}
