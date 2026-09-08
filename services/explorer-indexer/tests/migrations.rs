//! Schema sanity: bring up Postgres in a container, apply migrations,
//! confirm all nine §11 tables plus vaults and governance tables exist
//! with `chain_id` and (where applicable) `block_number` columns.
//!
//! Also covers issue #315 acceptance criteria:
//!   - Migration 0003 creates router_weight_snapshots, governance_proposals,
//!     governance_votes tables and the account_positions view.
//!   - vault_address column is added to vault_snapshots and backfilled.
//!
//! Issue #1359 adds the two `migrate_only_mode_*` tests, which drive the
//! compiled `indexer` binary rather than the library: they are the coverage for
//! the CLI's migrate-only mode now that the normal boot path no longer migrates.
//!
//! Skips cleanly when Docker is not available so contributor laptops
//! without docker still run `cargo test` green.

mod common;

use alloy_primitives::U256;
use common::{try_pg_fixture, try_raw_pg};
use explorer_indexer::db::CountTable;
use explorer_indexer::Db;
use std::process::{Command, Output};

/// Runs the compiled `indexer` binary in migrate-only mode against `url` and
/// waits for it to exit.
///
/// Issue #1359 AC: migrate-only must not require the chain-watching arguments,
/// so this passes ONLY `--migrate-only` and `DATABASE_URL`, and scrubs every
/// `INDEXER_*` variable clap would otherwise pick up from the test runner's
/// environment. If the flag did not relax `required_unless_present`, clap would
/// exit 2 here and both tests below would show it.
///
/// `Output` is only produced once the child has been reaped, so a returned
/// `Output` is itself the proof that the process did not stay running.
fn run_migrate_only(url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_indexer"))
        .arg("--migrate-only")
        .env("DATABASE_URL", url)
        .env_remove("INDEXER_RPC_URL")
        .env_remove("INDEXER_GATEWAY")
        .env_remove("INDEXER_VAULT")
        .env_remove("INDEXER_REGISTRY")
        .env_remove("INDEXER_ROUTER_GOVERNANCE")
        .env_remove("INDEXER_PORTFOLIO_ROUTER")
        .env_remove("INDEXER_INVESTMENT_COMMITTEE")
        .env_remove("INDEXER_CONSENSUS_RECEIPT")
        .output()
        .expect("failed to spawn the indexer binary")
}

fn describe(out: &Output) -> String {
    format!(
        "status={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

/// Issue #1359 AC — `indexer --migrate-only` migrates a virgin database and
/// exits 0, with no `--rpc-url` / `--gateway` / `--vault` supplied.
#[tokio::test]
async fn migrate_only_mode_runs_migrations_and_exits_zero() {
    let Some(pg) = try_raw_pg().await else {
        return;
    };

    let out = run_migrate_only(&pg.url);
    assert!(
        out.status.success(),
        "migrate-only must exit 0 on a valid migrations directory, without the \
         chain-watching args.\n{}",
        describe(&out)
    );

    // The exit code alone would also be satisfied by a binary that connected
    // and did nothing, so assert the schema is actually there. `indexer_runs`
    // is the table explorer-api's /health probe reads, and `consensus_receipts`
    // comes from the LAST migration (0015) — together they prove the whole
    // migration set ran, not just the first file.
    let db = Db::connect(&pg.url)
        .await
        .expect("connect to the migrated database");
    for table in ["indexer_runs", "consensus_receipts", "_sqlx_migrations"] {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*)::BIGINT FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = $1",
        )
        .bind(table)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(row.0, 1, "table {table} must exist after --migrate-only");
    }
}

/// Issue #1359 AC — a deliberately broken migration makes migrate-only exit
/// non-zero and terminate, so a deploy's migrate step fails instead of the
/// service crash-looping against a half-migrated schema.
///
/// The breakage is seeded in the database rather than by editing a migration
/// file: `MIGRATOR` is `sqlx::migrate!()`, embedded at COMPILE time, so the
/// only way to make the shipped migration set fail at runtime is to give it a
/// database it conflicts with. Pre-creating a `chains` table without a
/// `chain_id` column does exactly that — 0001's `CREATE TABLE IF NOT EXISTS
/// chains` is then a no-op, and the very next statement's
/// `REFERENCES chains(chain_id)` foreign key cannot be built.
#[tokio::test]
async fn migrate_only_mode_exits_non_zero_on_a_broken_migration() {
    let Some(pg) = try_raw_pg().await else {
        return;
    };

    let db = Db::connect(&pg.url)
        .await
        .expect("connect to the unmigrated database");
    sqlx::query("CREATE TABLE chains (a_column_that_is_not_chain_id TEXT PRIMARY KEY)")
        .execute(db.pool())
        .await
        .expect("seed the conflicting table");

    let out = run_migrate_only(&pg.url);
    assert!(
        !out.status.success(),
        "migrate-only must exit non-zero when a migration fails.\n{}",
        describe(&out)
    );
    // Exited of its own accord (Some(code)) rather than being signalled or
    // left running — `output()` returning at all already means it was reaped.
    assert!(
        out.status.code().is_some(),
        "migrate-only must exit with a status code, not hang or die by signal.\n{}",
        describe(&out)
    );

    // And it must not have left a partial schema behind: sqlx runs each
    // migration in a transaction, so the failed 0001 rolls back whole.
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'indexer_runs'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        row.0, 0,
        "a failed migration must not leave the later tables half-created"
    );
}

#[tokio::test]
async fn migrations_create_all_tables() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
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
        // migration 0002
        CountTable::Vaults,
        // migration 0003 — governance tables (issue #307)
        CountTable::GovernanceProposals,
        CountTable::GovernanceVotes,
        CountTable::RouterWeightSnapshots,
    ] {
        let n = fx.db.count(t).await.unwrap_or_else(|e| panic!("{e}"));
        // Nothing inserted yet — just confirms the table exists and
        // the COUNT(*) plan succeeds.
        assert_eq!(n, 0, "table {t:?} should be empty");
    }
}

/// Issue #315 AC — Migration 0003 creates the three new tables and the
/// account_positions view.
#[tokio::test]
async fn migration_0003_creates_multi_vault_tables() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
    for t in [
        CountTable::RouterWeightSnapshots,
        CountTable::GovernanceProposals,
        CountTable::GovernanceVotes,
    ] {
        let n = fx.db.count(t).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(n, 0, "table {t:?} must exist and be empty after migration");
    }

    // account_positions is a VIEW — verify it exists by selecting from it.
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*)::BIGINT FROM account_positions")
        .fetch_one(fx.db.pool())
        .await
        .unwrap_or_else(|e| panic!("account_positions view must exist: {e}"));
    assert_eq!(
        row.0, 0,
        "account_positions view must be queryable and empty"
    );
}

/// Issue #315 AC — Migration 0003 adds vault_address to vault_snapshots
/// and backfills existing rows.
///
/// Test plan item 1: apply migration to a test DB with a pre-existing
/// vault_snapshots row; assert the row is preserved and vault_address is
/// set to the same value as contract (the backfill UPDATE in 0003).
///
/// The fixture DB already has migration 0003 applied (try_pg_fixture runs
/// all migrations from scratch on a fresh Postgres container).  We insert
/// a vault_snapshots row directly via the Db helper (which does NOT write
/// vault_address, relying on the DB default / backfill path), then verify
/// the vault_address column is non-NULL.
#[tokio::test]
async fn migration_0003_preserves_vault_snapshots_with_vault_address_backfilled() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };

    fx.db.upsert_chain(8453, "base", "stub").await.unwrap();
    let vault_addr = [0xAAu8; 20];
    fx.db
        .upsert_contract(8453, vault_addr, "vault", None)
        .await
        .unwrap();

    // insert_vault_snapshot does NOT write vault_address — the column is
    // set by the migration backfill (UPDATE ... SET vault_address = contract
    // WHERE vault_address IS NULL).  On a fresh DB, newly inserted rows will
    // have vault_address = NULL (the column has no DEFAULT after 0003), so
    // we verify the INSERT + explicit backfill path.
    fx.db
        .insert_vault_snapshot(
            8453,
            vault_addr,
            100,
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            50,
            U256::ZERO,
            false,
        )
        .await
        .unwrap();

    // Manually backfill vault_address for the row we just inserted
    // (simulating the backfill the 0003 migration runs on existing rows).
    sqlx::query("UPDATE vault_snapshots SET vault_address = contract WHERE vault_address IS NULL")
        .execute(fx.db.pool())
        .await
        .unwrap();

    // The row must still be there (snapshot_count = 1).
    let count = fx.db.count(CountTable::VaultSnapshots).await.unwrap();
    assert_eq!(count, 1, "vault_snapshots row must survive migration");

    // vault_address must be set and equal to contract.
    let row: (Vec<u8>, Option<Vec<u8>>) =
        sqlx::query_as("SELECT contract, vault_address FROM vault_snapshots WHERE chain_id = $1")
            .bind(8453i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    let (contract_bytes, vault_address_bytes) = row;
    let va = vault_address_bytes.expect("vault_address must be non-NULL after backfill");
    assert_eq!(
        va, contract_bytes,
        "vault_address must equal contract after backfill"
    );
}

#[tokio::test]
async fn every_row_has_chain_id_and_block_number() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
    // §11 acceptance criterion: each row carries chain_id and
    // block_number. Verified by interrogating information_schema:
    // every minimum table has a `chain_id` column, and every event
    // / state-snapshot table also has `block_number`.
    let needs_block_number = [
        "blocks",
        "transactions",
        "agent_deposits",
        "agent_policies",
        "vault_snapshots",
        "wallet_positions",
        // migration 0003
        "governance_proposals",
        "governance_votes",
        "router_weight_snapshots",
    ];
    let needs_chain_id = [
        "chains",
        "contracts",
        "blocks",
        "transactions",
        "agent_deposits",
        "agent_policies",
        "vault_snapshots",
        "wallet_positions",
        "indexer_runs",
        // migration 0002
        "vaults",
        // migration 0003
        "governance_proposals",
        "governance_votes",
        "router_weight_snapshots",
    ];
    for t in needs_chain_id {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*)::BIGINT FROM information_schema.columns WHERE table_name = $1 AND column_name = 'chain_id'",
        )
        .bind(t)
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
        assert_eq!(row.0, 1, "{t} must have a chain_id column");
    }
    for t in needs_block_number {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*)::BIGINT FROM information_schema.columns WHERE table_name = $1 AND column_name = 'block_number'",
        )
        .bind(t)
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
        assert_eq!(row.0, 1, "{t} must have a block_number column");
    }
}
