//! Canonical: docs/architecture.md §5.4 — Explorer Indexer and API.
//!
//! WHY THIS BUILD SCRIPT EXISTS (issue #1416)
//! ------------------------------------------
//! `src/db.rs` embeds the whole migration set at COMPILE time:
//!
//! ```ignore
//! pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
//! ```
//!
//! On **stable** Rust that macro reads `migrations/` with plain `std::fs` and
//! registers no build dependency on it. sqlx calls
//! `proc_macro::tracked_path::path()` — the API that would tell cargo the macro
//! consumed those files — only under
//! `#[cfg(any(sqlx_macros_unstable, procmacro2_semver_exempt))]`
//! (`sqlx-macros-core/src/migrate.rs`), and this workspace builds on stable.
//!
//! The consequence is a silent, invisible defect: change only a `.sql` file and
//! cargo considers `explorer-indexer` fresh, so the binary and the test
//! binaries keep an OLDER embedded migration set than the repository holds. CI
//! is exposed to exactly this because `Swatinem/rust-cache` restores `target/`
//! across runs — a PR that only adds a migration could go green with the new
//! migration never compiled in, and `indexer --migrate-only` built from that
//! cache would not apply it.
//!
//! This script supplies the dependency sqlx cannot. `rerun-if-changed` on a
//! DIRECTORY is walked recursively by cargo (it takes the newest mtime beneath
//! it), so this covers file additions and deletions as well as edits to an
//! existing migration. When the directory is dirty the build script re-runs,
//! which marks the crate dirty, which re-expands `sqlx::migrate!`.
//!
//! `tests/migration_set_parity.rs` is the guard on this mechanism: it compares
//! the compile-time embedded set against the run-time contents of
//! `migrations/`, so removing or breaking this script turns CI red instead of
//! shipping a stale binary.

use std::path::Path;

fn main() {
    // Cargo resolves a relative rerun-if-changed against the package root, the
    // same root `sqlx::migrate!("./migrations")` resolves against — so the two
    // always name the same directory.
    let migrations = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");

    // Fail the build rather than emit a directive for a path that does not
    // exist: cargo silently ignores a rerun-if-changed on a missing path, which
    // would quietly restore the very staleness bug this script exists to fix.
    assert!(
        migrations.is_dir(),
        "expected a migrations directory at {} — `sqlx::migrate!(\"./migrations\")` in \
         src/db.rs reads it at compile time, and this build script must track it so a \
         .sql-only change forces a rebuild (issue #1416)",
        migrations.display()
    );

    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=build.rs");
}
