//! Canonical: docs/architecture.md §5.4 — Explorer Indexer and API.
//!
//! WHY THIS TARGET EXISTS (issue #1416)
//! ------------------------------------
//! `src/db.rs` embeds the migration set at COMPILE time with
//! `sqlx::migrate!("./migrations")`. On stable Rust that macro registers no
//! build dependency on the directory it read — sqlx calls
//! `proc_macro::tracked_path::path()` only under `#[cfg(sqlx_macros_unstable)]`.
//! So without help, cargo considers this crate fresh when only a `.sql` file
//! changed, and the binary keeps an OLDER embedded migration set than the
//! repository holds. `build.rs` supplies the missing
//! `cargo:rerun-if-changed=migrations` dependency.
//!
//! This target is the guard on that mechanism. It compares the COMPILE-time
//! embedded set (`MIGRATOR`, frozen into this test binary when the crate was
//! last built) against the RUN-time contents of `migrations/` (read from disk
//! here and now). Those two can only disagree if the rebuild trigger failed,
//! so a regression in `build.rs` — deleting it, renaming the directory,
//! misspelling the directive — turns this RED instead of shipping a binary
//! that silently omits a migration.
//!
//! It needs no Postgres, no Docker and no network: it is a filesystem walk
//! plus an iteration over an embedded static. That is why it is wired into the
//! light `rust-lint` job (suite 4) rather than the Docker-bound
//! `explorer-indexer-fast` job (suite 8) — the guard is about the build, so it
//! must run on every Rust change, including drafts.

use explorer_indexer::db::MIGRATOR;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The directory `sqlx::migrate!("./migrations")` read at compile time,
/// resolved relative to the crate root exactly as the macro resolves it.
fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations")
}

/// Read `migrations/*.sql` off disk and derive `version -> description` the
/// same way sqlx's `resolve_blocking` does: split the file stem on the first
/// `_`, parse the left half as the version, and turn the right half's
/// underscores into spaces for the description.
fn on_disk_migrations() -> BTreeMap<i64, String> {
    let dir = migrations_dir();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read migrations directory {}: {e}", dir.display()));

    let mut found = BTreeMap::new();
    for entry in entries {
        let path = entry.expect("read a migrations directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| panic!("non-UTF-8 migration filename: {}", path.display()));
        let (version, description) = stem.split_once('_').unwrap_or_else(|| {
            panic!("migration filename {stem:?} is not `<version>_<description>.sql`")
        });
        let version: i64 = version
            .parse()
            .unwrap_or_else(|e| panic!("migration filename {stem:?} has no numeric version: {e}"));
        let previous = found.insert(version, description.replace('_', " "));
        assert!(
            previous.is_none(),
            "two migration files share version {version} in {}",
            dir.display()
        );
    }
    assert!(
        !found.is_empty(),
        "no .sql files under {} — the directory moved, and MIGRATOR would be silently empty",
        dir.display()
    );
    found
}

/// Issue #1416 AC — the embedded set matches the on-disk directory.
///
/// A failure here means the binary under test was NOT rebuilt after
/// `migrations/` changed. Read it as "the rebuild trigger is broken", not as
/// "a migration is malformed".
#[test]
fn embedded_migration_set_matches_the_migrations_directory() {
    let on_disk = on_disk_migrations();
    let embedded: BTreeMap<i64, String> = MIGRATOR
        .iter()
        .map(|m| (m.version, m.description.to_string()))
        .collect();

    let stale_hint = "The compile-time embedded set (MIGRATOR) disagrees with the on-disk \
         migrations/ directory. Almost always this means cargo did not rebuild \
         explorer-indexer after a .sql-only change: check that \
         services/explorer-indexer/build.rs still emits \
         `cargo:rerun-if-changed` for the migrations directory (issue #1416). \
         `touch services/explorer-indexer/src/db.rs` forces the rebuild by hand.";

    assert_eq!(
        embedded.len(),
        on_disk.len(),
        "embedded migration COUNT {} != on-disk count {}.\nembedded: {:?}\non disk: {:?}\n{stale_hint}",
        embedded.len(),
        on_disk.len(),
        embedded.keys().collect::<Vec<_>>(),
        on_disk.keys().collect::<Vec<_>>(),
    );

    assert_eq!(
        embedded.keys().max(),
        on_disk.keys().max(),
        "embedded MAX VERSION {:?} != on-disk max version {:?}.\n{stale_hint}",
        embedded.keys().max(),
        on_disk.keys().max(),
    );

    assert_eq!(
        embedded, on_disk,
        "embedded migrations differ from the on-disk directory (version -> description).\n{stale_hint}"
    );
}
