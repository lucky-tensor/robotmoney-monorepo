//! Canonical: docs/architecture.md §5.4 — Explorer Indexer and API
//! (See also: docs/technical/explorer-schema-decisions.md)
//! sqlx-Postgres pool wrapper plus all DML the indexer issues.
//!
//! Every write is keyed on the composite PKs from
//! `migrations/0001_minimum_tables.sql` (ADR §3.4) and uses
//! `ON CONFLICT DO NOTHING` so re-indexing the same range is a no-op
//! (issue #57 acceptance criterion).

use alloy_primitives::U256;
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// Returned by [`CountTable::try_from`] when the caller supplies a
    /// string that is not in the known-good identifier allowlist.
    #[error("unknown table identifier: {0:?}")]
    UnknownTable(String),
    /// Issue #1283. A live table carries `block_number` but no `chain_id`, so
    /// [`Db::delete_above_block`] cannot confine its rollback DELETE to the
    /// chain that reorged. Failing loudly is deliberate: silently skipping the
    /// table is precisely the orphaned-row bug this derivation replaced.
    #[error(
        "block-scoped table {0:?} has no chain_id column, so reorg rollback cannot \
         scope its DELETE to one chain. Add chain_id to the table, or add \
         ({0:?}, \"<reason>\") to db::REORG_ROLLBACK_EXCLUSIONS."
    )]
    UnscopedBlockTable(String),
    /// Issue #1392, narrowed by #1429. The boot path deliberately does **not**
    /// migrate (#1359), so a database whose applied migration set differs from
    /// the set this binary embeds is refused at boot instead of served. Before
    /// this existed, an indexer pointed at a stale schema just looped, failing
    /// every tick with no healthcheck to notice.
    ///
    /// The message comes from [`SchemaDivergence`], which names *which* of the
    /// four disagreement shapes this is — the remedies differ, and #1429 was
    /// filed because a single "version mismatch" message could not express an
    /// in-place edit or a rollback at all.
    #[error(transparent)]
    SchemaDivergent(#[from] SchemaDivergence),
    /// Issue #1392. An embedded migration opted out of its transaction with
    /// `-- no-transaction`, so a failure part-way through it would leave a
    /// half-applied schema behind — the property
    /// `migrate_only_mode_exits_non_zero_on_a_broken_migration` relies on.
    #[error(
        "migration {version} ({description:?}) is marked `-- no-transaction`, so a \
         failure part-way through it would leave a half-applied schema behind. \
         The explorer's migrate step relies on every migration being atomic \
         (issue #1392): split the statement so it can run inside a transaction, \
         or change this guard deliberately and record what replaces the \
         atomicity guarantee."
    )]
    NonTransactionalMigration { version: i64, description: String },
}

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

/// Embed the migrations directory at compile time so `cargo test`
/// (which does not call sqlx-cli) can still apply schema.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// One way the database's applied schema disagrees with the migration set this
/// binary embeds.
///
/// Issue #1429. The original guard (#1392) compared two **maxima** —
/// `MAX(version)` from `_sqlx_migrations` against the highest embedded version —
/// and ignored checksums, so two genuinely divergent schemas passed it: a
/// migration edited in place after being applied (content changes, version does
/// not), and rows hand-deleted from `_sqlx_migrations` *below* the maximum.
/// Both then produced exactly the silent tick-failure loop #1392 was opened to
/// eliminate, except announced as a match.
///
/// The data needed was already in the table the guard read: `_sqlx_migrations`
/// stores a per-version checksum, and `MIGRATOR.run` performs this same
/// comparison — which is why the compose stack's `explorer-migrate` step does
/// catch this. The residual exposure was precisely the **skipped-migrate-step**
/// case this boot guard exists to be the last line of defence against.
///
/// Each variant names one shape so the operator is told which mistake this is;
/// "your schema is wrong" does not distinguish a rollback from an edit.
///
/// # Match on the variant; do not print `Display` from another context
///
/// The messages are written for the **indexer boot path** and name its remedies
/// — `MissingVersion` says to run `indexer --migrate-only` against this
/// `DATABASE_URL`, `UnknownAppliedVersion` says to deploy a matching binary.
/// Those instructions are wrong anywhere else. A second caller comparing a
/// different pair of realities — the on-disk `migrations/` directory against the
/// embedded set, say — gets the right *classification* from [`compare_schema`]
/// but must render its own text from the variant, because its remedy is
/// "regenerate the embedded set", not "migrate the database".
///
/// That is why this is a public enum with public fields rather than an opaque
/// error: matching on it is the intended use, and `{e}` from a non-boot caller
/// is the misuse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaDivergence {
    /// `_sqlx_migrations` is absent or empty — nothing has ever been migrated.
    #[error(
        "the database has no applied migrations at all (`_sqlx_migrations` is \
         absent or empty), but this binary embeds explorer migrations up to \
         version {embedded}. The indexer never migrates on boot (issue #1359), \
         so it refuses to run against a schema it does not match (issue #1392). \
         Run `indexer --migrate-only` against this DATABASE_URL."
    )]
    NeverMigrated { embedded: i64 },

    /// Embedded here, never applied there. The ordinary stale-database case,
    /// and also what a row deleted from `_sqlx_migrations` below the maximum
    /// looks like — invisible to a comparison of maxima.
    #[error(
        "migration {version} ({description:?}) is embedded in this binary but is \
         not applied to the database, which has applied up to version {applied_max} \
         (this binary embeds up to {embedded}). The indexer never migrates on boot \
         (issue #1359), so it refuses to run against a schema it does not match \
         (issue #1392). Run `indexer --migrate-only` against this DATABASE_URL, or \
         deploy the binary that matches the database."
    )]
    MissingVersion {
        version: i64,
        description: String,
        applied_max: i64,
        embedded: i64,
    },

    /// Same version on both sides, different content: a migration edited in
    /// place after it had already been applied somewhere. `MAX(version)` is
    /// identical on both sides, which is why the maxima comparison passed it.
    #[error(
        "migration {version} ({description:?}) is applied to the database, but its \
         content has changed since: the applied checksum does not match the one \
         embedded in this binary. An already-applied migration was edited in \
         place, so the database's schema is not what this binary expects even \
         though the version numbers agree. Do not edit an applied migration — add \
         a new one. To recover, point this binary at a database migrated from the \
         current files, or restore the migration file to the content that was \
         applied."
    )]
    ChecksumMismatch { version: i64, description: String },

    /// Applied there, absent here: the database was migrated by a *newer*
    /// binary than this one, i.e. a rollback to an older image. Reported as
    /// its own shape because "run --migrate-only" is the wrong advice for it.
    #[error(
        "migration {version} is applied to the database but this binary does not \
         embed it (this binary embeds up to version {embedded}). The database was \
         migrated by a newer binary than this one — this looks like a rollback to \
         an older image, which `--migrate-only` will not fix because the schema is \
         ahead, not behind. Deploy the binary that matches the database, or restore \
         the database to a state this binary embeds."
    )]
    UnknownAppliedVersion { version: i64, embedded: i64 },
}

impl SchemaDivergence {
    /// The migration version this divergence is about, used to report the
    /// **first** (lowest-versioned) divergence rather than an arbitrary one.
    pub fn version(&self) -> i64 {
        match self {
            SchemaDivergence::NeverMigrated { .. } => 0,
            SchemaDivergence::MissingVersion { version, .. }
            | SchemaDivergence::ChecksumMismatch { version, .. }
            | SchemaDivergence::UnknownAppliedVersion { version, .. } => *version,
        }
    }
}

/// One row of `_sqlx_migrations`, reduced to what the boot guard compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMigration {
    pub version: i64,
    pub checksum: Vec<u8>,
}

/// Compare the applied migration **set** against the embedded one, returning
/// the agreed highest version or the first divergence.
///
/// Issue #1429. Taking both sides as slices — rather than reading [`MIGRATOR`]
/// and the pool directly — is what makes the comparison testable without a
/// database, the same reasoning as [`first_non_transactional`] (#1392): the
/// three refusal shapes can be driven from synthetic inputs, and the
/// database-backed tests then only have to prove the two halves are wired
/// together.
///
/// It is also why this is a free function over slices rather than a method:
/// `explorer-api` needs the identical check (#1430) and must not grow a second,
/// independently-drifting copy of it.
///
/// Divergences are reported lowest-version-first, so the message names the
/// earliest point at which the two schemas parted company rather than an
/// arbitrary one.
pub fn compare_schema(
    applied: &[AppliedMigration],
    embedded: &[sqlx::migrate::Migration],
) -> Result<i64, SchemaDivergence> {
    let embedded_max = embedded
        .iter()
        .map(|m| m.version)
        .max()
        .expect("the embedded migrations directory is never empty");

    if applied.is_empty() {
        return Err(SchemaDivergence::NeverMigrated {
            embedded: embedded_max,
        });
    }
    let applied_max = applied
        .iter()
        .map(|m| m.version)
        .max()
        .expect("applied is non-empty");

    let applied_by: std::collections::BTreeMap<i64, &[u8]> = applied
        .iter()
        .map(|a| (a.version, a.checksum.as_slice()))
        .collect();

    let mut divergences: Vec<SchemaDivergence> = Vec::new();

    for m in embedded {
        match applied_by.get(&m.version) {
            None => divergences.push(SchemaDivergence::MissingVersion {
                version: m.version,
                description: m.description.to_string(),
                applied_max,
                embedded: embedded_max,
            }),
            Some(checksum) if *checksum != m.checksum.as_ref() => {
                divergences.push(SchemaDivergence::ChecksumMismatch {
                    version: m.version,
                    description: m.description.to_string(),
                })
            }
            Some(_) => {}
        }
    }

    let embedded_versions: std::collections::BTreeSet<i64> =
        embedded.iter().map(|m| m.version).collect();
    for version in applied_by.keys() {
        if !embedded_versions.contains(version) {
            divergences.push(SchemaDivergence::UnknownAppliedVersion {
                version: *version,
                embedded: embedded_max,
            });
        }
    }

    divergences.sort_by_key(|d| d.version());
    match divergences.into_iter().next() {
        Some(d) => Err(d),
        None => Ok(embedded_max),
    }
}

/// Highest migration version compiled into this binary.
///
/// Issue #1392: one half of the original boot-time schema check. Superseded as
/// the *guard* by [`compare_schema`] (#1429), which compares the whole
/// version+checksum set rather than two maxima; this remains the value reported
/// on success and in operator messages.
pub fn embedded_schema_version() -> i64 {
    MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .expect("the embedded migrations directory is never empty")
}

/// The first migration in `migrations` that opted out of running inside a
/// transaction (`-- no-transaction` on its first line), if any.
///
/// Issue #1392: `migrate_only_mode_exits_non_zero_on_a_broken_migration` asserts
/// that a failed migration leaves no half-created tables behind. That holds only
/// because sqlx wraps each migration in a transaction, which a migration can opt
/// out of. Taking a slice (rather than reading [`MIGRATOR`] directly) is what
/// makes the guard itself testable: `tests/migrations.rs` hands it a synthetic
/// `no_tx` migration and proves the guard goes red, without committing one.
pub fn first_non_transactional(
    migrations: &[sqlx::migrate::Migration],
) -> Option<&sqlx::migrate::Migration> {
    migrations.iter().find(|m| m.no_tx)
}

/// Fail if any embedded migration opted out of its transaction (issue #1392).
///
/// This is the atomicity guard chosen over grepping the `.sql` files for the
/// literal `-- no-transaction`: the embedded `no_tx` flag is what sqlx actually
/// acts on at runtime, so the guard cannot disagree with the migrator (a file
/// whose marker is mis-spelled, indented, or on the wrong line would pass a grep
/// while still running inside a transaction, and vice versa).
pub fn assert_migrations_are_transactional() -> Result<(), DbError> {
    match first_non_transactional(&MIGRATOR.migrations) {
        Some(m) => Err(DbError::NonTransactionalMigration {
            version: m.version,
            description: m.description.to_string(),
        }),
        None => Ok(()),
    }
}

/// All countable tables (nine §11 tables plus the vault registry table
/// added in migration 0002, and the governance tables added in migration 0003).
///
/// Using a typed enum instead of a raw `&str` ensures no caller can
/// pass a user-controlled string to the dynamic `FORMAT` in
/// [`Db::count`] (issue #165).  Adding a new variant here is
/// intentionally explicit — the compiler will flag every incomplete
/// `match` if you add one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountTable {
    Chains,
    Contracts,
    Blocks,
    Transactions,
    AgentDeposits,
    AgentPolicies,
    VaultSnapshots,
    WalletPositions,
    IndexerRuns,
    /// Added in migration 0002 — vault registry table.
    Vaults,
    /// Added in migration 0012 — append-only vault status transition history
    /// that the in-place `vaults.status` is derived from, making status
    /// reorg-safe (IDX-8).
    VaultStatusEvents,
    /// Added in migration 0003 — governance proposal events.
    GovernanceProposals,
    /// Added in migration 0003 — per-voter vote events.
    GovernanceVotes,
    /// Added in migration 0003 — weight-change history from WeightsApplied events.
    RouterWeightSnapshots,
    /// Added in migration 0006 — per-leg data from RouterDeposit events (issue #373).
    RouterDepositLegs,
    /// Added in migration 0008 — adapter allocation history (issue #675).
    AdapterAllocations,
    /// Added in migration 0008 — exit fee event log (issue #675).
    VaultFeeEvents,
    /// Added in migration 0008 — ERC-4626 deposit/withdrawal event log (issue #675).
    VaultTransferEvents,
    /// Added in migration 0009 — full account history events (issue #654).
    AccountHistoryEvents,
    /// Added in migration 0014 — committee agent registry (issue #1053).
    CommitteeAgents,
    /// Added in migration 0014 — per-vote commitments (issue #1053).
    CommitteeVotes,
    /// Added in migration 0014 — aggregated per-vault tilt snapshots (issue #1053).
    RegimeSnapshots,
    /// Added in migration 0015 — on-chain consensus recommendation receipt
    /// commitments (issue #1247, docs/architecture.md §4.9).
    ConsensusReceipts,
}

impl CountTable {
    /// Return the SQL table name for use in `FORMAT!("… FROM {}", …)`.
    fn as_str(self) -> &'static str {
        match self {
            CountTable::Chains => "chains",
            CountTable::Contracts => "contracts",
            CountTable::Blocks => "blocks",
            CountTable::Transactions => "transactions",
            CountTable::AgentDeposits => "agent_deposits",
            CountTable::AgentPolicies => "agent_policies",
            CountTable::VaultSnapshots => "vault_snapshots",
            CountTable::WalletPositions => "wallet_positions",
            CountTable::IndexerRuns => "indexer_runs",
            CountTable::Vaults => "vaults",
            CountTable::VaultStatusEvents => "vault_status_events",
            CountTable::GovernanceProposals => "governance_proposals",
            CountTable::GovernanceVotes => "governance_votes",
            CountTable::RouterWeightSnapshots => "router_weight_snapshots",
            CountTable::RouterDepositLegs => "router_deposit_legs",
            CountTable::AdapterAllocations => "adapter_allocations",
            CountTable::VaultFeeEvents => "vault_fee_events",
            CountTable::VaultTransferEvents => "vault_transfer_events",
            CountTable::AccountHistoryEvents => "account_history_events",
            CountTable::CommitteeAgents => "committee_agents",
            CountTable::CommitteeVotes => "committee_votes",
            CountTable::RegimeSnapshots => "regime_snapshots",
            CountTable::ConsensusReceipts => "consensus_receipts",
        }
    }
}

/// Runtime allowlist check: parse a raw string identifier into a
/// [`CountTable`] variant.  Any string not in the known-good set is
/// rejected with [`DbError::UnknownTable`], preventing callers that
/// receive table names from external sources (config files, HTTP
/// query parameters, etc.) from accidentally expanding the SQL query
/// surface.
///
/// The compile-time path (passing a [`CountTable`] literal directly to
/// [`Db::count`]) is always preferred.  This impl exists for code paths
/// where the table name is only known at runtime.
impl TryFrom<&str> for CountTable {
    type Error = DbError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "chains" => Ok(CountTable::Chains),
            "contracts" => Ok(CountTable::Contracts),
            "blocks" => Ok(CountTable::Blocks),
            "transactions" => Ok(CountTable::Transactions),
            "agent_deposits" => Ok(CountTable::AgentDeposits),
            "agent_policies" => Ok(CountTable::AgentPolicies),
            "vault_snapshots" => Ok(CountTable::VaultSnapshots),
            "wallet_positions" => Ok(CountTable::WalletPositions),
            "indexer_runs" => Ok(CountTable::IndexerRuns),
            "vaults" => Ok(CountTable::Vaults),
            "vault_status_events" => Ok(CountTable::VaultStatusEvents),
            "governance_proposals" => Ok(CountTable::GovernanceProposals),
            "governance_votes" => Ok(CountTable::GovernanceVotes),
            "router_weight_snapshots" => Ok(CountTable::RouterWeightSnapshots),
            "router_deposit_legs" => Ok(CountTable::RouterDepositLegs),
            "adapter_allocations" => Ok(CountTable::AdapterAllocations),
            "vault_fee_events" => Ok(CountTable::VaultFeeEvents),
            "vault_transfer_events" => Ok(CountTable::VaultTransferEvents),
            "account_history_events" => Ok(CountTable::AccountHistoryEvents),
            "committee_agents" => Ok(CountTable::CommitteeAgents),
            "committee_votes" => Ok(CountTable::CommitteeVotes),
            "regime_snapshots" => Ok(CountTable::RegimeSnapshots),
            "consensus_receipts" => Ok(CountTable::ConsensusReceipts),
            other => Err(DbError::UnknownTable(other.to_owned())),
        }
    }
}

/// Marker column that makes a table **block-scoped**: every row belongs to one
/// block of one chain, so an orphaned block must take its rows with it.
///
/// Issue #1283: this marker replaces the hand-copied table literal that
/// `delete_above_block` used to iterate. Migration 0014 added `committee_votes`
/// and `regime_snapshots` without touching that literal, and a reorg left both
/// behind — exactly the drift `schema.rs` had predicted in a comment. A comment
/// is not a mechanism; the live schema is.
const BLOCK_SCOPE_COLUMN: &str = "block_number";

/// Chain-scope column every block-scoped table must also carry, so the rollback
/// DELETE can be confined to the chain that reorged.
const CHAIN_SCOPE_COLUMN: &str = "chain_id";

/// Block-scoped tables that are deliberately **not** rolled back by
/// [`Db::delete_above_block`], each with the reason it is exempt.
///
/// This is the only escape hatch from the schema-derived set. It is an
/// allowlist, not a to-do list: an entry here means "these rows legitimately
/// survive an orphaned block", and the reason string must say why. A table that
/// is merely inconvenient to roll back does not belong here.
///
/// **Empty today.** Every table in the live schema that carries `block_number`
/// also rolls back with its block. The test
/// `reorg_rollback_exclusions_are_documented_and_live`
/// (`tests/reorg_cursor_vault_status.rs`) fails on an entry with no stated
/// reason, one that no longer matches a live block-scoped table, or one the
/// rollback covers anyway — so a stale exemption cannot sit here unnoticed.
pub const REORG_ROLLBACK_EXCLUSIONS: &[(&str, &str)] = &[];

/// A catalog identifier is only interpolated into SQL after this check.
///
/// The names come from the Postgres catalog rather than from a caller, so this
/// is defence in depth, not the primary control — but `delete_above_block` is
/// the one place in the crate that builds DDL-shaped SQL from a runtime string,
/// and it stays as narrow as [`Db::count`]'s [`CountTable`] allowlist.
fn is_safe_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Order `tables` so that every child is deleted before the parent it
/// references (Kahn's algorithm over the live foreign-key graph).
///
/// `governance_votes` has an FK to `governance_proposals` and both are
/// block-scoped: deleting the proposals first raises a foreign-key violation
/// when a vote above the reorg root still points at them. The old literal
/// encoded that ordering by hand; deriving it from `pg_constraint` means the
/// next migration that adds an FK between two block-scoped tables is ordered
/// correctly without anyone remembering to reorder the list.
///
/// Ties are broken by name so the emitted order is deterministic across runs.
/// A cycle (which no ordering could satisfy) leaves residue; it is appended in
/// name order so Postgres reports the real constraint rather than this function
/// hiding it behind a hang or a panic.
fn fk_child_first_order(tables: &[String], edges: &[(String, String)]) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};

    let known: BTreeSet<&str> = tables.iter().map(String::as_str).collect();

    // child -> parents it references, and parent -> how many distinct children
    // are still pending deletion.
    let mut parents_of: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut pending_children: BTreeMap<&str, usize> =
        tables.iter().map(|t| (t.as_str(), 0usize)).collect();

    for (child, parent) in edges {
        let (c, p) = (child.as_str(), parent.as_str());
        if c == p || !known.contains(c) || !known.contains(p) {
            continue;
        }
        if parents_of.entry(c).or_default().insert(p) {
            *pending_children.entry(p).or_insert(0) += 1;
        }
    }

    let mut ready: BTreeSet<&str> = pending_children
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(t, _)| *t)
        .collect();
    let mut ordered: Vec<String> = Vec::with_capacity(tables.len());

    while let Some(next) = ready.iter().next().copied() {
        ready.remove(next);
        ordered.push(next.to_string());
        for parent in parents_of.get(next).into_iter().flatten() {
            if let Some(n) = pending_children.get_mut(*parent) {
                *n -= 1;
                if *n == 0 {
                    ready.insert(parent);
                }
            }
        }
    }

    // FK cycle (or a table unreachable by the walk): keep it in the rollback
    // rather than silently dropping it.
    let emitted: BTreeSet<String> = ordered.iter().cloned().collect();
    for t in tables {
        if !emitted.contains(t) {
            ordered.push(t.clone());
        }
    }
    ordered
}

/// Derive, from the live schema, the tables `delete_above_block` must clear,
/// in a foreign-key-safe deletion order.
///
/// A table qualifies when it is a base table in the current schema with a
/// [`BLOCK_SCOPE_COLUMN`] column, and it is not listed in
/// [`REORG_ROLLBACK_EXCLUSIONS`]. A qualifying table with no
/// [`CHAIN_SCOPE_COLUMN`] is an error, not a silent skip: the rollback could
/// not confine its DELETE to the reorged chain, and quietly leaving the table
/// out would recreate issue #1283.
async fn derive_rollback_tables(conn: &mut sqlx::PgConnection) -> Result<Vec<String>, DbError> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT c.table_name::text, \
                COALESCE(bool_or(c.column_name = $2), FALSE) \
         FROM information_schema.columns c \
         JOIN information_schema.tables t \
           ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
         WHERE c.table_schema = current_schema() \
           AND t.table_type = 'BASE TABLE' \
         GROUP BY c.table_name \
         HAVING bool_or(c.column_name = $1) \
         ORDER BY c.table_name",
    )
    .bind(BLOCK_SCOPE_COLUMN)
    .bind(CHAIN_SCOPE_COLUMN)
    .fetch_all(&mut *conn)
    .await?;

    let mut tables: Vec<String> = Vec::with_capacity(rows.len());
    for (name, has_chain_id) in rows {
        if REORG_ROLLBACK_EXCLUSIONS.iter().any(|(t, _)| *t == name) {
            continue;
        }
        if !is_safe_identifier(&name) {
            return Err(DbError::UnknownTable(name));
        }
        if !has_chain_id {
            return Err(DbError::UnscopedBlockTable(name));
        }
        tables.push(name);
    }

    let edges: Vec<(String, String)> = sqlx::query_as(
        "SELECT child.relname::text, parent.relname::text \
         FROM pg_constraint con \
         JOIN pg_class child  ON child.oid  = con.conrelid \
         JOIN pg_class parent ON parent.oid = con.confrelid \
         JOIN pg_namespace ns ON ns.oid     = child.relnamespace \
         WHERE con.contype = 'f' AND ns.nspname = current_schema()",
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(fk_child_first_order(&tables, &edges))
}

impl Db {
    pub async fn connect(database_url: &str) -> Result<Self, DbError> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(10))
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<(), DbError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Highest version recorded in `_sqlx_migrations`, or `None` when nothing
    /// has ever been migrated here (the table does not exist, or is empty).
    ///
    /// The existence probe is a separate statement because Postgres parses the
    /// whole query up front: `SELECT MAX(version) FROM _sqlx_migrations` against
    /// a virgin database is a 42P01 parse error, not a NULL.
    pub async fn applied_schema_version(&self) -> Result<Option<i64>, DbError> {
        let table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations')::text")
                .fetch_one(&self.pool)
                .await?;
        if table.is_none() {
            return Ok(None);
        }
        let applied: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&self.pool)
            .await?;
        Ok(applied)
    }

    /// Every row of `_sqlx_migrations`, or an empty vector when nothing has ever
    /// been migrated here (the table does not exist, or is empty).
    ///
    /// Issue #1429: the guard needs the applied **set**, not its maximum. The
    /// existence probe is a separate statement for the same reason as in
    /// [`Db::applied_schema_version`] — Postgres parses the whole query up
    /// front, so selecting from `_sqlx_migrations` on a virgin database is a
    /// 42P01 parse error, not an empty result.
    pub async fn applied_migrations(&self) -> Result<Vec<AppliedMigration>, DbError> {
        let table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations')::text")
                .fetch_one(&self.pool)
                .await?;
        if table.is_none() {
            return Ok(Vec::new());
        }
        let rows: Vec<(i64, Vec<u8>)> =
            sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(version, checksum)| AppliedMigration { version, checksum })
            .collect())
    }

    /// Refuse to proceed unless the database's applied migration set matches the
    /// set this binary embeds, version **and** checksum. Returns the agreed
    /// highest version on success.
    ///
    /// Issue #1392 established the guard; issue #1429 widened it from a
    /// comparison of maxima to the full set. See [`compare_schema`] for the four
    /// disagreement shapes and why each is named separately, and
    /// [`SchemaDivergence`] for what the original comparison could not see.
    ///
    /// The indexer still never writes schema (#1359) — it declines to run
    /// against one it does not match.
    pub async fn assert_schema_matches_embedded(&self) -> Result<i64, DbError> {
        let applied = self.applied_migrations().await?;
        Ok(compare_schema(&applied, &MIGRATOR.migrations)?)
    }

    /// Idempotent insert for the `chains` row.
    pub async fn upsert_chain(
        &self,
        chain_id: i64,
        name: &str,
        rpc_label: &str,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO chains (chain_id, name, rpc_label) VALUES ($1, $2, $3) \
             ON CONFLICT (chain_id) DO NOTHING",
        )
        .bind(chain_id)
        .bind(name)
        .bind(rpc_label)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Idempotent insert for a watched contract row.
    pub async fn upsert_contract(
        &self,
        chain_id: i64,
        address: [u8; 20],
        kind: &str,
        deployed_block: Option<i64>,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO contracts (chain_id, address, kind, deployed_block) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (chain_id, address) DO NOTHING",
        )
        .bind(chain_id)
        .bind(&address[..])
        .bind(kind)
        .bind(deployed_block)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Idempotent block insert. Returns rows_affected so the caller can
    /// distinguish "new block" vs "already had it".
    pub async fn insert_block(
        &self,
        chain_id: i64,
        block_number: i64,
        hash: [u8; 32],
        parent_hash: [u8; 32],
        timestamp: i64,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO blocks (chain_id, block_number, hash, parent_hash, timestamp) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (chain_id, block_number) DO NOTHING",
        )
        .bind(chain_id)
        .bind(block_number)
        .bind(&hash[..])
        .bind(&parent_hash[..])
        .bind(timestamp)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    pub async fn get_block_hash(
        &self,
        chain_id: i64,
        block_number: i64,
    ) -> Result<Option<[u8; 32]>, DbError> {
        let row: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT hash FROM blocks WHERE chain_id = $1 AND block_number = $2")
                .bind(chain_id)
                .bind(block_number)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(v,)| v.try_into().ok()))
    }

    /// The tables [`Db::delete_above_block`] will clear, in the order it will
    /// clear them, derived from the live schema.
    ///
    /// Exposed so the reorg coverage tests can assert on the *actual* rollback
    /// set rather than on a copy of it — a test that restated the list would
    /// drift the same way the list did (issue #1283).
    pub async fn block_scoped_rollback_tables(&self) -> Result<Vec<String>, DbError> {
        let mut conn = self.pool.acquire().await?;
        derive_rollback_tables(&mut conn).await
    }

    /// Reorg recovery: delete every row with block_number > `root` for
    /// the given chain. Per ADR §3.3, this is preferred over soft-delete.
    ///
    /// **The table set is derived from the live schema, not hand-maintained**
    /// (issue #1283). [`derive_rollback_tables`] asks the catalog which base
    /// tables carry `block_number`, subtracts [`REORG_ROLLBACK_EXCLUSIONS`],
    /// and orders the result child-before-parent over the live foreign-key
    /// graph. The predecessor of this code iterated a literal list; migration
    /// 0014 added `committee_votes` and `regime_snapshots` without updating it,
    /// so a reorg past a `VoteSubmitted` left both tables serving orphaned rows
    /// as current state. Deriving the set means a new block-scoped table is
    /// covered by the migration that creates it.
    ///
    /// Reorg-rollback cascade: remove every block-numbered row strictly above
    /// `root` for this chain, then reconcile the two derived states that are
    /// **not** plain block-numbered rows:
    ///
    /// - **`vaults.status` (IDX-8)** — status is mutated in place, so it cannot
    ///   be deleted by block number.  After pruning the block-numbered
    ///   `vault_status_events` history above `root`, the live `vaults.status` /
    ///   `status_changed_at` are re-derived from the surviving highest-block
    ///   status event per vault (falling back to 0 = Active / NULL when no
    ///   status event survives, i.e. the registration default).
    /// - **`indexer_runs.last_indexed_block` (IDX-2)** — the durable cursor is
    ///   `MAX(last_indexed_block) WHERE error IS NULL`.  Any successful run that
    ///   recorded a cursor above `root` indexed now-orphaned blocks, so its
    ///   recorded cursor is capped down to `root` (or NULL when `root < 0`).
    ///   This makes the cursor reorg-safe even if the run that performed the
    ///   rollback subsequently fails: the next [`Db::last_indexed_block`]
    ///   resolves to `root`, so indexing resumes at `root + 1` with no gap.
    pub async fn delete_above_block(&self, chain_id: i64, root: i64) -> Result<u64, DbError> {
        let mut tx = self.pool.begin().await?;

        // Issue #1283: the table set is DERIVED from the live schema inside the
        // same transaction as the deletes, never hand-copied. Any migration that
        // adds a `block_number` table is covered the moment it has been applied.
        let tables = derive_rollback_tables(&mut tx).await?;

        let mut total = 0u64;
        for table in &tables {
            let q = format!(
                "DELETE FROM \"{}\" WHERE chain_id = $1 AND block_number > $2",
                table
            );
            let r = sqlx::query(&q)
                .bind(chain_id)
                .bind(root)
                .execute(&mut *tx)
                .await?;
            total += r.rows_affected();
        }

        // IDX-8: re-derive the in-place vaults.status from the surviving
        // (block_number <= root) status history.  A vault whose latest event
        // was rolled back reverts to its prior surviving status; a vault whose
        // every status event was rolled back reverts to the registration
        // default (status 0 = Active, status_changed_at NULL).
        sqlx::query(
            "UPDATE vaults v SET \
                 status = COALESCE(latest.status, 0), \
                 status_changed_at = latest.changed_at, \
                 indexed_at = now() \
             FROM ( \
                 SELECT v2.vault_address AS vault_address, e.status AS status, e.changed_at AS changed_at \
                 FROM vaults v2 \
                 LEFT JOIN LATERAL ( \
                     SELECT status, changed_at \
                     FROM vault_status_events e2 \
                     WHERE e2.chain_id = v2.chain_id \
                       AND e2.vault_address = v2.vault_address \
                     ORDER BY e2.block_number DESC, e2.log_index DESC \
                     LIMIT 1 \
                 ) e ON true \
                 WHERE v2.chain_id = $1 \
             ) latest \
             WHERE v.chain_id = $1 AND v.vault_address = latest.vault_address",
        )
        .bind(chain_id)
        .execute(&mut *tx)
        .await?;

        // Issue #1247 (docs/architecture.md §4.9): `consensus_receipts.released`
        // is an in-place mutation written by `ReceiptReleased`, exactly like
        // `vaults.status` above.  A reorg that rolls back the release block but
        // NOT the record block leaves a surviving row whose `released` flag came
        // from an orphaned log — the dapp would then show a receipt as released
        // that the canonical chain never released.  Un-release every surviving
        // row whose release landed above `root`.  The row itself is never
        // deleted here: an unreleased receipt stays an immutable public record
        // (architecture §4.9.1 answer 3).
        sqlx::query(
            "UPDATE consensus_receipts SET \
                 released = FALSE, \
                 released_at = NULL, \
                 released_block_number = NULL, \
                 released_by = NULL, \
                 indexed_at = now() \
             WHERE chain_id = $1 AND released_block_number > $2",
        )
        .bind(chain_id)
        .bind(root)
        .execute(&mut *tx)
        .await?;

        // IDX-2: cap any successful-run cursor that points above the rolled-back
        // root down to root (NULL when root < 0 — a full wipe), so the next
        // last_indexed_block() resolves to root regardless of whether the run
        // that triggered this rollback later fails.
        let capped: Option<i64> = if root < 0 { None } else { Some(root) };
        sqlx::query(
            "UPDATE indexer_runs SET last_indexed_block = $2 \
             WHERE chain_id = $1 AND error IS NULL AND last_indexed_block > $3",
        )
        .bind(chain_id)
        .bind(capped)
        .bind(root)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(total)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_transaction(
        &self,
        chain_id: i64,
        tx_hash: [u8; 32],
        block_number: i64,
        tx_index: i32,
        from_addr: [u8; 20],
        to_addr: Option<[u8; 20]>,
        status: i16,
    ) -> Result<u64, DbError> {
        let to_bytes: Option<&[u8]> = to_addr.as_ref().map(|a| &a[..]);
        let r = sqlx::query(
            "INSERT INTO transactions (chain_id, tx_hash, block_number, tx_index, from_addr, to_addr, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (chain_id, tx_hash) DO NOTHING",
        )
        .bind(chain_id)
        .bind(&tx_hash[..])
        .bind(block_number)
        .bind(tx_index)
        .bind(&from_addr[..])
        .bind(to_bytes)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Insert a row into `agent_deposits`.
    ///
    /// `vault` is the vault address the deposit was routed to.  For
    /// single-vault `AgentDeposit` events, pass the gateway's pinned vault
    /// address.  For multi-leg `AgentDepositRouted` events, pass `None`
    /// (per-leg vault addresses are stored in `router_deposit_legs`).
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_agent_deposit(
        &self,
        chain_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        payment_id: [u8; 32],
        order_id: [u8; 32],
        agent: [u8; 20],
        share_receiver: [u8; 20],
        amount: U256,
        shares_minted: U256,
        window_id: i64,
        vault: Option<[u8; 20]>,
    ) -> Result<u64, DbError> {
        let vault_bytes: Option<&[u8]> = vault.as_ref().map(|a| &a[..]);
        let r = sqlx::query(
            "INSERT INTO agent_deposits (chain_id, block_number, log_index, tx_hash, payment_id, order_id, agent, share_receiver, amount, shares_minted, window_id, vault) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             ON CONFLICT (chain_id, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&payment_id[..])
        .bind(&order_id[..])
        .bind(&agent[..])
        .bind(&share_receiver[..])
        .bind(u256_to_decimal(amount))
        .bind(u256_to_decimal(shares_minted))
        .bind(window_id)
        .bind(vault_bytes)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Insert a per-leg row into `router_deposit_legs` from a `RouterDeposit`
    /// event emitted by `PortfolioRouter`.  Uses `ON CONFLICT DO NOTHING` so
    /// re-indexing the same range is a no-op (issue #57 AC).
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_router_deposit_leg(
        &self,
        chain_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        payment_id: [u8; 32],
        depositor: [u8; 20],
        vault: [u8; 20],
        amount: U256,
        shares: U256,
        weight_bps: U256,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO router_deposit_legs \
             (chain_id, block_number, log_index, tx_hash, payment_id, depositor, vault, amount, shares, weight_bps) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (chain_id, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&payment_id[..])
        .bind(&depositor[..])
        .bind(&vault[..])
        .bind(u256_to_decimal(amount))
        .bind(u256_to_decimal(shares))
        .bind(u256_to_decimal(weight_bps))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_agent_policy(
        &self,
        chain_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        agent: [u8; 20],
        owner: Option<[u8; 20]>,
        revoked: bool,
        valid_until: Option<i64>,
        max_per_payment: Option<U256>,
        max_per_window: Option<U256>,
        window_usage_to_date: Option<U256>,
        share_receiver: Option<[u8; 20]>,
    ) -> Result<u64, DbError> {
        let sr_bytes: Option<&[u8]> = share_receiver.as_ref().map(|a| &a[..]);
        let owner_bytes: Option<&[u8]> = owner.as_ref().map(|a| &a[..]);
        let r = sqlx::query(
            "INSERT INTO agent_policies \
             (chain_id, block_number, log_index, tx_hash, agent, owner, revoked, valid_until, \
              max_per_payment, max_per_window, window_usage_to_date, share_receiver) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             ON CONFLICT (chain_id, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&agent[..])
        .bind(owner_bytes)
        .bind(revoked)
        .bind(valid_until)
        .bind(max_per_payment.map(u256_to_decimal))
        .bind(max_per_window.map(u256_to_decimal))
        .bind(window_usage_to_date.map(u256_to_decimal))
        .bind(sr_bytes)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_vault_snapshot(
        &self,
        chain_id: i64,
        contract: [u8; 20],
        block_number: i64,
        total_assets: U256,
        total_supply: U256,
        exit_fee_bps: i64,
        tvl_cap: U256,
        paused: bool,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO vault_snapshots (chain_id, contract, block_number, total_assets, total_supply, exit_fee_bps, tvl_cap, paused) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (chain_id, contract, block_number) DO NOTHING",
        )
        .bind(chain_id)
        .bind(&contract[..])
        .bind(block_number)
        .bind(u256_to_decimal(total_assets))
        .bind(u256_to_decimal(total_supply))
        .bind(exit_fee_bps)
        .bind(u256_to_decimal(tvl_cap))
        .bind(paused)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    pub async fn insert_wallet_position(
        &self,
        chain_id: i64,
        contract: [u8; 20],
        owner: [u8; 20],
        block_number: i64,
        shares: U256,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO wallet_positions (chain_id, contract, owner, block_number, shares) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (chain_id, contract, owner, block_number) DO NOTHING",
        )
        .bind(chain_id)
        .bind(&contract[..])
        .bind(&owner[..])
        .bind(block_number)
        .bind(u256_to_decimal(shares))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Most-recent `wallet_positions.shares` for an `(chain_id, contract, owner)`
    /// trio at or below `as_of_block`, i.e. the owner's running share balance
    /// just before a new event at `as_of_block` is applied.  Returns `None`
    /// (treated as a zero balance by callers) when the owner has no prior
    /// position row.
    ///
    /// IDX-3: `insert_wallet_position` writes a per-event *resulting balance*
    /// snapshot, so the indexer needs the prior balance to roll the delta
    /// forward.  Bounding by `as_of_block` keeps the read deterministic if
    /// events arrive out of block order within a tick.
    pub async fn latest_wallet_position(
        &self,
        chain_id: i64,
        contract: [u8; 20],
        owner: [u8; 20],
        as_of_block: i64,
    ) -> Result<Option<U256>, DbError> {
        let row: Option<(BigDecimal,)> = sqlx::query_as(
            "SELECT shares FROM wallet_positions \
             WHERE chain_id = $1 AND contract = $2 AND owner = $3 AND block_number <= $4 \
             ORDER BY block_number DESC LIMIT 1",
        )
        .bind(chain_id)
        .bind(&contract[..])
        .bind(&owner[..])
        .bind(as_of_block)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(d,)| decimal_to_u256(&d)))
    }

    /// Idempotent insert of a vault registration row sourced from a
    /// `VaultRegistered` event.  Uses `ON CONFLICT DO NOTHING` so
    /// re-indexing the same registration block is a no-op.
    ///
    /// `status` is the small integer encoding of `VaultStatus`:
    /// 0 = Active, 1 = Paused, 2 = Retired.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_vault(
        &self,
        chain_id: i64,
        vault_address: [u8; 20],
        name: &str,
        risk_label: &str,
        deposit_cap: U256,
        status: i16,
        registered_at: i64,
        registered_block: i64,
        registered_tx: [u8; 32],
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO vaults \
             (chain_id, vault_address, name, risk_label, deposit_cap, status, \
              registered_at, registered_block, registered_tx) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (chain_id, vault_address) DO NOTHING",
        )
        .bind(chain_id)
        .bind(&vault_address[..])
        .bind(name)
        .bind(risk_label)
        .bind(u256_to_decimal(deposit_cap))
        .bind(status)
        .bind(registered_at)
        .bind(registered_block)
        .bind(&registered_tx[..])
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Record a `VaultStatusChanged` event and refresh the live
    /// `vaults.status` / `status_changed_at` columns from it.
    ///
    /// IDX-8: the transition is appended to the block-numbered, append-only
    /// `vault_status_events` history (so a reorg can roll it back via
    /// [`Db::delete_above_block`]) *and* the in-place `vaults` columns are
    /// updated, in a single transaction.  The history row is idempotent on
    /// `(chain_id, vault_address, block_number, log_index)`, so re-indexing
    /// the same event is a no-op.
    ///
    /// Returns the number of `vaults` rows updated.  A `vaults` update is a
    /// no-op if the vault address is unknown (forward-safety: the indexer may
    /// not have seen the `VaultRegistered` event yet) — but the history event
    /// is still recorded so a later registration can re-derive correctly.
    pub async fn update_vault_status(
        &self,
        chain_id: i64,
        vault_address: [u8; 20],
        block_number: i64,
        log_index: i32,
        new_status: i16,
        changed_at: i64,
    ) -> Result<u64, DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO vault_status_events \
             (chain_id, vault_address, block_number, log_index, status, changed_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (chain_id, vault_address, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(&vault_address[..])
        .bind(block_number)
        .bind(log_index)
        .bind(new_status)
        .bind(changed_at)
        .execute(&mut *tx)
        .await?;
        let r = sqlx::query(
            "UPDATE vaults \
             SET status = $3, status_changed_at = $4, indexed_at = now() \
             WHERE chain_id = $1 AND vault_address = $2",
        )
        .bind(chain_id)
        .bind(&vault_address[..])
        .bind(new_status)
        .bind(changed_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(r.rows_affected())
    }

    /// Idempotent insert of a governance proposal row from a `ProposalCreated`
    /// event.  Uses `ON CONFLICT DO NOTHING` so re-indexing is a no-op.
    ///
    /// `status` is initialised to 0 (open); it is updated by subsequent
    /// `ProposalExecuted` events via [`Db::execute_proposal`].
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_proposal(
        &self,
        chain_id: i64,
        proposal_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        proposer: [u8; 20],
        description: &str,
        created_at: i64,
        deadline_block: i64,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO governance_proposals \
             (chain_id, proposal_id, block_number, log_index, tx_hash, proposer, description, created_at, deadline_block) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (chain_id, proposal_id) DO NOTHING",
        )
        .bind(chain_id)
        .bind(proposal_id)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&proposer[..])
        .bind(description)
        .bind(created_at)
        .bind(deadline_block)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Idempotent insert of a per-voter vote row from a `VoteCast` event.
    /// Uses `ON CONFLICT DO NOTHING` (one vote per voter per proposal).
    ///
    /// Also adds the vote's `weight` (voting power) to the running `votes_for`
    /// or `votes_against` tally on the parent `governance_proposals` row — but
    /// only when the vote row is new (rows_affected == 1).  The tally sums
    /// power, not voter count (IDX-5).
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_vote(
        &self,
        chain_id: i64,
        proposal_id: i64,
        voter: [u8; 20],
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        support: bool,
        weight: U256,
    ) -> Result<u64, DbError> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query(
            "INSERT INTO governance_votes \
             (chain_id, proposal_id, voter, block_number, log_index, tx_hash, support, weight) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (chain_id, proposal_id, voter) DO NOTHING",
        )
        .bind(chain_id)
        .bind(proposal_id)
        .bind(&voter[..])
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(support)
        .bind(u256_to_decimal(weight))
        .execute(&mut *tx)
        .await?;

        if r.rows_affected() == 1 {
            // IDX-5: sum the vote's *power* (weight) into the running tally,
            // not `+ 1` per voter — the tally must reflect voting power so it
            // agrees with on-chain quorum/threshold math.  The columns are
            // NUMERIC(78, 0) (migration 0013) to hold a U256 power sum.
            let col = if support {
                "votes_for"
            } else {
                "votes_against"
            };
            let q = format!(
                "UPDATE governance_proposals SET {} = {} + $3 \
                 WHERE chain_id = $1 AND proposal_id = $2",
                col, col
            );
            sqlx::query(&q)
                .bind(chain_id)
                .bind(proposal_id)
                .bind(u256_to_decimal(weight))
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(r.rows_affected())
    }

    /// Mark a proposal as executed (status = 2) from a `ProposalExecuted`
    /// event.  A no-op if the proposal is unknown or already executed.
    pub async fn execute_proposal(
        &self,
        chain_id: i64,
        proposal_id: i64,
        executed_block: i64,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "UPDATE governance_proposals \
             SET status = 2, executed_block = $3, indexed_at = now() \
             WHERE chain_id = $1 AND proposal_id = $2 AND status < 2",
        )
        .bind(chain_id)
        .bind(proposal_id)
        .bind(executed_block)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Idempotent insert of a `WeightsApplied` snapshot row.
    /// `vault_addresses` and `bps_values` are parallel arrays.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_router_weight_snapshot(
        &self,
        chain_id: i64,
        router_address: [u8; 20],
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        vault_addresses: Vec<[u8; 20]>,
        bps_values: Vec<i64>,
    ) -> Result<u64, DbError> {
        // Encode each vault address as a Postgres BYTEA literal so we can
        // pass the whole array via a single placeholder.
        let vault_bytes: Vec<Vec<u8>> = vault_addresses.into_iter().map(|a| a.to_vec()).collect();
        let r = sqlx::query(
            "INSERT INTO router_weight_snapshots \
             (chain_id, router_address, block_number, log_index, tx_hash, vault_addresses, bps_values) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (chain_id, router_address, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(&router_address[..])
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&vault_bytes)
        .bind(&bps_values)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Insert a row into `account_history_events` (issue #654).
    ///
    /// One row per source log, keyed by `(chain_id, block_number, log_index)`.
    /// `ON CONFLICT DO NOTHING` keeps re-indexing idempotent.
    ///
    /// `kind` must be one of: `"deposit"`, `"withdrawal"`, `"fee_charged"`,
    /// `"policy_change"`, `"governance_vote"`.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_history_event(
        &self,
        chain_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        account: [u8; 20],
        kind: &str,
        vault: Option<[u8; 20]>,
        agent: Option<[u8; 20]>,
        amount: Option<U256>,
    ) -> Result<u64, DbError> {
        let vault_bytes: Option<&[u8]> = vault.as_ref().map(|a| &a[..]);
        let agent_bytes: Option<&[u8]> = agent.as_ref().map(|a| &a[..]);
        let r = sqlx::query(
            "INSERT INTO account_history_events \
             (chain_id, block_number, log_index, tx_hash, account, kind, vault, agent, amount) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (chain_id, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&account[..])
        .bind(kind)
        .bind(vault_bytes)
        .bind(agent_bytes)
        .bind(amount.map(u256_to_decimal))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Open a new `indexer_runs` row, return the surrogate `run_id`.
    pub async fn start_run(&self, chain_id: i64, from_block: i64) -> Result<i64, DbError> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO indexer_runs (chain_id, from_block) VALUES ($1, $2) RETURNING run_id",
        )
        .bind(chain_id)
        .bind(from_block)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Close a run. `error` is `Some(...)` on failure, `None` on success.
    pub async fn finish_run(
        &self,
        run_id: i64,
        to_block: Option<i64>,
        last_indexed_block: Option<i64>,
        reorg_count: i32,
        rows_inserted: i64,
        error: Option<&str>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE indexer_runs SET finished_at = now(), to_block = $2, last_indexed_block = $3, reorg_count = $4, rows_inserted = $5, error = $6 \
             WHERE run_id = $1",
        )
        .bind(run_id)
        .bind(to_block)
        .bind(last_indexed_block)
        .bind(reorg_count)
        .bind(rows_inserted)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Highest `last_indexed_block` written by any successful run for
    /// this chain.
    pub async fn last_indexed_block(&self, chain_id: i64) -> Result<Option<i64>, DbError> {
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT MAX(last_indexed_block) FROM indexer_runs WHERE chain_id = $1 AND error IS NULL",
        )
        .bind(chain_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(v,)| v))
    }

    /// Most recent run (success or failure) for ops tooling.
    pub async fn latest_run(&self, chain_id: i64) -> Result<Option<RunRow>, DbError> {
        let row: Option<RunRow> = sqlx::query_as(
            "SELECT run_id, chain_id, started_at, finished_at, from_block, to_block, last_indexed_block, reorg_count, rows_inserted, error \
             FROM indexer_runs WHERE chain_id = $1 ORDER BY run_id DESC LIMIT 1",
        )
        .bind(chain_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Insert a row into `adapter_allocations` from a `VaultAllocated`,
    /// `VaultPulled`, or `VaultRebalanced` event.
    /// Uses `ON CONFLICT DO NOTHING` for idempotency (issue #57 AC).
    ///
    /// `adapter` and `adapter_index` may be `None` for `Rebalanced` events
    /// (which carry only a `totalMoved` amount).
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_adapter_allocation(
        &self,
        chain_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        vault: [u8; 20],
        adapter: Option<[u8; 20]>,
        adapter_index: Option<i64>,
        amount: U256,
        event_kind: &str,
    ) -> Result<u64, DbError> {
        let adapter_bytes: Option<&[u8]> = adapter.as_ref().map(|a| &a[..]);
        let r = sqlx::query(
            "INSERT INTO adapter_allocations \
             (chain_id, block_number, log_index, tx_hash, vault, adapter, adapter_index, amount, event_kind) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (chain_id, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&vault[..])
        .bind(adapter_bytes)
        .bind(adapter_index)
        .bind(u256_to_decimal(amount))
        .bind(event_kind)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Insert a row into `vault_fee_events` from an `ExitFeeCharged` event.
    /// Uses `ON CONFLICT DO NOTHING` for idempotency (issue #57 AC).
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_vault_fee_event(
        &self,
        chain_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        vault: [u8; 20],
        owner: [u8; 20],
        receiver: [u8; 20],
        gross_assets: U256,
        fee_amount: U256,
        net_assets: U256,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO vault_fee_events \
             (chain_id, block_number, log_index, tx_hash, vault, owner, receiver, \
              gross_assets, fee_amount, net_assets) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (chain_id, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&vault[..])
        .bind(&owner[..])
        .bind(&receiver[..])
        .bind(u256_to_decimal(gross_assets))
        .bind(u256_to_decimal(fee_amount))
        .bind(u256_to_decimal(net_assets))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Insert a row into `vault_transfer_events` from an ERC-4626
    /// `Deposit` or `Withdraw` event emitted by a vault contract.
    /// Uses `ON CONFLICT DO NOTHING` for idempotency (issue #57 AC).
    ///
    /// `direction` must be `"deposit"` or `"withdrawal"`.
    /// `owner_or_receiver` is the `owner` field for Deposit events and
    /// the `receiver` field for Withdraw events.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_vault_transfer_event(
        &self,
        chain_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        vault: [u8; 20],
        direction: &str,
        caller: [u8; 20],
        owner_or_receiver: [u8; 20],
        assets: U256,
        shares: U256,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO vault_transfer_events \
             (chain_id, block_number, log_index, tx_hash, vault, direction, \
              caller, owner_or_receiver, assets, shares) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (chain_id, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&vault[..])
        .bind(direction)
        .bind(&caller[..])
        .bind(&owner_or_receiver[..])
        .bind(u256_to_decimal(assets))
        .bind(u256_to_decimal(shares))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    // ─── InvestmentCommitteePolicy writers ───────────────────────────────────
    // Canonical: docs/architecture.md §5.4 — issue #1053.
    //
    // Three events drive three tables:
    //   AgentRegistered → upsert_committee_agent (sets active=true)
    //   AgentRevoked    → revoke_committee_agent (sets active=false)
    //   VoteSubmitted   → insert_committee_vote + upsert_regime_snapshot

    /// Upsert a committee agent row on AgentRegistered.
    ///
    /// Uses ON CONFLICT DO UPDATE so that re-indexing the same event is
    /// idempotent, and so that a previously-revoked agent re-registered
    /// with ADMIN_ROLE gets `active` flipped back to true.
    pub async fn upsert_committee_agent(
        &self,
        chain_id: i64,
        agent_address: [u8; 20],
        agent_id: &str,
        registered_at: i64,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO committee_agents \
               (chain_id, agent_address, agent_id, active, registered_at) \
             VALUES ($1, $2, $3, TRUE, $4) \
             ON CONFLICT (chain_id, agent_address) DO UPDATE \
               SET agent_id = EXCLUDED.agent_id, \
                   active   = TRUE, \
                   registered_at = EXCLUDED.registered_at, \
                   revoked_at    = NULL, \
                   indexed_at    = now()",
        )
        .bind(chain_id)
        .bind(&agent_address[..])
        .bind(agent_id)
        .bind(registered_at)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Mark a committee agent as revoked on AgentRevoked.
    ///
    /// No-ops gracefully if the agent row does not exist yet (the
    /// AgentRegistered event may have been missed on a partial reindex).
    pub async fn revoke_committee_agent(
        &self,
        chain_id: i64,
        agent_address: [u8; 20],
        revoked_at: i64,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "UPDATE committee_agents \
             SET active = FALSE, revoked_at = $3, indexed_at = now() \
             WHERE chain_id = $1 AND agent_address = $2",
        )
        .bind(chain_id)
        .bind(&agent_address[..])
        .bind(revoked_at)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Insert a committee vote row on VoteSubmitted.
    ///
    /// `verified` is set by the caller after memo fetch + keccak256 check.
    /// Uses ON CONFLICT DO NOTHING for idempotent re-indexing.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_committee_vote(
        &self,
        chain_id: i64,
        vote_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        agent_address: [u8; 20],
        vault_address: [u8; 20],
        stance: i16,
        target_weight_bps: i16,
        confidence: i16,
        rationale_uri: &str,
        vote_json_hash: [u8; 32],
        timestamp_secs: i64,
        verified: bool,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO committee_votes \
               (chain_id, vote_id, block_number, log_index, tx_hash, \
                agent_address, vault_address, stance, target_weight_bps, \
                confidence, rationale_uri, vote_json_hash, timestamp_secs, verified) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) \
             ON CONFLICT (chain_id, vote_id) DO NOTHING",
        )
        .bind(chain_id)
        .bind(vote_id)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(&agent_address[..])
        .bind(&vault_address[..])
        .bind(stance)
        .bind(target_weight_bps)
        .bind(confidence)
        .bind(rationale_uri)
        .bind(&vote_json_hash[..])
        .bind(timestamp_secs)
        .bind(verified)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Upsert a regime snapshot for (chain_id, vault_address, block_number).
    ///
    /// Called after every verified VoteSubmitted to keep the aggregate
    /// (avg_weight_bps, vote_count) current. Uses ON CONFLICT DO UPDATE
    /// so that re-indexing is idempotent.
    pub async fn upsert_regime_snapshot(
        &self,
        chain_id: i64,
        vault_address: [u8; 20],
        block_number: i64,
        avg_weight_bps: f64,
        vote_count: i32,
    ) -> Result<u64, DbError> {
        let avg_bps = BigDecimal::from_str(&format!("{avg_weight_bps:.2}"))
            .unwrap_or_else(|_| BigDecimal::from(0));
        let r = sqlx::query(
            "INSERT INTO regime_snapshots \
               (chain_id, vault_address, block_number, avg_weight_bps, vote_count) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (chain_id, vault_address, block_number) DO UPDATE \
               SET avg_weight_bps = EXCLUDED.avg_weight_bps, \
                   vote_count     = EXCLUDED.vote_count, \
                   indexed_at     = now()",
        )
        .bind(chain_id)
        .bind(&vault_address[..])
        .bind(block_number)
        .bind(avg_bps)
        .bind(vote_count)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Compute verified vote aggregate for a vault and upsert the regime
    /// snapshot row.  Returns the number of rows affected (0 or 1).
    pub async fn refresh_regime_snapshot(
        &self,
        chain_id: i64,
        vault_address: [u8; 20],
        block_number: i64,
    ) -> Result<u64, DbError> {
        // Aggregate over all verified votes for this vault.
        let row: Option<(Option<bigdecimal::BigDecimal>, Option<i64>)> = sqlx::query_as(
            "SELECT AVG(target_weight_bps::numeric), COUNT(*) \
             FROM committee_votes \
             WHERE chain_id = $1 AND vault_address = $2 AND verified = TRUE",
        )
        .bind(chain_id)
        .bind(&vault_address[..])
        .fetch_optional(&self.pool)
        .await?;
        let (avg, cnt) = row.unwrap_or((None, None));
        let avg_f: f64 = avg
            .and_then(|a| a.to_string().parse::<f64>().ok())
            .unwrap_or(0.0);
        let cnt_i = cnt.unwrap_or(0) as i32;
        if cnt_i == 0 {
            return Ok(0);
        }
        self.upsert_regime_snapshot(chain_id, vault_address, block_number, avg_f, cnt_i)
            .await
    }

    // ─── End InvestmentCommitteePolicy writers ────────────────────────────────

    // ─── ConsensusRecommendationReceipt writers ────────────────────────────────────
    // Canonical: docs/architecture.md §4.9 — issue #1247.
    //
    // Two events drive one table:
    //   ReceiptRecorded → insert_consensus_receipt
    //   ReceiptReleased → mark_consensus_receipt_released (in-place flip)

    /// Insert a consensus receipt commitment row on `ReceiptRecorded`.
    ///
    /// `verified` is set by the caller after fetching `payload_uri` and
    /// recomputing `keccak256(body) == payload_digest`.  It records **digest**
    /// verification only — per-analyst ed25519 verification of the payload's
    /// embedded signatures is `rmpc`'s job at submit time (architecture §4.9.1)
    /// and a future indexer pass.
    ///
    /// Uses `ON CONFLICT DO NOTHING` so re-indexing the same range is a no-op.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_consensus_receipt(
        &self,
        chain_id: i64,
        receipt_id: [u8; 32],
        receipt_index: i64,
        submitter: [u8; 20],
        payload_digest: [u8; 32],
        payload_uri: &str,
        recorded_at: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        verified: bool,
        payload_bytes: Option<i64>,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "INSERT INTO consensus_receipts \
               (chain_id, receipt_id, receipt_index, submitter, payload_digest, \
                payload_uri, recorded_at, block_number, log_index, tx_hash, \
                verified, payload_bytes) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
             ON CONFLICT (chain_id, receipt_id) DO NOTHING",
        )
        .bind(chain_id)
        .bind(&receipt_id[..])
        .bind(receipt_index)
        .bind(&submitter[..])
        .bind(&payload_digest[..])
        .bind(payload_uri)
        .bind(recorded_at)
        .bind(block_number)
        .bind(log_index)
        .bind(&tx_hash[..])
        .bind(verified)
        .bind(payload_bytes)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Flip a recorded receipt to released on `ReceiptReleased`.
    ///
    /// No-ops gracefully when the row does not exist (the `ReceiptRecorded`
    /// event may not have been indexed yet on a partial re-index).
    /// `released_block_number` is stored so [`Db::delete_above_block`] can
    /// un-release a receipt whose release landed on an orphaned block.
    pub async fn mark_consensus_receipt_released(
        &self,
        chain_id: i64,
        receipt_id: [u8; 32],
        released_by: [u8; 20],
        released_at: i64,
        block_number: i64,
    ) -> Result<u64, DbError> {
        let r = sqlx::query(
            "UPDATE consensus_receipts SET \
                 released = TRUE, \
                 released_at = $3, \
                 released_block_number = $4, \
                 released_by = $5, \
                 indexed_at = now() \
             WHERE chain_id = $1 AND receipt_id = $2",
        )
        .bind(chain_id)
        .bind(&receipt_id[..])
        .bind(released_at)
        .bind(block_number)
        .bind(&released_by[..])
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    // ─── End ConsensusRecommendationReceipt writers ────────────────────────────────

    /// Row count for any of the nine §11 tables.
    ///
    /// Accepts a [`CountTable`] enum value instead of a raw `&str` so
    /// no caller — current or future — can accidentally pass a
    /// user-controlled string into the dynamic `FORMAT` statement
    /// (issue #165).  `sqlx` does not support placeholder-binding for
    /// identifiers, so the `format!()` is unavoidable; the enum
    /// closes the injection surface at the type level.
    pub async fn count(&self, table: CountTable) -> Result<i64, DbError> {
        let q = format!("SELECT COUNT(*)::BIGINT FROM {}", table.as_str());
        let row: (i64,) = sqlx::query_as(&q).fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    // -----------------------------------------------------------------------
    // Dev-scout stubs — issue #703
    // -----------------------------------------------------------------------
    // The function signatures below are the integration points for the four
    // data-layer issues that extend this service.  Each stub is annotated
    // with the implementing issue number, the migration file that creates its
    // target table, and the query pattern it must execute.
    //
    // All stubs compile, panic at runtime if called, and will be replaced by
    // real implementations in their respective feature branches.
    // -----------------------------------------------------------------------

    // NOTE: The dev-scout (#703) provided #654 stubs for
    // `insert_account_history_event` and `list_account_history` here. Issue #654
    // (this PR) implements account history for real: rows are written via
    // `insert_history_event` (above) and read by the explorer API's raw query in
    // `clients/explorer-api/src/routes.rs` (`get_account_history`). The scout
    // stubs and their placeholder `AccountHistoryRow` type were removed during
    // the rebase to avoid dead, schema-stale definitions.

    // NOTE: The dev-scout (#703) provided #675 stubs for
    // `insert_adapter_allocation`, `insert_vault_fee_event`, and
    // `insert_vault_transfer_event` here. Issue #675 implements them for real
    // earlier in this impl block; the stubs were removed during the rebase to
    // avoid duplicate definitions.

    /// Insert an agent policy row, including the `owner` and `window_usage_to_date`
    /// fields added by migration 0007 (issue #661).
    ///
    /// # Target issue: #661 — GET /v1/accounts/:address/policies endpoint
    ///
    /// The existing `insert_agent_policy` (above) does not accept `owner` or
    /// `window_usage_to_date`. Issue #661 adds those columns to the migration
    /// and updates the existing function signature — OR adds a new
    /// `insert_agent_policy_v2` — to pass them.
    ///
    /// This stub documents the *intended* extended signature so issue #661
    /// implementors have a concrete target.
    ///
    /// Migration: `0007_account_history_and_vault_detail_stubs.sql`
    ///   ALTER TABLE agent_policies ADD COLUMN IF NOT EXISTS owner BYTEA
    ///   ALTER TABLE agent_policies ADD COLUMN IF NOT EXISTS window_usage_to_date NUMERIC(78,0)
    ///
    /// Explorer API consumer:
    ///   `GET /v1/accounts/:address/policies` (new route, issue #661)
    ///   Query: SELECT … FROM agent_policies WHERE chain_id = $1 AND owner = $2
    ///           ORDER BY block_number DESC
    #[allow(dead_code, unused_variables, clippy::too_many_arguments)]
    pub async fn insert_agent_policy_with_owner(
        &self,
        chain_id: i64,
        block_number: i64,
        log_index: i32,
        tx_hash: [u8; 32],
        agent: [u8; 20],
        owner: [u8; 20],
        revoked: bool,
        valid_until: Option<i64>,
        max_per_payment: Option<U256>,
        max_per_window: Option<U256>,
        window_usage_to_date: Option<U256>,
        share_receiver: Option<[u8; 20]>,
    ) -> Result<u64, DbError> {
        // STUB — replaced by issue #661.
        // Real implementation inserts owner and window_usage_to_date into
        // the agent_policies row. Issue #661 may choose to:
        //   (a) add parameters to the existing insert_agent_policy, or
        //   (b) keep this as a separate function for the AgentAuthorized-with-owner path.
        // The ABI drift fix in issue #366 is a prerequisite: AgentAuthorized currently
        // drops `address indexed owner` (abi.rs drift map, § IGatewayEvents).
        unimplemented!("stub — implement in issue #661 (requires issue #366 ABI fix first)")
    }

    /// List all agent policies owned by a depositor address.
    ///
    /// # Target issue: #661 — GET /v1/accounts/:address/policies endpoint
    ///
    /// Query pattern (after issue #661's migration lands):
    ///   SELECT DISTINCT ON (chain_id, agent) * FROM agent_policies
    ///   WHERE chain_id = $1 AND owner = $2
    ///   ORDER BY chain_id, agent, block_number DESC
    ///
    /// Returns the latest policy state per agent for the given owner.
    #[allow(dead_code, unused_variables)]
    pub async fn list_policies_by_owner(
        &self,
        chain_id: i64,
        owner: [u8; 20],
    ) -> Result<Vec<AgentPolicyRow>, DbError> {
        // STUB — replaced by issue #661.
        unimplemented!("stub — implement in issue #661")
    }
}

/// Stub row type for `list_policies_by_owner` — issue #661.
///
/// The real implementation extends the existing `agent_policies` columns with
/// `owner` and `window_usage_to_date` from migration 0007.
#[derive(Debug, Clone)]
pub struct AgentPolicyRow {
    pub chain_id: i64,
    pub block_number: i64,
    pub log_index: i32,
    pub tx_hash: Vec<u8>,
    pub agent: Vec<u8>,
    pub owner: Option<Vec<u8>>,
    pub revoked: bool,
    pub valid_until: Option<i64>,
    pub max_per_payment: Option<BigDecimal>,
    pub max_per_window: Option<BigDecimal>,
    pub window_usage_to_date: Option<BigDecimal>,
    pub share_receiver: Option<Vec<u8>>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RunRow {
    pub run_id: i64,
    pub chain_id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub from_block: i64,
    pub to_block: Option<i64>,
    pub last_indexed_block: Option<i64>,
    pub reorg_count: i32,
    pub rows_inserted: i64,
    pub error: Option<String>,
}

/// uint256 → exact decimal `NUMERIC(78, 0)` value. ADR §3.1.
fn u256_to_decimal(v: U256) -> BigDecimal {
    // U256::to_string is the decimal representation; BigDecimal parses
    // it losslessly.
    BigDecimal::from_str(&v.to_string()).expect("U256 always parses as BigDecimal")
}

/// Exact decimal `NUMERIC(78, 0)` value → uint256. ADR §3.1. The inverse of
/// [`u256_to_decimal`]: `wallet_positions.shares` is a non-negative integer
/// share balance, so the value round-trips through its base-10 integer string.
/// A negative or out-of-range value saturates to `U256::ZERO` rather than
/// panicking (defensive — the writer never persists such a value).
fn decimal_to_u256(d: &BigDecimal) -> U256 {
    // `with_scale(0)` drops any (always-absent for share balances) fractional
    // part; the resulting `to_string()` is the base-10 integer, which U256
    // parses losslessly.
    U256::from_str(&d.with_scale(0).to_string()).unwrap_or(U256::ZERO)
}

#[cfg(test)]
mod count_guard_tests {
    //! Unit tests for the `CountTable` type-guard / allowlist (issue #695).
    //!
    //! These tests run without a live Postgres connection — they only exercise
    //! the pure-Rust allowlist logic.

    use super::{CountTable, DbError};

    /// Every known table name round-trips: `CountTable::try_from(name)` succeeds
    /// and `as_str()` returns the same name.
    #[test]
    fn known_tables_round_trip() {
        let known = [
            "chains",
            "contracts",
            "blocks",
            "transactions",
            "agent_deposits",
            "agent_policies",
            "vault_snapshots",
            "wallet_positions",
            "indexer_runs",
            "vaults",
            "vault_status_events",
            "governance_proposals",
            "governance_votes",
            "router_weight_snapshots",
            "router_deposit_legs",
            "committee_agents",
            "committee_votes",
            "regime_snapshots",
            // Migration 0015 — issue #1247.
            "consensus_receipts",
        ];
        for name in known {
            let variant = CountTable::try_from(name)
                .unwrap_or_else(|_| panic!("known table {name:?} was rejected by allowlist"));
            assert_eq!(
                variant.as_str(),
                name,
                "as_str() for {name:?} must match the input"
            );
        }
    }

    /// An arbitrary / user-controlled string must be rejected with
    /// `DbError::UnknownTable` — it must never reach the SQL formatter.
    #[test]
    fn unknown_table_returns_err() {
        let adversarial = [
            "DROP TABLE chains",
            "chains; DROP TABLE blocks --",
            "wallets",
            "users",
            "pg_tables",
            "",
            " chains",
            "CHAINS",
        ];
        for s in adversarial {
            match CountTable::try_from(s) {
                Err(DbError::UnknownTable(rejected)) => {
                    assert_eq!(
                        rejected, s,
                        "error payload must echo back the rejected string"
                    );
                }
                Ok(v) => panic!("expected Err for {s:?} but got Ok({:?})", v.as_str()),
                Err(other) => panic!("unexpected error variant for {s:?}: {other}"),
            }
        }
    }
}
