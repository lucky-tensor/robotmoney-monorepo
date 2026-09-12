//! Canonical: docs/architecture.md §5.4 — Explorer Indexer and API
//! Indexer orchestration. The hot path is `run_once`: one tick of the
//! poll loop, factored out so integration tests can drive it directly
//! without a long-running daemon.
//!
//! Sequence per ADR §3.2 / §3.3 / §3.5:
//!
//! 1. Open an `indexer_runs` row.
//! 2. Fetch `eth_blockNumber`; cap the safe head at `tip - CONFIRMATIONS`.
//! 3. Reorg check: compare stored hash for `last_indexed_block` against
//!    the chain's hash at the same height. On mismatch walk back, then
//!    `DELETE WHERE block_number > root` and reset `last_indexed_block`.
//! 4. For the range `[last+1, target]`: fetch all watched logs, fetch
//!    each block (header+txs), upsert blocks/transactions/events.
//! 5. State snapshots — for every contract whose events touched a block
//!    in this range, take a snapshot at that block. Apply heartbeat
//!    snapshot if the last snapshot is more than
//!    `SNAPSHOT_HEARTBEAT_BLOCKS` behind.
//! 6. Close the run with `last_indexed_block = target`.
//!
//! All errors short-circuit and write to the run's `error` column;
//! `last_indexed_block` is left at the last block we successfully
//! committed, so the next run resumes there.

use crate::abi::{
    IConsensusRecommendationReceiptEvents, IGatewayEvents, IInvestmentCommitteePolicyEvents,
    IPortfolioRouterEvents, IRouterGovernanceEvents, IVaultEvents, IVaultReads,
    IVaultRegistryEvents, Topics,
};
use crate::db::{Db, DbError};
use crate::rpc::{JsonRpc, LogEntry, RpcError};
use crate::{CONFIRMATIONS, SNAPSHOT_HEARTBEAT_BLOCKS};
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolEvent};
use std::collections::BTreeSet;

#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("decode: {0}")]
    Decode(String),
}

#[derive(Debug, Clone)]
pub struct IndexerConfig {
    pub chain_id: i64,
    pub chain_name: String,
    pub rpc_label: String,
    /// Watched gateway address (one per chain).
    pub gateway: Address,
    /// Watched vault address (one per chain, legacy single-vault config).
    pub vault: Address,
    /// Optional on-chain VaultRegistry contract address.  When set, the
    /// indexer calls `listVaults()` on each tick and ingests
    /// `VaultRegistered` / `VaultStatusChanged` events from the registry.
    pub registry: Option<Address>,
    /// Optional PortfolioRouter / RouterGovernance contract address.
    /// When set, the indexer ingests `ProposalCreated`, `VoteCast`,
    /// `ProposalExecuted`, and `WeightsApplied` events.
    pub router_governance: Option<Address>,
    /// Optional PortfolioRouter contract address (may differ from
    /// `router_governance`).  When set, the indexer ingests `WeightsSet`
    /// and `DefaultWeightsSet` events emitted by direct admin calls to
    /// `setWeights()` / `setDefaultWeights()` (the demo-seed path), and
    /// watches `RouterDeposit` events to take a fresh TVL snapshot of every
    /// registered vault in any block where a router deposit is processed —
    /// ensuring deposits via the router are reflected in the TVL index without
    /// waiting for the SNAPSHOT_HEARTBEAT_BLOCKS interval.
    pub portfolio_router: Option<Address>,
    /// Optional InvestmentCommitteePolicy contract address.
    /// When set, the indexer ingests `AgentRegistered`, `AgentRevoked`, and
    /// `VoteSubmitted` events and writes to `committee_agents`,
    /// `committee_votes`, and `regime_snapshots` tables (issue #1053).
    pub investment_committee: Option<Address>,
    /// Optional ConsensusRecommendationReceipt contract address.
    /// When set, the indexer ingests `ReceiptRecorded` and `ReceiptReleased`
    /// events, fetches each receipt's `payloadUri` to recompute its keccak256
    /// digest, and writes to the `consensus_receipts` table
    /// (issue #1247, docs/architecture.md §4.9).
    pub consensus_receipt: Option<Address>,
    /// Hard cap on per-tick block range. Protects against an unbounded
    /// `eth_getLogs` request when the indexer is far behind tip.
    pub max_blocks_per_tick: u64,
    /// Optional explicit upper bound — useful for bounded test runs.
    /// When `Some(end)`, the indexer never advances past `end`.
    pub end_block: Option<u64>,
    /// Feature flag bitmap loaded from `FEATURE_FLAGS` env var via
    /// `feature_flags::bitmap_from_env()`.  Bit positions are defined in
    /// `config/feature-flags.json` and `feature_flags.rs`.  Default `0`
    /// disables all optional paths (conservative / backwards-compatible).
    pub feature_flags: u64,
}

impl IndexerConfig {
    pub fn watched_addresses(&self) -> Vec<Address> {
        let mut addrs = vec![self.gateway, self.vault];
        // Gate VaultRegistry event ingestion behind INDEXER_MULTI_VAULT_EVENTS
        // (config/feature-flags.json id=2).
        if crate::feature_flags::is_enabled(
            crate::feature_flags::INDEXER_MULTI_VAULT_EVENTS,
            self.feature_flags,
        ) {
            if let Some(reg) = self.registry {
                addrs.push(reg);
            }
        }
        if let Some(gov) = self.router_governance {
            addrs.push(gov);
        }
        if let Some(pr) = self.portfolio_router {
            // Only add if not already present (e.g. when portfolio_router == router_governance).
            if !addrs.contains(&pr) {
                addrs.push(pr);
            }
        }
        if let Some(ic) = self.investment_committee {
            if !addrs.contains(&ic) {
                addrs.push(ic);
            }
        }
        if let Some(cr) = self.consensus_receipt {
            if !addrs.contains(&cr) {
                addrs.push(cr);
            }
        }
        addrs
    }
}

#[derive(Debug, Clone, Default)]
pub struct IndexerOutcome {
    pub run_id: i64,
    pub from_block: i64,
    pub to_block: Option<i64>,
    pub last_indexed_block: Option<i64>,
    pub rows_inserted: i64,
    pub reorg_detected: bool,
    pub error: Option<String>,
}

/// One indexer tick. Returns the outcome (also written to `indexer_runs`).
pub async fn run_once(
    db: &Db,
    rpc: &JsonRpc,
    cfg: &IndexerConfig,
) -> Result<IndexerOutcome, IndexerError> {
    // Bookkeeping rows — chains/contracts must exist before any FK insert.
    db.upsert_chain(cfg.chain_id, &cfg.chain_name, &cfg.rpc_label)
        .await?;
    db.upsert_contract(cfg.chain_id, cfg.gateway.into_array(), "gateway", None)
        .await?;
    db.upsert_contract(cfg.chain_id, cfg.vault.into_array(), "vault", None)
        .await?;
    if let Some(reg) = cfg.registry {
        db.upsert_contract(cfg.chain_id, reg.into_array(), "vault_registry", None)
            .await?;
    }
    if let Some(gov) = cfg.router_governance {
        db.upsert_contract(cfg.chain_id, gov.into_array(), "router_governance", None)
            .await?;
    }
    if let Some(pr) = cfg.portfolio_router {
        // Register the PortfolioRouter as a watched contract so its events are
        // attributed correctly. Use "portfolio_router" type to distinguish from
        // the RouterGovernance contract.
        db.upsert_contract(cfg.chain_id, pr.into_array(), "portfolio_router", None)
            .await?;
    }
    if let Some(ic) = cfg.investment_committee {
        db.upsert_contract(cfg.chain_id, ic.into_array(), "investment_committee", None)
            .await?;
    }
    if let Some(cr) = cfg.consensus_receipt {
        db.upsert_contract(cfg.chain_id, cr.into_array(), "consensus_receipt", None)
            .await?;
    }

    let last_indexed = db.last_indexed_block(cfg.chain_id).await?;
    let from_block = last_indexed.map(|x| x + 1).unwrap_or(0);
    let run_id = db.start_run(cfg.chain_id, from_block).await?;

    // IDX-2: `run_inner` may roll a reorg back via `delete_above_block` and
    // *then* fail (e.g. an RPC error fetching the new range).  The corrected
    // post-rollback cursor must survive into the error arm — recording the
    // stale pre-reorg cursor here would leave the next run resuming above the
    // deleted blocks.  `run_inner` writes the post-rollback cursor into
    // `rollback_cursor` before any fallible step that follows the rollback, so
    // the error arm persists `root` (the durable cursor) rather than the
    // pre-reorg value captured above.
    let mut rollback_cursor = last_indexed;

    let outcome = match run_inner(db, rpc, cfg, last_indexed, &mut rollback_cursor).await {
        Ok(mut o) => {
            o.run_id = run_id;
            o.from_block = from_block;
            db.finish_run(
                run_id,
                o.to_block,
                o.last_indexed_block,
                if o.reorg_detected { 1 } else { 0 },
                o.rows_inserted,
                None,
            )
            .await?;
            o
        }
        Err(e) => {
            let msg = format!("{e}");
            // Persist the post-rollback cursor, not the pre-reorg one.  Note
            // this run row is `error IS NOT NULL` so it is excluded from the
            // `last_indexed_block()` MAX; the durable cursor reconciliation
            // happens inside `delete_above_block`, which caps any successful
            // run's cursor down to root.  Recording `rollback_cursor` here keeps
            // the failed run's audit row internally consistent with the rollback.
            db.finish_run(run_id, None, rollback_cursor, 0, 0, Some(&msg))
                .await?;
            IndexerOutcome {
                run_id,
                from_block,
                to_block: None,
                last_indexed_block: rollback_cursor,
                rows_inserted: 0,
                reorg_detected: false,
                error: Some(msg),
            }
        }
    };
    Ok(outcome)
}

async fn run_inner(
    db: &Db,
    rpc: &JsonRpc,
    cfg: &IndexerConfig,
    last_indexed: Option<i64>,
    rollback_cursor: &mut Option<i64>,
) -> Result<IndexerOutcome, IndexerError> {
    let topics = Topics::new();

    // Reorg check: compare stored hash for `last_indexed` against chain.
    let mut reorg_detected = false;
    let mut last_indexed = last_indexed;
    if let Some(li) = last_indexed {
        if let Some(stored_hash) = db.get_block_hash(cfg.chain_id, li).await? {
            if let Some(header) = rpc.block_header(li as u64).await? {
                if header.hash.0 != stored_hash {
                    let root = walk_back_to_match(db, rpc, cfg.chain_id, li).await?;
                    db.delete_above_block(cfg.chain_id, root).await?;
                    last_indexed = if root < 0 { None } else { Some(root) };
                    // IDX-2: publish the post-rollback cursor *immediately* after
                    // the rollback commits, so a later failure in this same run
                    // carries `root` into the error arm.
                    *rollback_cursor = last_indexed;
                    reorg_detected = true;
                }
            }
        }
    }

    let from_block = last_indexed.map(|x| x + 1).unwrap_or(0);

    let tip = rpc.block_number().await?;
    let safe_head = tip.saturating_sub(CONFIRMATIONS);
    if (from_block as u64) > safe_head {
        return Ok(IndexerOutcome {
            to_block: None,
            last_indexed_block: last_indexed,
            rows_inserted: 0,
            reorg_detected,
            ..Default::default()
        });
    }
    let mut target = safe_head;
    if let Some(e) = cfg.end_block {
        target = target.min(e);
    }
    let max_advance = (from_block as u64).saturating_add(cfg.max_blocks_per_tick - 1);
    target = target.min(max_advance);
    if (from_block as u64) > target {
        return Ok(IndexerOutcome {
            to_block: None,
            last_indexed_block: last_indexed,
            rows_inserted: 0,
            reorg_detected,
            ..Default::default()
        });
    }

    // Static watched set (gateway, pinned vault, registry, router) plus every
    // registry-discovered active vault. Without the latter, ERC-4626
    // `Deposit`/`Withdraw` logs emitted by vaults learned from `VaultRegistered`
    // are never fetched, so their burn volume is invisible to the watchdog
    // (scan finding IDX-6). `get_logs` filters server-side on the address set, so
    // a registered vault that emits a withdrawal in a block where the gateway is
    // idle would otherwise be dropped entirely.
    let mut watched = cfg.watched_addresses();
    let registered_vaults: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT vault_address FROM vaults WHERE chain_id = $1 AND status = 0 ORDER BY vault_address",
    )
    .bind(cfg.chain_id)
    .fetch_all(db.pool())
    .await
    .map_err(DbError::from)?;
    for vault in &registered_vaults {
        if let Ok(bytes) = <[u8; 20]>::try_from(vault.as_slice()) {
            let address = Address::from(bytes);
            if !watched.contains(&address) {
                watched.push(address);
            }
        }
    }
    let topic0 = topics.all_topic0();
    let logs = rpc
        .get_logs(from_block as u64, target, &watched, &topic0)
        .await?;

    // Group logs by (block_number, contract) so we know which blocks
    // need state snapshots per ADR §3.5 trigger 1.
    let mut event_blocks_per_contract: BTreeSet<(u64, Address)> = BTreeSet::new();
    let mut blocks_with_events: BTreeSet<u64> = BTreeSet::new();
    for log in &logs {
        event_blocks_per_contract.insert((log.block_number, log.address));
        blocks_with_events.insert(log.block_number);
    }

    let mut rows_inserted: i64 = 0;

    // Ingest blocks (and their txs) for every block we touch — only
    // those with at least one watched event for now, so we don't pull
    // every tx on Base. The §11 acceptance criterion says "each row
    // carries chain_id and block_number"; non-event blocks aren't
    // required by the schema.
    for &bn in &blocks_with_events {
        let (header, txs) = rpc.block_with_txs(bn).await?;
        let r = db
            .insert_block(
                cfg.chain_id,
                bn as i64,
                header.hash.0,
                header.parent_hash.0,
                header.timestamp as i64,
            )
            .await?;
        rows_inserted += r as i64;
        for t in txs {
            let r = db
                .insert_transaction(
                    cfg.chain_id,
                    t.tx_hash.0,
                    bn as i64,
                    t.tx_index as i32,
                    t.from.into_array(),
                    t.to.map(|a| a.into_array()),
                    t.status as i16,
                )
                .await?;
            rows_inserted += r as i64;
        }
    }

    // Always persist the cursor block header, even when `target` had no
    // watched events. Without a stored hash at `target`, the next tick
    // cannot perform a reorg check (the `get_block_hash` guard short-
    // circuits) and stale rows below a no-event cursor block would
    // survive a reorg undetected (issue #177).
    if !blocks_with_events.contains(&target) {
        if let Some(header) = rpc.block_header(target).await? {
            let r = db
                .insert_block(
                    cfg.chain_id,
                    target as i64,
                    header.hash.0,
                    header.parent_hash.0,
                    header.timestamp as i64,
                )
                .await?;
            rows_inserted += r as i64;
        }
    }

    // Decode + insert events.
    for log in &logs {
        rows_inserted += handle_log(db, cfg, &topics, log).await? as i64;
    }

    // Reuse the registered-vault set gathered above for the watched-address
    // union; it drives both the event-driven snapshot (router-deposit path) and
    // the heartbeat.
    let mut heartbeat_vaults = vec![cfg.vault];
    for vault in &registered_vaults {
        if let Ok(bytes) = <[u8; 20]>::try_from(vault.as_slice()) {
            let address = Address::from(bytes);
            if !heartbeat_vaults.contains(&address) {
                heartbeat_vaults.push(address);
            }
        }
    }

    // State snapshots — event-driven (one per touched vault contract per
    // touched block). Heartbeat handled below.
    //
    // For the primary vault: snapshot whenever it had an event.
    // For registered extra vaults: snapshot whenever the PortfolioRouter
    // had an event in that block — router deposits update TVL in extra
    // vaults that are not individually watched, so piggybacking on the
    // router's event blocks keeps their total_assets current without
    // waiting for the SNAPSHOT_HEARTBEAT_BLOCKS interval.
    let router_event_blocks: BTreeSet<u64> = match cfg.portfolio_router {
        Some(router) => event_blocks_per_contract
            .iter()
            .filter(|(_, c)| *c == router)
            .map(|(bn, _)| *bn)
            .collect(),
        None => BTreeSet::new(),
    };
    for (bn, contract) in &event_blocks_per_contract {
        if *contract == cfg.vault {
            rows_inserted += snapshot_vault_or_skip(db, rpc, cfg.chain_id, cfg.vault, *bn).await;
        }
    }
    // Snapshot all registered vaults for every block where the router processed deposits.
    for bn in &router_event_blocks {
        for vault in &heartbeat_vaults {
            rows_inserted += snapshot_vault_or_skip(db, rpc, cfg.chain_id, *vault, *bn).await;
        }
    }

    // Heartbeat snapshots — cover the legacy configured vault and every
    // active vault learned from VaultRegistry events. The PK on
    // (chain_id, contract, block_number) deduplicates against event-driven
    // snapshots.

    for vault in heartbeat_vaults {
        // Fetch both the last snapshot block and whether its total_assets was
        // zero so we can force a re-snapshot for vaults that were registered
        // before deposits arrived (e.g. extra demo vaults seeded via the
        // PortfolioRouter after an indexer that didn't watch it).
        let last_snap: Option<(i64, String)> = sqlx::query_as(
            "SELECT block_number, CAST(total_assets AS text) FROM vault_snapshots \
             WHERE chain_id = $1 AND contract = $2 \
             ORDER BY block_number DESC LIMIT 1",
        )
        .bind(cfg.chain_id)
        .bind(&vault.into_array()[..])
        .fetch_optional(db.pool())
        .await
        .map_err(DbError::from)?;
        let needs_heartbeat = match &last_snap {
            Some((prev, total_assets)) => {
                let behind = (target as i64 - prev) >= SNAPSHOT_HEARTBEAT_BLOCKS as i64;
                // Re-snapshot more aggressively when TVL shows zero — the
                // vault may have received deposits since the last snapshot.
                let zero_tvl = total_assets == "0" || total_assets.is_empty();
                behind || zero_tvl
            }
            None => true,
        };
        if needs_heartbeat {
            rows_inserted += snapshot_vault_or_skip(db, rpc, cfg.chain_id, vault, target).await;
        }
    }

    Ok(IndexerOutcome {
        to_block: Some(target as i64),
        last_indexed_block: Some(target as i64),
        rows_inserted,
        reorg_detected,
        ..Default::default()
    })
}

/// Walk back from `start` until we find a block whose stored hash
/// matches the on-chain hash. Returns that block number as the reorg
/// root. Returns `-1` if we walk past block 0 without finding a match,
/// which signals the caller to wipe all data (effectively a full
/// re-index).
///
/// A block with **no stored hash** is skipped — a missing hash means the
/// indexer never persisted this block (it had no watched events and was
/// not the cursor). Treating a missing-hash block as a "clean root"
/// would incorrectly stop the walk, leaving stale event rows below the
/// true reorg point undetected (issue #177 bug fix).
async fn walk_back_to_match(
    db: &Db,
    rpc: &JsonRpc,
    chain_id: i64,
    start: i64,
) -> Result<i64, IndexerError> {
    let mut n = start;
    while n >= 0 {
        let stored = db.get_block_hash(chain_id, n).await?;
        if let Some(stored) = stored {
            if let Some(header) = rpc.block_header(n as u64).await? {
                if header.hash.0 == stored {
                    return Ok(n);
                }
            }
            // Hash mismatch — keep walking back.
        }
        // No stored hash for this height — we never persisted this block
        // so we cannot validate it as a canonical root. Keep walking.
        n -= 1;
    }
    Ok(-1)
}

/// Dispatch a single decoded log to its writer(s).  Public so integration
/// tests can drive one synthetic event through the full handler-wiring path
/// (IDX-3 regression coverage) without booting a chain.
pub async fn handle_log(
    db: &Db,
    cfg: &IndexerConfig,
    topics: &Topics,
    log: &LogEntry,
) -> Result<u64, IndexerError> {
    let topic0 = match log.topics.first() {
        Some(t) => *t,
        None => return Ok(0),
    };

    if topic0 == topics.agent_deposit {
        let decoded = IGatewayEvents::AgentDeposit::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("AgentDeposit: {e}")))?;
        let mut r = db
            .insert_agent_deposit(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.paymentId.0,
                decoded.orderId.0,
                decoded.agent.into_array(),
                decoded.shareReceiver.into_array(),
                decoded.amount,
                decoded.sharesMinted,
                decoded.windowId as i64,
                // Single-vault path: the deposit went to the gateway's pinned vault.
                Some(cfg.vault.into_array()),
            )
            .await?;
        // Store in account_history_events for the share_receiver.
        r += db
            .insert_history_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.shareReceiver.into_array(),
                "deposit",
                Some(cfg.vault.into_array()),
                Some(decoded.agent.into_array()),
                Some(decoded.amount),
            )
            .await?;
        return Ok(r);
    }

    // AgentDepositRouted — multi-leg router deposit (IGateway.sol:119).
    // Stores a parent row in agent_deposits (vault = NULL; per-leg data is
    // written by the corresponding RouterDeposit events from PortfolioRouter).
    if topic0 == topics.agent_deposit_routed {
        let decoded = IGatewayEvents::AgentDepositRouted::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("AgentDepositRouted: {e}")))?;
        // Sum sharesPerLeg to populate shares_minted on the parent row.
        let total_shares: U256 = decoded
            .sharesPerLeg
            .iter()
            .copied()
            .fold(U256::ZERO, |acc, s| acc.saturating_add(s));
        let mut r = db
            .insert_agent_deposit(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.paymentId.0,
                decoded.orderId.0,
                decoded.agent.into_array(),
                decoded.shareReceiver.into_array(),
                decoded.amount,
                total_shares,
                decoded.windowId as i64,
                // Router path: vault is NULL; per-leg rows carry the vault address.
                None,
            )
            .await?;
        // Store in account_history_events for the share_receiver (vault=NULL for routed).
        r += db
            .insert_history_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.shareReceiver.into_array(),
                "deposit",
                None,
                Some(decoded.agent.into_array()),
                Some(decoded.amount),
            )
            .await?;
        return Ok(r);
    }

    // AgentWithdrawal — gateway-level withdrawal (IGateway.sol:139).
    // Stores a withdrawal history row for the agent address.
    // The ERC-4626 Withdraw event (emitted by the vault in the same tx)
    // is stored separately — both coexist because they have distinct log_index values.
    if topic0 == topics.agent_withdrawal {
        let decoded = IGatewayEvents::AgentWithdrawal::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("AgentWithdrawal: {e}")))?;
        let r = db
            .insert_history_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.agent.into_array(),
                "withdrawal",
                Some(decoded.sourceVault.into_array()),
                Some(decoded.agent.into_array()),
                Some(decoded.assetsOut),
            )
            .await?;
        return Ok(r);
    }

    if topic0 == topics.agent_authorized {
        let decoded = IGatewayEvents::AgentAuthorized::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("AgentAuthorized: {e}")))?;
        let mut r = db
            .insert_agent_policy(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.agent.into_array(),
                Some(decoded.owner.into_array()),
                false,
                Some(decoded.validUntil as i64),
                Some(decoded.maxPerPayment),
                Some(decoded.maxPerWindow),
                None, // window_usage_to_date not available from AgentAuthorized event
                Some(decoded.shareReceiver.into_array()),
            )
            .await?;
        // Store policy change in account history for the agent.
        r += db
            .insert_history_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.agent.into_array(),
                "policy_change",
                None,
                Some(decoded.agent.into_array()),
                None,
            )
            .await?;
        return Ok(r);
    }

    if topic0 == topics.agent_revoked {
        let decoded = IGatewayEvents::AgentRevoked::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("AgentRevoked: {e}")))?;
        let mut r = db
            .insert_agent_policy(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.agent.into_array(),
                Some(decoded.owner.into_array()),
                true,
                None,
                None,
                None,
                None, // window_usage_to_date not available from AgentRevoked event
                None,
            )
            .await?;
        // Store policy change (revocation) in account history for the agent.
        r += db
            .insert_history_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.agent.into_array(),
                "policy_change",
                None,
                Some(decoded.agent.into_array()),
                None,
            )
            .await?;
        return Ok(r);
    }

    // RouterDeposit — per-leg event from PortfolioRouter.sol:71.
    // Each leg records (depositor, vault, amount, shares, weightBps) for one
    // vault in the router's weight vector.  Legs are linked to the parent
    // AgentDepositRouted row via payment_id.
    //
    // Note: RouterDeposit carries no paymentId of its own — the payment_id
    // stored here is the tx_hash (best-effort correlation key) because the
    // PortfolioRouter does not forward the gateway's paymentId.  Callers
    // should join on (chain_id, tx_hash) to correlate with agent_deposits.
    if topic0 == topics.router_deposit {
        let decoded = IPortfolioRouterEvents::RouterDeposit::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("RouterDeposit: {e}")))?;
        // Use the tx_hash as the payment_id correlation key: PortfolioRouter
        // does not forward the gateway's paymentId, so the tx hash is the
        // best available link between leg rows and the parent deposit.
        let r = db
            .insert_router_deposit_leg(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                log.tx_hash.0, // payment_id = tx_hash (correlation key)
                decoded.depositor.into_array(),
                decoded.vault.into_array(),
                decoded.amount,
                decoded.shares,
                decoded.weightBps,
            )
            .await?;
        return Ok(r);
    }

    // ERC-4626 Withdraw — Withdraw(address indexed caller, address indexed receiver,
    //                              address indexed owner, uint256 assets, uint256 shares).
    // A single Withdraw log must persist BOTH:
    //   1. an account_history_events withdrawal row attributed to `owner` (issue #654, §5.4), and
    //   2. a vault_transfer_events row with direction='withdrawal' (issue #675 AC-2),
    //      which is the table the watchdog burn-volume circuit breaker reads.
    // These were previously split across two branches keyed on the same Withdraw
    // topic0; the first returned early, leaving the vault_transfer_events writer
    // unreachable and stranding the watchdog burn alarm (issue #989). They are now
    // merged into this single reachable branch.
    if topic0 == topics.erc4626_withdraw {
        let decoded = IVaultEvents::Withdraw::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("Withdraw: {e}")))?;
        let mut r = db
            .insert_history_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.owner.into_array(),
                "withdrawal",
                Some(log.address.into_array()),
                None,
                Some(decoded.assets),
            )
            .await?;
        r += db
            .insert_vault_transfer_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                log.address.into_array(),
                "withdrawal",
                decoded.caller.into_array(),
                decoded.receiver.into_array(),
                decoded.assets,
                decoded.shares,
            )
            .await?;
        // IDX-3: roll the owner's wallet_positions balance back by the burned
        // shares.  ERC-4626 `Withdraw` burns `shares` from `owner`; the new
        // balance is prior - shares (saturating at zero).
        r += persist_wallet_position(
            db,
            cfg,
            log,
            decoded.owner.into_array(),
            decoded.shares,
            false,
        )
        .await?;
        return Ok(r);
    }

    // VaultAllocated — Allocated(uint256 indexed index, address indexed adapter, uint256 amount).
    // Persists an adapter_allocations row so the vault detail API can surface
    // per-adapter allocation history (issue #675 AC-2).
    if topic0 == topics.vault_allocated {
        let decoded = IVaultEvents::Allocated::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("Allocated: {e}")))?;
        let r = db
            .insert_adapter_allocation(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                log.address.into_array(),
                Some(decoded.adapter.into_array()),
                Some(decoded.index.try_into().unwrap_or(i64::MAX)),
                decoded.amount,
                "allocated",
            )
            .await?;
        return Ok(r);
    }

    // VaultPulled — Pulled(uint256 indexed index, address indexed adapter, uint256 amount).
    // Mirrors VaultAllocated but records a withdrawal from the adapter back to the vault.
    if topic0 == topics.vault_pulled {
        let decoded = IVaultEvents::Pulled::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("Pulled: {e}")))?;
        let r = db
            .insert_adapter_allocation(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                log.address.into_array(),
                Some(decoded.adapter.into_array()),
                Some(decoded.index.try_into().unwrap_or(i64::MAX)),
                decoded.amount,
                "pulled",
            )
            .await?;
        return Ok(r);
    }

    // VaultRebalanced — Rebalanced(uint256 totalMoved).
    // No adapter address in this event; adapter and adapter_index are NULL.
    if topic0 == topics.vault_rebalanced {
        let decoded = IVaultEvents::Rebalanced::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("Rebalanced: {e}")))?;
        let r = db
            .insert_adapter_allocation(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                log.address.into_array(),
                None,
                None,
                decoded.totalMoved,
                "rebalanced",
            )
            .await?;
        return Ok(r);
    }

    // ExitFeeCharged — ExitFeeCharged(address indexed owner, address indexed receiver,
    //                                 uint256 grossAssets, uint256 fee, uint256 netAssets).
    // One event drives two tables:
    //   * vault_fee_events — fee collection history for the vault detail API
    //     (issue #675 AC-2).
    //   * account_history_events — a 'fee_charged' row attributed to the owner
    //     so the per-account history feed surfaces the fee (issue #654, §5.4).
    if topic0 == topics.vault_exit_fee_charged {
        let decoded = IVaultEvents::ExitFeeCharged::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("ExitFeeCharged: {e}")))?;
        let fee_rows = db
            .insert_vault_fee_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                log.address.into_array(),
                decoded.owner.into_array(),
                decoded.receiver.into_array(),
                decoded.grossAssets,
                decoded.fee,
                decoded.netAssets,
            )
            .await?;
        let history_rows = db
            .insert_history_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.owner.into_array(),
                "fee_charged",
                Some(log.address.into_array()),
                None,
                Some(decoded.fee),
            )
            .await?;
        return Ok(fee_rows + history_rows);
    }

    // Paused / Unpaused — only drive state snapshots; no dedicated table row.
    if topic0 == topics.paused || topic0 == topics.unpaused {
        return Ok(0);
    }

    // VaultRegistered — upsert a row into `vaults`.
    // New signature: (address indexed vault, string name, address indexed asset).
    // Fields riskLabel/depositCap/registeredAt removed from contract; use
    // empty-string/zero/block_number as DB defaults.
    if topic0 == topics.vault_registered {
        let decoded = IVaultRegistryEvents::VaultRegistered::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("VaultRegistered: {e}")))?;
        db.upsert_contract(cfg.chain_id, decoded.vault.into_array(), "vault", None)
            .await?;
        let r = db
            .upsert_vault(
                cfg.chain_id,
                decoded.vault.into_array(),
                &decoded.name,
                risk_label_from_vault_name(&decoded.name),
                U256::ZERO,              // depositCap removed
                0i16,                    // VaultStatus::Active at registration
                log.block_number as i64, // registeredAt removed; use block_number
                log.block_number as i64,
                log.tx_hash.0,
            )
            .await?;
        return Ok(r);
    }

    // VaultStatusChanged — update `status` and `status_changed_at`.
    // New signature: (address indexed vault, uint8 indexed newStatus, uint256 timestamp).
    if topic0 == topics.vault_status_changed {
        let decoded =
            IVaultRegistryEvents::VaultStatusChanged::decode_log(&into_alloy_log(log), true)
                .map_err(|e| IndexerError::Decode(format!("VaultStatusChanged: {e}")))?;
        let r = db
            .update_vault_status(
                cfg.chain_id,
                decoded.vault.into_array(),
                log.block_number as i64,
                log.log_index as i32,
                decoded.newStatus as i16,
                decoded.timestamp.try_into().unwrap_or(i64::MAX),
            )
            .await?;
        return Ok(r);
    }

    // ProposalCreated — insert a new governance proposal row.
    // New signature: (uint256 indexed proposalId, address indexed proposer,
    //                  address[] vaults, uint256[] bps, uint64 votingDeadline).
    // Fields description/createdAt/deadlineBlock removed; use empty-string/
    // block_number/votingDeadline-as-deadline as DB defaults.
    if topic0 == topics.proposal_created {
        let decoded =
            IRouterGovernanceEvents::ProposalCreated::decode_log(&into_alloy_log(log), true)
                .map_err(|e| IndexerError::Decode(format!("ProposalCreated: {e}")))?;
        let r = db
            .insert_proposal(
                cfg.chain_id,
                decoded.proposalId.try_into().unwrap_or(i64::MAX),
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.proposer.into_array(),
                "", // description removed from ProposalCreated (RouterGovernance.sol:106)
                log.block_number as i64, // createdAt removed; use block_number
                decoded.votingDeadline as i64,
            )
            .await?;
        return Ok(r);
    }

    // VoteCast — insert a per-voter vote row and update running tally.
    // New signature: (uint256 indexed proposalId, address indexed voter,
    //                  uint256 power, uint256 totalFor).
    // `support` bool removed (all votes are FOR); `weight` renamed to `power`.
    if topic0 == topics.vote_cast {
        let decoded = IRouterGovernanceEvents::VoteCast::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("VoteCast: {e}")))?;
        let mut r = db
            .insert_vote(
                cfg.chain_id,
                decoded.proposalId.try_into().unwrap_or(i64::MAX),
                decoded.voter.into_array(),
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                true, // support bool removed; governance only records FOR votes
                decoded.power,
            )
            .await?;
        // Store governance_vote in account history for the voter.
        r += db
            .insert_history_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                decoded.voter.into_array(),
                "governance_vote",
                None,
                None,
                None,
            )
            .await?;
        return Ok(r);
    }

    // ProposalExecuted — mark proposal status = 2 (executed).
    // New signature: (uint256 indexed proposalId, address indexed executor).
    if topic0 == topics.proposal_executed {
        let decoded =
            IRouterGovernanceEvents::ProposalExecuted::decode_log(&into_alloy_log(log), true)
                .map_err(|e| IndexerError::Decode(format!("ProposalExecuted: {e}")))?;
        let r = db
            .execute_proposal(
                cfg.chain_id,
                decoded.proposalId.try_into().unwrap_or(i64::MAX),
                log.block_number as i64,
            )
            .await?;
        return Ok(r);
    }

    // WeightsApplied — record a router weight snapshot.
    // New signature: (uint256 indexed proposalId, address[] vaults, uint256[] bps).
    if topic0 == topics.weights_applied {
        let decoded =
            IRouterGovernanceEvents::WeightsApplied::decode_log(&into_alloy_log(log), true)
                .map_err(|e| IndexerError::Decode(format!("WeightsApplied: {e}")))?;
        let vault_addresses: Vec<[u8; 20]> =
            decoded.vaults.iter().map(|a| a.into_array()).collect();
        let bps_values: Vec<i64> = decoded
            .bps
            .iter()
            .map(|b| b.try_into().unwrap_or(i64::MAX))
            .collect();
        let r = db
            .insert_router_weight_snapshot(
                cfg.chain_id,
                log.address.into_array(),
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                vault_addresses,
                bps_values,
            )
            .await?;
        return Ok(r);
    }

    // WeightsSet — direct admin call to PortfolioRouter.setWeights().
    // Signature: WeightsSet(address[] vaults, uint256[] bps).
    // This is the path taken by the demo seed script (not governance execution).
    if topic0 == topics.weights_set {
        let decoded = IPortfolioRouterEvents::WeightsSet::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("WeightsSet: {e}")))?;
        let vault_addresses: Vec<[u8; 20]> =
            decoded.vaults.iter().map(|a| a.into_array()).collect();
        let bps_values: Vec<i64> = decoded
            .bps
            .iter()
            .map(|b| b.try_into().unwrap_or(i64::MAX))
            .collect();
        let r = db
            .insert_router_weight_snapshot(
                cfg.chain_id,
                log.address.into_array(),
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                vault_addresses,
                bps_values,
            )
            .await?;
        return Ok(r);
    }

    // DefaultWeightsSet — direct admin call to PortfolioRouter.setDefaultWeights().
    // Signature: DefaultWeightsSet(address[] vaults, uint256[] bps).
    // Emitted by ADMIN_ROLE setting the fallback weight vector.
    if topic0 == topics.default_weights_set {
        let decoded =
            IPortfolioRouterEvents::DefaultWeightsSet::decode_log(&into_alloy_log(log), true)
                .map_err(|e| IndexerError::Decode(format!("DefaultWeightsSet: {e}")))?;
        let vault_addresses: Vec<[u8; 20]> =
            decoded.vaults.iter().map(|a| a.into_array()).collect();
        let bps_values: Vec<i64> = decoded
            .bps
            .iter()
            .map(|b| b.try_into().unwrap_or(i64::MAX))
            .collect();
        let r = db
            .insert_router_weight_snapshot(
                cfg.chain_id,
                log.address.into_array(),
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                vault_addresses,
                bps_values,
            )
            .await?;
        return Ok(r);
    }

    // ERC-4626 Deposit — Deposit(address indexed caller, address indexed owner,
    //                            uint256 assets, uint256 shares).
    // Persists a vault_transfer_events row for the deposit/withdrawal log
    // (issue #675 AC-2). The event-driven snapshot is still handled by the
    // caller via `event_blocks_per_contract`.
    if topic0 == topics.erc4626_deposit {
        let decoded = IVaultEvents::Deposit::decode_log(&into_alloy_log(log), true)
            .map_err(|e| IndexerError::Decode(format!("ERC4626 Deposit: {e}")))?;
        let mut r = db
            .insert_vault_transfer_event(
                cfg.chain_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                log.address.into_array(),
                "deposit",
                decoded.caller.into_array(),
                decoded.owner.into_array(),
                decoded.assets,
                decoded.shares,
            )
            .await?;
        // IDX-3: roll the owner's wallet_positions balance forward by the
        // minted shares so the account-positions view is populated.  A deposit
        // mints `shares` to `owner`; the new balance is prior + shares.
        r += persist_wallet_position(
            db,
            cfg,
            log,
            decoded.owner.into_array(),
            decoded.shares,
            true,
        )
        .await?;
        return Ok(r);
    }

    // ─── InvestmentCommitteePolicy event handlers ─────────────────────────────
    // Canonical: docs/architecture.md §5.4 — issue #1053.
    //
    // Three events: AgentRegistered, AgentRevoked (IC-specific), VoteSubmitted.
    // Note: IC AgentRevoked has signature `AgentRevoked(address)` — one indexed
    // field — which is distinct from IGateway AgentRevoked `AgentRevoked(address,address)`
    // (two indexed fields).  They have different topic-0 hashes and are dispatched
    // independently.

    if topic0 == topics.ic_agent_registered {
        let decoded = IInvestmentCommitteePolicyEvents::AgentRegistered::decode_log(
            &into_alloy_log(log),
            true,
        )
        .map_err(|e| IndexerError::Decode(format!("IC AgentRegistered: {e}")))?;
        let r = db
            .upsert_committee_agent(
                cfg.chain_id,
                decoded.agent.into_array(),
                &decoded.agentId,
                log.block_number as i64,
            )
            .await?;
        return Ok(r);
    }

    if topic0 == topics.ic_agent_revoked {
        let decoded =
            IInvestmentCommitteePolicyEvents::AgentRevoked::decode_log(&into_alloy_log(log), true)
                .map_err(|e| IndexerError::Decode(format!("IC AgentRevoked: {e}")))?;
        let r = db
            .revoke_committee_agent(
                cfg.chain_id,
                decoded.agent.into_array(),
                log.block_number as i64,
            )
            .await?;
        return Ok(r);
    }

    if topic0 == topics.ic_vote_submitted {
        let decoded =
            IInvestmentCommitteePolicyEvents::VoteSubmitted::decode_log(&into_alloy_log(log), true)
                .map_err(|e| IndexerError::Decode(format!("IC VoteSubmitted: {e}")))?;
        let vote_id: i64 = decoded.voteId.try_into().unwrap_or(i64::MAX);
        let agent = decoded.agent.into_array();
        let vault = decoded.vault.into_array();
        let vote_json_hash: [u8; 32] = decoded.voteJsonHash.0;

        // Memo fetch + keccak256 verification.
        // If rationaleUri is empty or the fetch fails, store with verified=false.
        // If the memo's keccak256 matches voteJsonHash, store with verified=true.
        let verified = if decoded.rationaleUri.is_empty() {
            false
        } else {
            match fetch_and_verify_memo(&decoded.rationaleUri, vote_json_hash).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        vote_id,
                        rationale_uri = %decoded.rationaleUri,
                        error = %e,
                        "memo fetch/verify failed; storing vote with verified=false"
                    );
                    false
                }
            }
        };

        let mut r = db
            .insert_committee_vote(
                cfg.chain_id,
                vote_id,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                agent,
                vault,
                decoded.stance as i16,
                decoded.targetWeightBps as i16,
                decoded.confidence as i16,
                &decoded.rationaleUri,
                vote_json_hash,
                decoded.timestamp as i64,
                verified,
            )
            .await?;

        // Refresh the regime snapshot for this vault (verified votes only).
        if verified {
            r += db
                .refresh_regime_snapshot(cfg.chain_id, vault, log.block_number as i64)
                .await?;
        }

        return Ok(r);
    }

    // ─── ConsensusRecommendationReceipt event handlers ─────────────────────────────
    // Canonical: docs/architecture.md §4.9 — issue #1247.
    //
    // Two events: ReceiptRecorded (append) and ReceiptReleased (in-place flip).
    // Neither carries a signature parameter — the analysts' ed25519 signatures
    // are payload data, never event data.

    if topic0 == topics.consensus_receipt_recorded {
        let decoded = IConsensusRecommendationReceiptEvents::ReceiptRecorded::decode_log(
            &into_alloy_log(log),
            true,
        )
        .map_err(|e| IndexerError::Decode(format!("ReceiptRecorded: {e}")))?;
        let receipt_id: [u8; 32] = decoded.receiptId.0;
        let payload_digest: [u8; 32] = decoded.payloadDigest.0;
        let receipt_index: i64 = decoded.index.try_into().unwrap_or(i64::MAX);

        // Independent digest verification: fetch the bytes the on-chain commitment
        // points at and recompute keccak256 over them.  A fetch failure is
        // NON-FATAL — the commitment row is always stored, with verified = false,
        // so a suppressed or unreachable payload is visible as such rather than
        // silently absent (architecture §4.9: the record's whole point is that it
        // cannot be quietly withheld).
        //
        // SCOPE: this verifies the payload DIGEST only.  Per-analyst ed25519
        // verification of the signatures embedded in the payload is `rmpc`'s job
        // at submit time (architecture §4.9.1 answer 1: rmpc refuses to submit a
        // receipt whose digest or embedded signatures do not verify) and a future
        // indexer pass.  The EVM has no ed25519 precompile and ADR-0012 §5 closes
        // that seam, so nothing on chain asserts it either.
        let (verified, payload_bytes) = if decoded.payloadUri.is_empty() {
            (false, None)
        } else {
            match fetch_and_verify_payload(&decoded.payloadUri, payload_digest).await {
                Ok((v, len)) => (v, Some(len)),
                Err(e) => {
                    tracing::warn!(
                        receipt_id = %alloy_primitives::hex::encode(receipt_id),
                        payload_uri = %decoded.payloadUri,
                        error = %e,
                        "consensus receipt payload fetch/verify failed; \
                         storing receipt with verified=false"
                    );
                    (false, None)
                }
            }
        };

        let r = db
            .insert_consensus_receipt(
                cfg.chain_id,
                receipt_id,
                receipt_index,
                decoded.submitter.into_array(),
                payload_digest,
                &decoded.payloadUri,
                decoded.recordedAt as i64,
                log.block_number as i64,
                log.log_index as i32,
                log.tx_hash.0,
                verified,
                payload_bytes,
            )
            .await?;
        return Ok(r);
    }

    if topic0 == topics.consensus_receipt_released {
        let decoded = IConsensusRecommendationReceiptEvents::ReceiptReleased::decode_log(
            &into_alloy_log(log),
            true,
        )
        .map_err(|e| IndexerError::Decode(format!("ReceiptReleased: {e}")))?;
        let r = db
            .mark_consensus_receipt_released(
                cfg.chain_id,
                decoded.receiptId.0,
                decoded.releasedBy.into_array(),
                decoded.releasedAt as i64,
                log.block_number as i64,
            )
            .await?;
        return Ok(r);
    }

    Ok(0)
}

/// Fetch the canonical receipt bytes from `payload_uri` and verify their
/// keccak256 against the on-chain `expected_digest`.  Returns
/// `(digest_matches, body_len)`.  Errors on network failure, non-200 HTTP, or
/// a response body over the 1 MiB cap — every error is non-fatal at the call
/// site, which stores the receipt with `verified = false`.
async fn fetch_and_verify_payload(
    payload_uri: &str,
    expected_digest: [u8; 32],
) -> Result<(bool, i64), String> {
    // Mirrors `fetch_and_verify_memo`: 10s timeout, 1 MiB body cap.
    const MAX_BODY: usize = 1024 * 1024;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build client: {e}"))?;

    let resp = client
        .get(payload_uri)
        .send()
        .await
        .map_err(|e| format!("GET {payload_uri}: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("GET {payload_uri} returned {}", resp.status()));
    }

    let body = resp.bytes().await.map_err(|e| format!("read body: {e}"))?;

    if body.len() > MAX_BODY {
        return Err(format!(
            "receipt payload too large: {} bytes (max {MAX_BODY})",
            body.len()
        ));
    }

    let actual = alloy_primitives::keccak256(&body);
    Ok((actual.0 == expected_digest, body.len() as i64))
}

/// Fetch the JSON memo from `rationale_uri` and verify its keccak256 against
/// `expected_hash`.  Returns `true` if the hash matches, `false` otherwise.
/// Errors on network failure, non-200 HTTP, or response body too large.
async fn fetch_and_verify_memo(
    rationale_uri: &str,
    expected_hash: [u8; 32],
) -> Result<bool, String> {
    // Use a one-off reqwest client — the indexer is single-tick so no pool needed.
    // Limit body to 1 MiB to prevent runaway allocations on malicious URIs.
    const MAX_BODY: usize = 1024 * 1024;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build client: {e}"))?;

    let resp = client
        .get(rationale_uri)
        .send()
        .await
        .map_err(|e| format!("GET {rationale_uri}: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("GET {rationale_uri} returned {}", resp.status()));
    }

    let body = resp.bytes().await.map_err(|e| format!("read body: {e}"))?;

    if body.len() > MAX_BODY {
        return Err(format!(
            "memo body too large: {} bytes (max {MAX_BODY})",
            body.len()
        ));
    }

    let actual = alloy_primitives::keccak256(&body);
    Ok(actual.0 == expected_hash)
}

/// IDX-3: persist the owner's resulting `wallet_positions` share balance after
/// an ERC-4626 mint/burn event.  Reads the owner's prior balance for this
/// vault (`contract = log.address`), applies the `shares` delta (`add = true`
/// for a deposit/mint, `false` for a withdraw/burn, saturating at zero), and
/// writes the new block-numbered balance snapshot.  Returns the number of rows
/// inserted (0 when the row already exists, via the writer's `ON CONFLICT`).
async fn persist_wallet_position(
    db: &Db,
    cfg: &IndexerConfig,
    log: &LogEntry,
    owner: [u8; 20],
    shares: U256,
    add: bool,
) -> Result<u64, IndexerError> {
    let contract = log.address.into_array();
    let block = log.block_number as i64;
    let prior = db
        .latest_wallet_position(cfg.chain_id, contract, owner, block)
        .await?
        .unwrap_or(U256::ZERO);
    let new_balance = if add {
        prior.saturating_add(shares)
    } else {
        prior.saturating_sub(shares)
    };
    let r = db
        .insert_wallet_position(cfg.chain_id, contract, owner, block, new_balance)
        .await?;
    Ok(r)
}

/// Convert our local `LogEntry` to the `alloy_primitives::Log` shape
/// `SolEvent::decode_log` expects.
fn into_alloy_log(log: &LogEntry) -> alloy_primitives::Log {
    alloy_primitives::Log {
        address: log.address,
        data: alloy_primitives::LogData::new_unchecked(log.topics.clone(), log.data.clone()),
    }
}

/// Read totalAssets / totalSupply / exitFeeBps / tvlCap / paused from a
/// vault at `block` and write a `vault_snapshots` row.
/// Snapshot one vault, logging and skipping on failure instead of propagating.
///
/// A single vault's `totalAssets()`/`totalSupply()` read can fail for reasons
/// that must not wedge the whole indexer — e.g. the call lands on a block where
/// that vault had no code yet (router-event-block snapshots of a vault registered
/// later return an empty `eth_call` -> "u256 read: short response (0 bytes)"), or
/// a transient revert. Previously the `?` in `snapshot_vault_address` aborted the
/// entire tick, so one un-snapshottable vault left every other registered vault's
/// TVL null forever and stalled the indexer (issue #878). Returning the rows
/// inserted (0 on failure) keeps the tick advancing; the next heartbeat re-snaps
/// the skipped vault at `target`, where it does have code.
async fn snapshot_vault_or_skip(
    db: &Db,
    rpc: &JsonRpc,
    chain_id: i64,
    vault: Address,
    block: u64,
) -> i64 {
    match snapshot_vault_address(db, rpc, chain_id, vault, block).await {
        Ok(n) => n as i64,
        Err(e) => {
            tracing::warn!(vault = %vault, block, error = %e, "skipping vault snapshot");
            0
        }
    }
}

async fn snapshot_vault_address(
    db: &Db,
    rpc: &JsonRpc,
    chain_id: i64,
    vault: Address,
    block: u64,
) -> Result<u64, IndexerError> {
    let total_assets = call_u256(
        rpc,
        vault,
        IVaultReads::totalAssetsCall {}.abi_encode(),
        block,
    )
    .await?;
    let total_supply = call_u256(
        rpc,
        vault,
        IVaultReads::totalSupplyCall {}.abi_encode(),
        block,
    )
    .await?;
    let exit_fee_bps = call_u256(
        rpc,
        vault,
        IVaultReads::exitFeeBpsCall {}.abi_encode(),
        block,
    )
    .await
    .unwrap_or(U256::ZERO);
    let tvl_cap = call_u256(rpc, vault, IVaultReads::tvlCapCall {}.abi_encode(), block)
        .await
        .unwrap_or(U256::ZERO);
    let paused = call_bool(rpc, vault, IVaultReads::pausedCall {}.abi_encode(), block)
        .await
        .unwrap_or(false);

    db.insert_vault_snapshot(
        chain_id,
        vault.into_array(),
        block as i64,
        total_assets,
        total_supply,
        exit_fee_bps.try_into().unwrap_or(0i64),
        tvl_cap,
        paused,
    )
    .await
    .map_err(IndexerError::Db)
}

async fn call_u256(
    rpc: &JsonRpc,
    to: Address,
    data: Vec<u8>,
    block: u64,
) -> Result<U256, IndexerError> {
    let bytes = rpc.eth_call_at(to, Bytes::from(data), block).await?;
    if bytes.len() < 32 {
        return Err(IndexerError::Decode(format!(
            "u256 read: short response ({} bytes)",
            bytes.len()
        )));
    }
    Ok(U256::from_be_slice(&bytes[..32]))
}

async fn call_bool(
    rpc: &JsonRpc,
    to: Address,
    data: Vec<u8>,
    block: u64,
) -> Result<bool, IndexerError> {
    let v = call_u256(rpc, to, data, block).await?;
    Ok(v != U256::ZERO)
}

/// The label used when a registration name carries no recognised
/// discriminator. It is a real classification, not a sentinel, which is why
/// [`risk_label_from_vault_name`] logs when it falls back to it — issue #1434
/// was invisible for exactly as long as the fallback was silent.
const DEFAULT_RISK_LABEL: &str = "STABLE_YIELD";

/// Classify a vault registration name, or `None` if no discriminator matched.
///
/// Matching is on a normalised **substring**, not the whole name, because the
/// name is not a stable key:
///
/// - two of the five registration sites take the name from an environment
///   variable (`VAULT_NAME` in `DeployVaultRegistry.s.sol`, `RWA_VAULT_NAME` in
///   `DeployDemoExtraVaults.s.sol`), so the exact string is a deploy-time
///   input, not a constant;
/// - the shipped names already disagree on the RWA vault's suffix —
///   `DeployVaultThemes.s.sol` registers `"Robot Money RWA"` while
///   `DeployDemoExtraVaults.s.sol` defaults to `"Robot Money RWA / Thematic"`.
///
/// An exact-match table over such a key is what produced #1434: it was keyed on
/// an `"RM …"` spelling that no deploy script has ever registered, so all four
/// arms were dead and every vault took the default.
///
/// Order is significant. `protocol` is tested after the basket discriminators
/// and before `usdc` so that `ProtocolAssetVault` — whose asset *is* USDC —
/// classifies on what the vault holds rather than on what it is denominated in.
fn classify_risk_label(name: &str) -> Option<&'static str> {
    // Lower-case and collapse runs of whitespace, so a double space or a
    // trailing newline in an env-supplied name cannot defeat the match.
    let normalised = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();

    if normalised.contains("agent token")
        || normalised.contains("rwa")
        || normalised.contains("thematic")
    {
        Some("SPECULATIVE")
    } else if normalised.contains("protocol") {
        Some("VOLATILE")
    } else if normalised.contains("usdc") {
        Some("STABLE_YIELD")
    } else {
        None
    }
}

/// Map vault name to risk label per PRD §11.
/// The VaultRegistered event carries only name and asset; risk_label was
/// removed from VaultMetadata to avoid contract changes. The indexer derives
/// it from the registration name as a stopgap — a contract-level risk_label
/// field is a future improvement.
///
/// See [`classify_risk_label`] for why the derivation is a normalised substring
/// match rather than an exact-name table (issue #1434). An unrecognised name is
/// still classified [`DEFAULT_RISK_LABEL`], but loudly: the dapp's
/// `CompositionSection` picks its rendering off this column, so a
/// misclassification shows up as a basket vault rendering the static
/// `USDC -> rmUSDC` label, with nothing in the logs to point at the cause.
fn risk_label_from_vault_name(name: &str) -> &'static str {
    classify_risk_label(name).unwrap_or_else(|| {
        tracing::warn!(
            vault_name = %name,
            risk_label = DEFAULT_RISK_LABEL,
            "vault registration name matches no risk-label discriminator; \
             defaulting. The dapp renders composition off risk_label, so a \
             basket vault landing here will render as a plain USDC vault."
        );
        DEFAULT_RISK_LABEL
    })
}

#[cfg(test)]
mod risk_label_tests {
    use super::{classify_risk_label, risk_label_from_vault_name, DEFAULT_RISK_LABEL};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    /// Repository root, derived from this crate's manifest directory
    /// (`services/explorer-indexer`).
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
    }

    /// Every double-quoted literal on one line of Solidity.
    ///
    /// Deliberately naive — the lines this is applied to are filtered to the
    /// three registration shapes below, none of which contain an escaped quote.
    fn quoted_literals(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = line;
        while let Some(open) = rest.find('"') {
            rest = &rest[open + 1..];
            match rest.find('"') {
                Some(close) => {
                    out.push(rest[..close].to_string());
                    rest = &rest[close + 1..];
                }
                None => break,
            }
        }
        out
    }

    /// Scrape the vault names the deploy scripts actually register.
    ///
    /// Three shapes carry a name literal today:
    ///
    /// - `VaultMetadata({name: "…", …})`
    /// - `string public constant …_NAME = "…"` (the env-override defaults)
    /// - `_registerIfAbsent(registry, vault, asset, "…")`
    ///
    /// Comment lines are skipped so the doc comments that quote these names for
    /// documentation do not count as registration sites.
    ///
    /// Returns `(names, files_contributing)`.
    fn registered_vault_names() -> (BTreeSet<String>, BTreeSet<String>) {
        let script_dir = repo_root().join("contracts").join("script");
        let mut names = BTreeSet::new();
        let mut files = BTreeSet::new();

        let entries = std::fs::read_dir(&script_dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", script_dir.display()));

        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("sol") {
                continue;
            }
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("script file name")
                .to_string();

            for line in body.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                let is_registration = trimmed.contains("name: \"")
                    || trimmed.contains("_NAME = \"")
                    || trimmed.contains("_registerIfAbsent(");
                if !is_registration {
                    continue;
                }
                for literal in quoted_literals(trimmed) {
                    if literal.trim().is_empty() {
                        continue;
                    }
                    names.insert(literal);
                    files.insert(file_name.clone());
                }
            }
        }

        (names, files)
    }

    /// Every name a deploy script registers must be recognised by the mapping.
    ///
    /// This is the regression pin for issue #1434: the previous table was keyed
    /// on an `"RM …"` spelling that no script has ever registered, so all four
    /// arms were dead and every vault fell through to the default. Renaming a
    /// vault at its registration site now fails this test instead of silently
    /// re-defaulting the whole registry to `STABLE_YIELD`.
    #[test]
    fn every_registered_vault_name_is_classified() {
        let (names, files) = registered_vault_names();

        // Guard the scrape itself. If registration moves to a shape this does
        // not recognise, the loop below would pass vacuously; five names across
        // four scripts is the state at the time of writing.
        assert!(
            names.len() >= 5,
            "scraped only {} vault name(s) from contracts/script — the scrape has \
             gone blind, or registration moved to a new shape. Found: {names:?}",
            names.len()
        );
        assert!(
            files.len() >= 4,
            "scraped registration names from only {} script file(s): {files:?}",
            files.len()
        );

        let unclassified: Vec<&String> = names
            .iter()
            .filter(|n| classify_risk_label(n).is_none())
            .collect();
        assert!(
            unclassified.is_empty(),
            "these names are registered by a deploy script but match no \
             risk-label discriminator, so every vault carrying one silently \
             becomes {DEFAULT_RISK_LABEL}: {unclassified:?}"
        );
    }

    /// Each shipped name maps to the label the PRD intends for that vault.
    #[test]
    fn risk_label_mapping_matches_shipped_vault_names() {
        // The names registered on-chain today, per contracts/script.
        let cases = [
            ("Robot Money USDC", "STABLE_YIELD"),
            ("Robot Money Protocol", "VOLATILE"),
            ("Robot Money Agent Tokens", "SPECULATIVE"),
            ("Robot Money RWA / Thematic", "SPECULATIVE"),
            // DeployVaultThemes.s.sol registers the RWA vault without the
            // "/ Thematic" suffix; both spellings must land on one label.
            ("Robot Money RWA", "SPECULATIVE"),
        ];

        for (name, expected) in cases {
            assert_eq!(
                risk_label_from_vault_name(name),
                expected,
                "{name} must classify {expected}"
            );
            assert_eq!(
                classify_risk_label(name),
                Some(expected),
                "{name} must be recognised explicitly, not reached via the default"
            );
        }
    }

    /// The pre-#1434 `"RM …"` spellings keep working: a database registered by
    /// an older deploy script must not be reclassified by this change.
    #[test]
    fn legacy_rm_prefixed_names_still_classify() {
        for (name, expected) in [
            ("RM USDC", "STABLE_YIELD"),
            ("RM Protocol", "VOLATILE"),
            ("RM Agent Tokens", "SPECULATIVE"),
            ("RM RWA / Thematic", "SPECULATIVE"),
        ] {
            assert_eq!(classify_risk_label(name), Some(expected), "legacy {name}");
        }
    }

    /// A basket discriminator wins over the `usdc` denomination.
    ///
    /// `ProtocolAssetVault`'s asset *is* USDC, so a name mentioning both must
    /// classify on what the vault holds, not on what it is denominated in.
    #[test]
    fn basket_discriminator_outranks_usdc_denomination() {
        assert_eq!(
            classify_risk_label("Robot Money Protocol (USDC)"),
            Some("VOLATILE")
        );
        assert_eq!(
            classify_risk_label("Robot Money Agent Tokens USDC"),
            Some("SPECULATIVE")
        );
    }

    /// Normalisation covers the deploy-time inputs: `VAULT_NAME` and
    /// `RWA_VAULT_NAME` are environment variables, so casing and stray
    /// whitespace are operator-supplied and must not defeat the match.
    #[test]
    fn classification_survives_casing_and_whitespace() {
        for name in [
            "robot money agent tokens",
            "ROBOT MONEY AGENT TOKENS",
            "Robot  Money   Agent  Tokens",
            "  Robot Money Agent Tokens\n",
        ] {
            assert_eq!(
                classify_risk_label(name),
                Some("SPECULATIVE"),
                "{name:?} must normalise to the Agent Tokens classification"
            );
        }
    }

    /// An unrecognised name is still classified, but reports that it was not
    /// recognised — the distinction #1434 needed and did not have.
    #[test]
    fn unrecognised_name_falls_back_visibly() {
        assert_eq!(classify_risk_label("Some Unrelated Vault"), None);
        assert_eq!(
            risk_label_from_vault_name("Some Unrelated Vault"),
            DEFAULT_RISK_LABEL
        );
    }
}
