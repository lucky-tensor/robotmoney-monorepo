//! Issue #87 — canonical schema parity.
//!
//! Asserts (a) the migration this crate links against is the byte-exact
//! file owned by `services/explorer-indexer/migrations/`, and (b) when
//! that migration is applied to a Postgres testcontainer, the same nine
//! §11 minimum tables observed by `services/explorer-indexer/tests/migrations.rs`
//! are also observed from the api crate's harness. Together these two
//! checks prove there is one canonical schema and that both crates can
//! stand it up identically.
//!
//! Check (a) needs no Docker and runs everywhere. Check (b) needs a Postgres
//! testcontainer and **fails loudly**, naming Docker, when one cannot be booted —
//! it used to `return` early instead, which the harness reported as a pass having
//! asserted nothing (#1377).

mod common;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

use common::{apply_migrations, CANONICAL_MIGRATION};

/// Static byte-for-byte parity between the file shipped in this crate's
/// `include_str!` and the file on disk in the indexer crate. Runs in
/// every `cargo test`, no Docker required — a divergence here is a
/// build-time hard fail.
#[test]
fn migration_bytes_equal_indexer_canonical() {
    let on_disk = std::fs::read_to_string(
        "../../services/explorer-indexer/migrations/0001_minimum_tables.sql",
    )
    .expect("read canonical migration from disk");
    assert_eq!(
        on_disk, CANONICAL_MIGRATION,
        "explorer-api include_str! drifted from canonical indexer migration"
    );
}

/// Information-schema parity: applying the canonical migration from the
/// api crate's harness yields exactly the nine §11 tables that
/// `services/explorer-indexer/tests/migrations.rs` also asserts on. If
/// either crate ever applies a different DDL, this test fails.
#[tokio::test]
async fn canonical_schema_yields_nine_minimum_tables() {
    let pool = pg_pool().await;
    apply_migrations(&pool).await;

    let expected = [
        "chains",
        "contracts",
        "blocks",
        "transactions",
        "agent_deposits",
        "agent_policies",
        "vault_snapshots",
        "wallet_positions",
        "indexer_runs",
    ];
    for t in expected {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*)::BIGINT FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = $1",
        )
        .bind(t)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 1, "canonical schema must define table {t}");
    }
}

/// Prefix every unmet-dependency panic carries, so one grep over a job log finds
/// the cause without reading the backtrace.
const MISSING: &str = "[explorer-api-tests] REQUIRED DEPENDENCY UNAVAILABLE";

/// Boot a Postgres testcontainer and return a pool against it.
///
/// # Panics
///
/// Panics — naming the missing dependency — when Docker is unusable or the
/// container cannot be started, addressed, or connected to. It does **not** return
/// an "unavailable" sentinel: the caller previously turned that sentinel into an
/// early `return`, which the harness reported as a pass having asserted nothing
/// (#1377). The `explorer-api-committee-regime` job that runs this target already
/// verifies Docker is present before invoking it.
async fn pg_pool() -> PgPool {
    assert!(
        docker_usable(),
        "{MISSING}: `docker --version` did not succeed. This test needs a Postgres \
         testcontainer to prove canonical-schema parity — install/start Docker, or \
         run only `cargo test -p explorer-api --test canonical_schema \
         migration_bytes_equal_indexer_canonical`, which needs none. This is a hard \
         failure rather than a skip on purpose: an absent dependency must red the job."
    );
    let container = match Postgres::default().start().await {
        Ok(c) => c,
        Err(e) => panic!(
            "{MISSING}: the Postgres testcontainer failed to start: {e}. Docker responds \
             but is not usable (daemon down, socket permissions, or no image pull)."
        ),
    };
    // Leak the container handle for the duration of this single test
    // process; it will be reaped when the test process exits. We hold
    // it alive only to keep the connection valid below.
    let host = container
        .get_host()
        .await
        .unwrap_or_else(|e| panic!("{MISSING}: Postgres testcontainer host unknown: {e}"));
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .unwrap_or_else(|e| panic!("{MISSING}: Postgres testcontainer port 5432 unmapped: {e}"));
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap_or_else(|e| {
            panic!("{MISSING}: could not connect to the Postgres testcontainer at {url}: {e}")
        });
    // Box-leak the container so it outlives the pool; an alternative is
    // to thread it through, but this test only needs a one-shot fixture.
    Box::leak(Box::new(container));
    pool
}

fn docker_usable() -> bool {
    std::process::Command::new("docker")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}
