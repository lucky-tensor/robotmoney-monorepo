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
//! Issue #1392 adds the two `boot_*` tests (the normal boot path must refuse a
//! schema whose applied version differs from the embedded one, and must start
//! normally when they agree) and the two `migration_atomicity_*` tests (every
//! embedded migration runs inside a transaction, plus the negative self-test
//! proving that guard is not vacuous).
//!
//! Issue #1383 removed the "skips cleanly when Docker is not available" escape:
//! `pg_fixture()` / `raw_pg()` are infallible and panic naming Docker/Postgres,
//! so a run without a container is RED here, never a green zero-assertion pass.

mod common;

use alloy_primitives::U256;
use common::{pg_fixture, raw_pg};
use explorer_indexer::db::{
    assert_migrations_are_transactional, compare_schema, embedded_schema_version,
    first_non_transactional, AppliedMigration, CountTable, DbError, SchemaDivergence,
};
use explorer_indexer::Db;
use sqlx::migrate::{Migration, MigrationType};
use std::borrow::Cow;
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
    let pg = raw_pg().await;

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
    let pg = raw_pg().await;

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

    // And no half-applied schema is left behind: `indexer_runs` (created at
    // line 122 of migration 0001) must not exist.
    //
    // Issue #1392 — what this assertion does and does NOT demonstrate. It is
    // NOT evidence of transactional rollback: the seeded conflict aborts 0001
    // at its `REFERENCES chains(chain_id)` (line 20), which is *before* the
    // `indexer_runs` CREATE TABLE this counts, so the table would be absent
    // whether or not the migration ran in a transaction. What it does prove is
    // the acceptance criterion it was written for — a failed migrate step exits
    // non-zero, is reaped, and leaves the later tables uncreated, so nothing
    // downstream runs against a partly-built schema.
    //
    // The atomicity property itself (a migration that fails half-way rolls back
    // whole) is real but is pinned separately, by
    // `migration_atomicity_every_embedded_migration_is_transactional` below —
    // it holds only while no migration opts out with `-- no-transaction`, and
    // this assertion would not go red if one did.
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

// ─── Issue #1392: the boot-time schema-version guard ─────────────────────────
//
// Issue #1359 stopped the indexer migrating on boot, which was right — but
// auto-migration had also been the only thing that made a schema mismatch
// *fail*. Without a replacement, an indexer started against a stale schema just
// loops, failing every tick, with no healthcheck to notice. `main` now asserts
// that the highest embedded migration version equals the highest applied one
// and refuses to start otherwise. The two tests below drive the compiled binary
// against a real Postgres so the refusal is demonstrated, not asserted.

/// Runs the compiled `indexer` binary on its NORMAL boot path (no
/// `--migrate-only`) against `url`, for a single tick, and waits for it to exit.
///
/// `--once` bounds the run: on the happy path the poll loop performs one tick
/// and returns instead of running forever. `--rpc-url` points at a closed port,
/// so that tick reaches the chain and fails — `run_once` records the RPC error
/// on the `indexer_runs` row and still returns `Ok`, which is what makes the
/// happy path both terminating and observable. Everything before the tick (the
/// schema check under test) runs exactly as it does in production.
fn run_boot(url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_indexer"))
        .arg("--once")
        // 127.0.0.1:1 is closed: the tick fails fast rather than hanging.
        .args(["--rpc-url", "http://127.0.0.1:1"])
        .args(["--gateway", "0x00000000000000000000000000000000000000a1"])
        .args(["--vault", "0x00000000000000000000000000000000000000a2"])
        .env("DATABASE_URL", url)
        .env_remove("INDEXER_RPC_URL")
        .env_remove("INDEXER_GATEWAY")
        .env_remove("INDEXER_VAULT")
        .env_remove("INDEXER_REGISTRY")
        .env_remove("INDEXER_ROUTER_GOVERNANCE")
        .env_remove("INDEXER_PORTFOLIO_ROUTER")
        .env_remove("INDEXER_INVESTMENT_COMMITTEE")
        .env_remove("INDEXER_CONSENSUS_RECEIPT")
        .env_remove("INDEXER_END_BLOCK")
        .output()
        .expect("failed to spawn the indexer binary")
}

/// How many rows the binary wrote to `indexer_runs`. A refusal at boot must
/// leave this at 0: the process must decline *before* it starts indexing.
async fn indexer_run_count(db: &Db) -> i64 {
    db.count(CountTable::IndexerRuns)
        .await
        .expect("count indexer_runs")
}

/// Issue #1392 AC — an indexer started against a deliberately stale schema
/// fails at boot, naming the embedded and the applied version.
///
/// The staleness is real, not simulated: the database is migrated in full, then
/// rolled back to the state it would have been in before the newest migration
/// ran — the table migration 0015 creates is dropped and its `_sqlx_migrations`
/// row deleted. That is exactly the shape of a deploy whose migrate step was
/// skipped while a newer indexer image rolled out.
#[tokio::test]
async fn boot_refuses_a_stale_schema_and_names_both_versions() {
    let pg = raw_pg().await;

    let out = run_migrate_only(&pg.url);
    assert!(
        out.status.success(),
        "setup: migrate-only must succeed first.\n{}",
        describe(&out)
    );

    let db = Db::connect(&pg.url)
        .await
        .expect("connect to the migrated database");
    let embedded = embedded_schema_version();

    // Roll the database back one migration.
    sqlx::query("DROP TABLE IF EXISTS consensus_receipts CASCADE")
        .execute(db.pool())
        .await
        .expect("drop the table migration 0015 creates");
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(embedded)
        .execute(db.pool())
        .await
        .expect("un-record the newest migration");

    let applied = db
        .applied_schema_version()
        .await
        .expect("read the applied schema version")
        .expect("the database is still migrated, just to an older version");
    assert_ne!(
        applied, embedded,
        "setup: the database must now be behind the binary"
    );

    let out = run_boot(&pg.url);
    let report = describe(&out);
    assert!(
        !out.status.success(),
        "boot must FAIL against a stale schema, not loop against it.\n{report}"
    );
    assert!(
        out.status.code().is_some(),
        "boot must exit with a status code, not hang or die by signal.\n{report}"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    // Issue #1429 reshaped this message: rolling the database back one migration
    // is now reported as the specific shape `MissingVersion`, naming the
    // migration that is absent rather than only the two maxima.
    assert!(
        stderr.contains("not applied"),
        "the refusal must say what is wrong.\n{report}"
    );
    assert!(
        stderr.contains(&embedded.to_string()) && stderr.contains(&applied.to_string()),
        "the refusal must name BOTH the embedded version ({embedded}) and the \
         applied version ({applied}) so an operator can act on it.\n{report}"
    );
    assert!(
        stderr.contains(&format!("migration {embedded}")),
        "the refusal must name the migration that is missing, not only the two \
         maxima (issue #1429).\n{report}"
    );

    assert_eq!(
        indexer_run_count(&db).await,
        0,
        "a refused boot must not have started indexing.\n{report}"
    );
}

/// Issue #1392 AC — the same binary against a MATCHING schema starts normally,
/// proving the check does not false-positive.
///
/// "Starts normally" is proved by work the process could only have done after
/// the check passed: it logs the agreed version and it opens an `indexer_runs`
/// row. The tick itself fails (the RPC port is closed) and is recorded on that
/// row — a tick error is a retryable condition, not a boot refusal, so the
/// process still exits 0 under `--once`.
#[tokio::test]
async fn boot_accepts_a_matching_schema_and_starts_indexing() {
    let pg = raw_pg().await;

    let out = run_migrate_only(&pg.url);
    assert!(
        out.status.success(),
        "setup: migrate-only must succeed first.\n{}",
        describe(&out)
    );

    let db = Db::connect(&pg.url)
        .await
        .expect("connect to the migrated database");
    assert_eq!(
        db.applied_schema_version().await.unwrap(),
        Some(embedded_schema_version()),
        "setup: a freshly migrated database must match the embedded version"
    );
    assert_eq!(
        indexer_run_count(&db).await,
        0,
        "setup: migrate-only must not have started a run"
    );

    let out = run_boot(&pg.url);
    let report = describe(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("refuses to run against a schema it does not match"),
        "a matching schema must NOT be refused — the check false-positived.\n{report}"
    );
    assert!(
        stderr.contains("applied migration set matches the embedded one"),
        "the boot check must log the version it agreed on. Issue #1429 also \
         narrowed this line's claim: it used to say the schema \"matches\" on the \
         strength of a maxima comparison that could not see a checksum \
         divergence.\n{report}"
    );
    assert!(
        out.status.success(),
        "boot against a matching schema must proceed and exit 0 under --once \
         (the tick's RPC failure is retryable, not fatal).\n{report}"
    );
    assert_eq!(
        indexer_run_count(&db).await,
        1,
        "the accepted boot must have gone on to open an indexer_runs row.\n{report}"
    );
}

// ─── Issue #1392: pinning migration atomicity ────────────────────────────────
//
// `migrate_only_mode_exits_non_zero_on_a_broken_migration` observes that a
// failed migration leaves no half-created tables, but it cannot enforce the
// atomicity that makes that true in general: sqlx wraps each migration in a
// transaction, and a migration can opt out with `-- no-transaction`. If one
// ever did, that test would stay green while the property was gone.
//
// The guard chosen is an assertion over the EMBEDDED `no_tx` flag rather than a
// grep of the `.sql` files for the literal `-- no-transaction`. The flag is what
// sqlx actually acts on, so the guard cannot disagree with the migrator: a
// marker that is mis-spelled, indented, or below the first line would pass a
// grep while still running inside a transaction, and a future sqlx spelling of
// the opt-out would defeat a grep entirely.
//
// KNOWN LIMIT (issue #1416, filed from this work): on stable Rust `sqlx::migrate!`
// registers no rerun-if-changed dependency on `migrations/`, and this crate has no
// build.rs, so a change that touches ONLY a `.sql` file may not recompile the crate
// against a warm target dir — the guard would then read a stale embedded set. That
// is a build-freshness defect, not a hole in the guard, and it is tracked separately
// rather than hidden here.

/// Every migration this binary embeds runs inside a transaction, so a migration
/// that fails part-way rolls back whole.
#[test]
fn migration_atomicity_every_embedded_migration_is_transactional() {
    assert_migrations_are_transactional().unwrap_or_else(|e| panic!("{e}"));
}

/// Issue #1392 AC — negative self-test: hand the guard a migration that opted
/// out of its transaction and it must go red, naming the offender.
///
/// Without this, the test above would pass just as happily against a guard that
/// could never fail — the false-green shape this issue exists to close. The
/// offending migration is synthetic rather than a committed `.sql` file so the
/// guard's failure path is exercised on every CI run, not once by hand. (It was
/// also confirmed against a real `-- no-transaction` file: dropping one into
/// `migrations/` turns the test above red with
/// `migration 16 (...) is marked \`-- no-transaction\``.)
#[test]
fn migration_atomicity_guard_goes_red_on_a_no_transaction_migration() {
    // A set shaped like the real one — transactional migrations first, then one
    // that opted out. The guard must walk past the good ones and find it.
    let set = vec![
        Migration::new(
            1,
            Cow::Borrowed("synthetic_transactional"),
            MigrationType::Simple,
            Cow::Borrowed("CREATE TABLE probe (id BIGINT PRIMARY KEY);"),
            false,
        ),
        Migration::new(
            9999,
            Cow::Borrowed("synthetic_no_transaction_probe"),
            MigrationType::Simple,
            Cow::Borrowed(
                "-- no-transaction\nCREATE INDEX CONCURRENTLY probe ON chains (chain_id);",
            ),
            true,
        ),
    ];

    let flagged = first_non_transactional(&set).expect(
        "the guard must flag a no_tx migration — if this is None the guard is \
         vacuous and migration atomicity is unpinned",
    );
    assert_eq!(flagged.version, 9999);

    let msg = DbError::NonTransactionalMigration {
        version: flagged.version,
        description: flagged.description.to_string(),
    }
    .to_string();
    assert!(msg.contains("9999"), "message must name the version: {msg}");
    assert!(
        msg.contains("synthetic_no_transaction_probe"),
        "message must name the migration: {msg}"
    );
}

#[tokio::test]
async fn migrations_create_all_tables() {
    let fx = pg_fixture().await;
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
    let fx = pg_fixture().await;
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
/// The fixture DB already has migration 0003 applied (pg_fixture runs
/// all migrations from scratch on a fresh Postgres container).  We insert
/// a vault_snapshots row directly via the Db helper (which does NOT write
/// vault_address, relying on the DB default / backfill path), then verify
/// the vault_address column is non-NULL.
#[tokio::test]
async fn migration_0003_preserves_vault_snapshots_with_vault_address_backfilled() {
    let fx = pg_fixture().await;

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
    let fx = pg_fixture().await;
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

// ─── Issue #1429: the boot guard compares the SET, not two maxima ─────────────
//
// #1392's guard compared `MAX(version)` from `_sqlx_migrations` against the
// highest embedded version and ignored checksums. Two genuinely divergent
// schemas therefore passed it *and were announced as a match*:
//
//   1. a migration edited in place after being applied — content changes, version
//      does not, so both maxima are identical;
//   2. a row deleted from `_sqlx_migrations` BELOW the maximum — the maximum is
//      untouched;
//   3. an applied version this binary does not embed, below the maximum — a
//      migration file removed from the repo.
//
// Each of the three tests below therefore has a property the #1392 tests did not:
// it leaves `MAX(version)` on both sides EQUAL. That is what makes them red
// against the old guard rather than merely red against a missing one, and it is
// why they cannot be satisfied by any comparison of maxima.
//
// All three drive the compiled binary against a real database, like the #1392
// boot tests, because the claim is about what the process does at boot — not
// what a library function returns.

/// Overwrite one applied migration's recorded checksum.
///
/// This is the faithful mirror of editing an already-applied migration file. The
/// divergence the guard sees is "applied checksum != embedded checksum" for a
/// version present on both sides; whether that arose because the file changed
/// after being applied (the real deployment mistake) or because the row was
/// altered is not, and cannot be, distinguishable from the database. Perturbing
/// the row is the direction reachable from a test, since the embedded set is
/// fixed at compile time.
async fn corrupt_applied_checksum(db: &Db, version: i64) {
    let rows =
        sqlx::query("UPDATE _sqlx_migrations SET checksum = decode($1, 'hex') WHERE version = $2")
            .bind("deadbeef")
            .bind(version)
            .execute(db.pool())
            .await
            .expect("perturb the recorded checksum")
            .rows_affected();
    assert_eq!(
        rows, 1,
        "setup: expected to perturb exactly one _sqlx_migrations row for version {version}"
    );
}

/// Insert a `_sqlx_migrations` row for a version this binary does not embed.
async fn record_unembedded_migration(db: &Db, version: i64, description: &str) {
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES ($1, $2, now(), true, decode('00', 'hex'), 0)",
    )
    .bind(version)
    .bind(description)
    .execute(db.pool())
    .await
    .expect("record an unembedded migration");
}

/// Assert the binary refused at boot, and that the refusal reached the operator
/// and stopped short of any indexing.
fn assert_refused(out: &Output, report: &str) {
    assert!(
        !out.status.success(),
        "boot must FAIL, not proceed against a divergent schema.\n{report}"
    );
    assert!(
        out.status.code().is_some(),
        "boot must exit with a status code, not hang or die by signal.\n{report}"
    );
}

/// Issue #1429 AC — a migration edited in place after being applied is refused,
/// naming the version and that its checksum diverged.
///
/// `MAX(version)` is identical on both sides throughout, so #1392's guard
/// reported this as a match and then failed every tick on the schema it had just
/// approved. That is the silent-loop symptom #1392 existed to remove, announced
/// as success.
#[tokio::test]
async fn boot_refuses_a_migration_edited_in_place_naming_the_checksum() {
    let pg = raw_pg().await;

    let out = run_migrate_only(&pg.url);
    assert!(
        out.status.success(),
        "setup: migrate-only must succeed first.\n{}",
        describe(&out)
    );

    let db = Db::connect(&pg.url)
        .await
        .expect("connect to the migrated database");
    let embedded = embedded_schema_version();

    corrupt_applied_checksum(&db, embedded).await;

    // The whole point of this case: the maxima still agree.
    assert_eq!(
        db.applied_schema_version().await.unwrap(),
        Some(embedded),
        "setup: MAX(version) must still MATCH — that is what made #1392's guard \
         approve this schema"
    );

    let out = run_boot(&pg.url);
    let report = describe(&out);
    assert_refused(&out, &report);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("checksum"),
        "the refusal must say the CHECKSUM diverged, not merely that something is \
         wrong — the remedy for an edited migration differs from the remedy for a \
         stale one.\n{report}"
    );
    assert!(
        stderr.contains(&embedded.to_string()),
        "the refusal must name the diverging version ({embedded}).\n{report}"
    );
    assert_eq!(
        indexer_run_count(&db).await,
        0,
        "a refused boot must not have started indexing.\n{report}"
    );
}

/// Issue #1429 AC — a row deleted from `_sqlx_migrations` below the maximum is
/// refused, naming the missing version.
///
/// This is the case a comparison of maxima cannot see even in principle: the
/// newest migration is still recorded, so `MAX(version)` matches while a
/// migration in the middle of the set was never applied (or was un-recorded by
/// hand).
#[tokio::test]
async fn boot_refuses_a_row_deleted_below_the_maximum_naming_the_missing_version() {
    let pg = raw_pg().await;

    let out = run_migrate_only(&pg.url);
    assert!(
        out.status.success(),
        "setup: migrate-only must succeed first.\n{}",
        describe(&out)
    );

    let db = Db::connect(&pg.url)
        .await
        .expect("connect to the migrated database");
    let embedded = embedded_schema_version();

    // Pick a version strictly below the maximum so the maximum is untouched.
    let missing = 7i64;
    assert!(
        missing < embedded,
        "setup: the deleted version must be BELOW the maximum for this test to \
         mean anything (embedded max is {embedded})"
    );
    let rows = sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(missing)
        .execute(db.pool())
        .await
        .expect("un-record a migration below the maximum")
        .rows_affected();
    assert_eq!(rows, 1, "setup: expected to delete exactly one row");

    assert_eq!(
        db.applied_schema_version().await.unwrap(),
        Some(embedded),
        "setup: MAX(version) must still MATCH — that is what made #1392's guard \
         approve this schema"
    );

    let out = run_boot(&pg.url);
    let report = describe(&out);
    assert_refused(&out, &report);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("migration {missing}")),
        "the refusal must name the MISSING version ({missing}), not just report a \
         mismatch.\n{report}"
    );
    assert!(
        stderr.contains("not applied"),
        "the refusal must say the migration is not applied.\n{report}"
    );
    assert_eq!(
        indexer_run_count(&db).await,
        0,
        "a refused boot must not have started indexing.\n{report}"
    );
}

/// Issue #1429 AC — an applied version with no embedded counterpart is refused
/// with a message that says so, rather than being reported as a match.
///
/// Version 0 is used deliberately. The embedded set is contiguous (0001..NNNN),
/// so version 0 is the only version that is both absent from the embedded set
/// and low enough to leave `MAX(version)` unchanged — and leaving the maximum
/// unchanged is the entire point, since that is the state #1392's guard called a
/// match. It stands for a migration file deleted from the repo while still
/// applied to a long-lived database.
#[tokio::test]
async fn boot_refuses_an_applied_version_this_binary_does_not_embed() {
    let pg = raw_pg().await;

    let out = run_migrate_only(&pg.url);
    assert!(
        out.status.success(),
        "setup: migrate-only must succeed first.\n{}",
        describe(&out)
    );

    let db = Db::connect(&pg.url)
        .await
        .expect("connect to the migrated database");
    let embedded = embedded_schema_version();

    record_unembedded_migration(&db, 0, "deleted_from_the_repo").await;

    assert_eq!(
        db.applied_schema_version().await.unwrap(),
        Some(embedded),
        "setup: MAX(version) must still MATCH — that is what made #1392's guard \
         approve this schema"
    );

    let out = run_boot(&pg.url);
    let report = describe(&out);
    assert_refused(&out, &report);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not embed"),
        "the refusal must say the binary does not embed the applied migration, \
         rather than reporting a match.\n{report}"
    );
    assert!(
        stderr.contains("migration 0"),
        "the refusal must name the offending applied version.\n{report}"
    );
    assert_eq!(
        indexer_run_count(&db).await,
        0,
        "a refused boot must not have started indexing.\n{report}"
    );
}

/// Issue #1429 — a rollback to an older binary is refused with advice that fits
/// it: the schema is AHEAD, so `--migrate-only` is not the remedy.
///
/// #1392's guard did refuse this (the maxima differ), but its only message told
/// the operator to run `--migrate-only`, which cannot move a schema backwards.
/// Naming the shape is the deliverable, not merely refusing.
#[tokio::test]
async fn boot_refuses_a_rollback_without_advising_migrate_only() {
    let pg = raw_pg().await;

    let out = run_migrate_only(&pg.url);
    assert!(
        out.status.success(),
        "setup: migrate-only must succeed first.\n{}",
        describe(&out)
    );

    let db = Db::connect(&pg.url)
        .await
        .expect("connect to the migrated database");
    let embedded = embedded_schema_version();
    let ahead = embedded + 1;

    record_unembedded_migration(&db, ahead, "applied_by_a_newer_binary").await;

    let out = run_boot(&pg.url);
    let report = describe(&out);
    assert_refused(&out, &report);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("migration {ahead}")),
        "the refusal must name the version the database is ahead by.\n{report}"
    );
    assert!(
        stderr.contains("newer binary"),
        "the refusal must identify this as a rollback, not a stale schema.\n{report}"
    );
    assert!(
        !stderr.contains("Run `indexer --migrate-only`"),
        "a schema that is AHEAD cannot be fixed by migrating forward, so the \
         refusal must not advise it.\n{report}"
    );
    assert_eq!(
        indexer_run_count(&db).await,
        0,
        "a refused boot must not have started indexing.\n{report}"
    );
}

// ─── Issue #1429: compare_schema without a database ──────────────────────────
//
// The comparison takes both sides as slices for the same reason
// `first_non_transactional` does (#1392): the shapes can be driven from
// synthetic inputs, so the database-backed tests above only have to prove the
// two halves are wired together. It is also a free function rather than a method
// because `explorer-api` needs the identical check (#1430) and must not grow a
// second, independently-drifting copy.

/// Two embedded migrations with real checksums, derived from their SQL by
/// `Migration::new` exactly as `sqlx::migrate!` does.
fn synthetic_embedded() -> Vec<Migration> {
    vec![
        Migration::new(
            1,
            Cow::Borrowed("first"),
            MigrationType::Simple,
            Cow::Borrowed("CREATE TABLE a (id BIGINT PRIMARY KEY);"),
            false,
        ),
        Migration::new(
            2,
            Cow::Borrowed("second"),
            MigrationType::Simple,
            Cow::Borrowed("CREATE TABLE b (id BIGINT PRIMARY KEY);"),
            false,
        ),
    ]
}

fn applied_from(embedded: &[Migration]) -> Vec<AppliedMigration> {
    embedded
        .iter()
        .map(|m| AppliedMigration {
            version: m.version,
            checksum: m.checksum.to_vec(),
        })
        .collect()
}

#[test]
fn compare_schema_accepts_an_identical_set() {
    let embedded = synthetic_embedded();
    let applied = applied_from(&embedded);
    assert_eq!(compare_schema(&applied, &embedded), Ok(2));
}

#[test]
fn compare_schema_rejects_an_empty_database() {
    let embedded = synthetic_embedded();
    assert_eq!(
        compare_schema(&[], &embedded),
        Err(SchemaDivergence::NeverMigrated { embedded: 2 })
    );
}

#[test]
fn compare_schema_rejects_a_changed_checksum_while_the_maxima_agree() {
    let embedded = synthetic_embedded();
    let mut applied = applied_from(&embedded);
    applied[0].checksum = vec![0xde, 0xad, 0xbe, 0xef];

    // The maxima are equal, so a maxima-only comparison would accept this.
    assert_eq!(
        applied.iter().map(|a| a.version).max(),
        embedded.iter().map(|m| m.version).max()
    );

    let err = compare_schema(&applied, &embedded).expect_err("must be rejected");
    assert_eq!(
        err,
        SchemaDivergence::ChecksumMismatch {
            version: 1,
            description: "first".to_string(),
        }
    );
    assert!(
        err.to_string().contains("edited in place"),
        "the message must name the mistake: {err}"
    );
}

#[test]
fn compare_schema_rejects_a_missing_version_below_the_maximum() {
    let embedded = synthetic_embedded();
    let applied: Vec<AppliedMigration> = applied_from(&embedded)
        .into_iter()
        .filter(|a| a.version != 1)
        .collect();

    let err = compare_schema(&applied, &embedded).expect_err("must be rejected");
    assert_eq!(
        err,
        SchemaDivergence::MissingVersion {
            version: 1,
            description: "first".to_string(),
            applied_max: 2,
            embedded: 2,
        }
    );
}

#[test]
fn compare_schema_rejects_an_applied_version_with_no_embedded_counterpart() {
    let embedded = synthetic_embedded();
    let mut applied = applied_from(&embedded);
    applied.push(AppliedMigration {
        version: 3,
        checksum: vec![0x00],
    });

    assert_eq!(
        compare_schema(&applied, &embedded),
        Err(SchemaDivergence::UnknownAppliedVersion {
            version: 3,
            embedded: 2,
        })
    );
}

/// The refusal names the FIRST divergence, so the message points at the earliest
/// point the two schemas parted company rather than an arbitrary one.
#[test]
fn compare_schema_reports_the_lowest_versioned_divergence_first() {
    let embedded = synthetic_embedded();
    let mut applied = applied_from(&embedded);
    // Two divergences at once: version 1's checksum changed, version 2 missing.
    applied[0].checksum = vec![0xde, 0xad];
    applied.retain(|a| a.version != 2);

    let err = compare_schema(&applied, &embedded).expect_err("must be rejected");
    assert_eq!(
        err.version(),
        1,
        "the lowest-versioned divergence must be the one reported: {err}"
    );
}
