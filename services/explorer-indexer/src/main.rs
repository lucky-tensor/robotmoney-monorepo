//! Canonical: docs/architecture.md §5.4 — Explorer Indexer and API
//! `indexer` — long-running poll loop. Wraps `run_once` in a tokio
//! interval; press Ctrl-C to stop. For one-shot bounded ingestion
//! (e.g. CI), use `--once` and `--end-block`.
//!
//! Schema migration is a SEPARATE invocation, not a side effect of booting
//! the poll loop (issue #1359). `indexer --migrate-only` connects to
//! `DATABASE_URL`, runs the embedded migrator, and exits — 0 on success,
//! non-zero on failure — without requiring any of the chain-watching
//! arguments. The normal boot path never migrates, so a broken migration
//! fails a deployment's explicit migrate step instead of crash-looping a
//! long-running container against a half-migrated database.

use alloy_primitives::Address;
use clap::Parser;
use explorer_indexer::{
    db::Db, feature_flags, indexer::run_once, indexer::IndexerConfig, rpc::JsonRpc,
    DEFAULT_TICK_SECONDS,
};
use std::str::FromStr;
use std::time::Duration;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(name = "indexer", about = "Robot Money explorer indexer (Phase 5)")]
struct Cli {
    /// Postgres connection URL (e.g. postgres://indexer:pw@localhost:5432/explorer).
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Run the embedded schema migrations against `--database-url` and
    /// exit. Exits 0 when every migration applied, non-zero otherwise.
    /// None of the chain-watching arguments are required in this mode.
    #[arg(long, default_value_t = false)]
    migrate_only: bool,

    /// JSON-RPC URL for the chain to index.
    /// Required unless `--migrate-only`.
    #[arg(
        long,
        env = "INDEXER_RPC_URL",
        required_unless_present = "migrate_only"
    )]
    rpc_url: Option<String>,

    /// Chain id (8453 for Base mainnet).
    #[arg(long, env = "INDEXER_CHAIN_ID", default_value_t = 8453)]
    chain_id: i64,

    /// Human-readable chain name. Stored on the `chains` row.
    #[arg(long, env = "INDEXER_CHAIN_NAME", default_value = "base")]
    chain_name: String,

    /// Sanitized RPC label (no API key) for `chains.rpc_label`.
    #[arg(long, env = "INDEXER_RPC_LABEL", default_value = "unknown")]
    rpc_label: String,

    /// Watched gateway address. Required unless `--migrate-only`.
    #[arg(
        long,
        env = "INDEXER_GATEWAY",
        required_unless_present = "migrate_only"
    )]
    gateway: Option<String>,

    /// Watched vault address. Required unless `--migrate-only`.
    #[arg(long, env = "INDEXER_VAULT", required_unless_present = "migrate_only")]
    vault: Option<String>,

    /// Optional VaultRegistry contract address.  When set, the indexer
    /// ingests VaultRegistered and VaultStatusChanged events from this
    /// contract on every tick.
    #[arg(long, env = "INDEXER_REGISTRY")]
    registry: Option<String>,

    /// Optional PortfolioRouter / RouterGovernance contract address.
    /// When set, the indexer ingests ProposalCreated, VoteCast,
    /// ProposalExecuted, and WeightsApplied events.
    #[arg(long, env = "INDEXER_ROUTER_GOVERNANCE")]
    router_governance: Option<String>,

    /// Optional PortfolioRouter contract address (may differ from RouterGovernance).
    /// When set, the indexer ingests WeightsSet and DefaultWeightsSet events
    /// emitted by direct admin calls to setWeights() / setDefaultWeights()
    /// (the demo-seed path), and watches RouterDeposit events to take a fresh
    /// TVL snapshot of all registered vaults in any block where a router
    /// deposit is processed.
    #[arg(long, env = "INDEXER_PORTFOLIO_ROUTER")]
    portfolio_router: Option<String>,

    /// Optional InvestmentCommitteePolicy contract address.
    /// When set, the indexer ingests AgentRegistered, AgentRevoked, and
    /// VoteSubmitted events, performs memo keccak256 verification, and
    /// writes to committee_agents, committee_votes, and regime_snapshots
    /// tables (issue #1053).
    #[arg(long, env = "INDEXER_INVESTMENT_COMMITTEE")]
    investment_committee: Option<String>,

    /// Optional ConsensusRecommendationReceipt contract address.
    /// When set, the indexer ingests ReceiptRecorded and ReceiptReleased
    /// events, fetches each receipt's payloadUri to recompute its keccak256
    /// digest, and writes to the consensus_receipts table
    /// (issue #1247, docs/architecture.md §4.9).
    #[arg(long, env = "INDEXER_CONSENSUS_RECEIPT")]
    consensus_receipt: Option<String>,

    /// Tick interval in seconds (default 12, ADR §3.2).
    #[arg(long, env = "INDEXER_TICK_SECONDS", default_value_t = DEFAULT_TICK_SECONDS)]
    tick_seconds: u64,

    /// Hard cap on per-tick block range.
    #[arg(long, env = "INDEXER_MAX_BLOCKS_PER_TICK", default_value_t = 1000)]
    max_blocks_per_tick: u64,

    /// Optional explicit upper-bound block. When set, the indexer
    /// stops after reaching this block — useful for bounded test runs.
    #[arg(long, env = "INDEXER_END_BLOCK")]
    end_block: Option<u64>,

    /// Run a single tick and exit. Useful in scripted/CI flows where
    /// the long-running daemon is unwanted.
    #[arg(long, default_value_t = false)]
    once: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Canonical: docs/technical/security-model.md §14 — Process — audit & institutional memory. All Rust
    // binaries route through the workspace-shared logging facade so
    // operator output is byte-for-byte consistent with `rmpc` and
    // `explorer-api` (issue #247).
    if let Err(e) = rmpc_logging::init_service("explorer-indexer") {
        // Bootstrap failure path — facade is not installed; emit one
        // stderr line so the operator can diagnose the boot crash.
        eprintln!("explorer-indexer: logging init failed: {e}");
        return Err(e.into());
    }

    let cli = Cli::parse();

    let db = Db::connect(&cli.database_url).await?;

    // Issue #1359: migration is its own invocation. `--migrate-only` runs the
    // embedded migrator and returns; any migration error propagates out of
    // `main` as a non-zero exit so the deploy's migrate step fails loudly.
    if cli.migrate_only {
        db.migrate().await?;
        info!("migrations applied; exiting (--migrate-only)");
        return Ok(());
    }

    // The normal boot path deliberately does NOT migrate — the schema must
    // already have been migrated by a prior `--migrate-only` run.
    //
    // clap enforces these three via `required_unless_present = "migrate_only"`,
    // so the `ok_or` arms are unreachable on the normal path; they exist so a
    // future CLI edit that drops the constraint fails with a message instead
    // of a panic.
    let rpc_url = cli
        .rpc_url
        .ok_or("--rpc-url (INDEXER_RPC_URL) is required unless --migrate-only")?;
    let gateway = cli
        .gateway
        .ok_or("--gateway (INDEXER_GATEWAY) is required unless --migrate-only")?;
    let vault = cli
        .vault
        .ok_or("--vault (INDEXER_VAULT) is required unless --migrate-only")?;

    let rpc = JsonRpc::new(&rpc_url);

    let registry = cli
        .registry
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Address::from_str(s.trim_start_matches("0x")))
        .transpose()?;

    let router_governance = cli
        .router_governance
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Address::from_str(s.trim_start_matches("0x")))
        .transpose()?;

    let portfolio_router = cli
        .portfolio_router
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Address::from_str(s.trim_start_matches("0x")))
        .transpose()?;

    let investment_committee = cli
        .investment_committee
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Address::from_str(s.trim_start_matches("0x")))
        .transpose()?;

    let consensus_receipt = cli
        .consensus_receipt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Address::from_str(s.trim_start_matches("0x")))
        .transpose()?;

    let cfg = IndexerConfig {
        chain_id: cli.chain_id,
        chain_name: cli.chain_name,
        rpc_label: cli.rpc_label,
        gateway: Address::from_str(gateway.trim_start_matches("0x"))?,
        vault: Address::from_str(vault.trim_start_matches("0x"))?,
        registry,
        router_governance,
        portfolio_router,
        investment_committee,
        consensus_receipt,
        max_blocks_per_tick: cli.max_blocks_per_tick,
        end_block: cli.end_block,
        // Load feature flags from FEATURE_FLAGS env var at startup.
        // config/feature-flags.json is the canonical registry.
        feature_flags: feature_flags::bitmap_from_env(),
    };

    let mut interval = tokio::time::interval(Duration::from_secs(cli.tick_seconds));

    loop {
        match run_once(&db, &rpc, &cfg).await {
            Ok(o) => {
                if let Some(ref err) = o.error {
                    error!(run_id = o.run_id, error = %err, "tick error (will retry)");
                }
                info!(
                    run_id = o.run_id,
                    last_indexed_block = ?o.last_indexed_block,
                    rows = o.rows_inserted,
                    reorg = o.reorg_detected,
                    "tick complete"
                );
                if cli.once {
                    return Ok(());
                }
                if let (Some(end), Some(li)) = (cli.end_block, o.last_indexed_block) {
                    if li as u64 >= end {
                        info!("end_block reached, exiting");
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "tick failed; will retry");
                if cli.once {
                    return Err(e.into());
                }
            }
        }
        interval.tick().await;
    }
}
