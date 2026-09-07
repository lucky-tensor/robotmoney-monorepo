//! Core watchdog loop: poll indexed volume data, compare against thresholds, and
//! trigger the configured action on breach.
//!
//! Canonical: docs/technical/security-model.md §9
//!
//! # Cycle
//!
//! 1. Determine the latest indexed block from `indexer_runs` and load the
//!    watchdog's last-processed-block cursor.
//! 2. For **every** block in `(cursor, latest]` (not just the latest — scan
//!    finding WD-6), query per-block mint/burn volume and the rolling per-hour
//!    volume anchored to that block's on-chain timestamp (WD-2).
//! 3. Compare each metric against the global thresholds from [`Config`].
//! 4. On breach: dispatch the alert first, then the pause under an SLA-bounded
//!    timeout, so a hung pause RPC never starves the alert path (WD-1).
//! 5. Advance the cursor past each successfully evaluated block.
//! 6. Sleep for `poll_interval_secs` and repeat.
//!
//! The watchdog does NOT exit on a breach — it continues polling to catch
//! subsequent breaches and to log that the gateway remains paused.

use std::time::Duration;

use reqwest::Client;
use sqlx::postgres::PgPool;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::{
    alert::{dispatch_alert, BreachEvent, ThresholdKind},
    config::Config,
    pause::{trigger_pause, PauseParams, PauserSigningKey},
    volume::{
        burn_volume_per_block, burn_volume_per_block_for_vault, burn_volume_per_hour,
        burn_volume_per_hour_for_vault, mint_volume_per_block, mint_volume_per_block_for_vault,
        mint_volume_per_hour, mint_volume_per_hour_for_vault,
    },
    WatchdogError,
};

/// Result of a single watchdog poll cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleResult {
    /// No threshold was breached; normal operation.
    Ok,
    /// One or more thresholds were breached; actions were dispatched.
    Breached(Vec<ThresholdKind>),
    /// The indexer has not yet written any block data; poll again later.
    NoData,
}

/// Rolling per-hour window width, in seconds.
const HOUR_WINDOW_SECS: i64 = 3600;

/// Run a single watchdog cycle against the given `block_number`.
///
/// Exposed as a free function so integration tests can drive individual cycles
/// deterministically without a real timer loop. Most callers should prefer
/// [`run_cycles_since_cursor`], which evaluates every block since the cursor and
/// persists progress; this single-block entry point is retained for tests and is
/// also used internally per block by the cursor loop.
///
/// `pauser` carries the signing state derived once at startup by
/// [`Config::take_pauser_signing_key`]; pass `None` when no pauser key is
/// configured (alert-only deployments). The cycle never reads raw key material
/// from `config`.
pub async fn run_cycle(
    pool: &PgPool,
    config: &Config,
    client: &Client,
    chain_id: i64,
    block_number: i64,
    pauser: Option<&PauserSigningKey>,
) -> Result<CycleResult, WatchdogError> {
    // Anchor the rolling per-hour window to indexed *chain* time, not wall-clock
    // (scan finding WD-2). Using the evaluated block's on-chain timestamp keeps
    // the window consistent whether the watchdog is at tip or catching up, and
    // makes the breach decision deterministic for a given indexed state. Fall
    // back to the block's persisted timestamp; if the header row is somehow
    // missing the per-hour window is empty rather than wall-clock-skewed.
    let now_unix = match block_timestamp(pool, chain_id, block_number).await? {
        Some(ts) => ts,
        None => match latest_block_timestamp(pool, chain_id).await? {
            Some(ts) => {
                warn!(
                    chain_id,
                    block_number,
                    "no persisted header for evaluated block; anchoring per-hour \
                     window to latest known chain time instead"
                );
                ts
            }
            None => {
                warn!(
                    chain_id,
                    block_number, "no persisted block headers; per-hour window empty this cycle"
                );
                0
            }
        },
    };

    // --- Compute volumes ---
    let mint_block = mint_volume_per_block(pool, chain_id, block_number).await?;
    let mint_hour = mint_volume_per_hour(pool, chain_id, now_unix, HOUR_WINDOW_SECS).await?;
    let burn_block = burn_volume_per_block(pool, chain_id, block_number).await?;
    let burn_hour = burn_volume_per_hour(pool, chain_id, now_unix, HOUR_WINDOW_SECS).await?;

    // --- Compare against thresholds ---
    let mut breaches: Vec<BreachEvent> = Vec::new();

    let pb_mint_limit = config.global_per_block_mint_limit();
    if mint_block > pb_mint_limit {
        warn!(
            chain_id,
            block_number,
            volume = mint_block,
            threshold = pb_mint_limit,
            "per-block mint threshold breached"
        );
        breaches.push(BreachEvent {
            kind: ThresholdKind::PerBlockMint,
            threshold_usdc: pb_mint_limit,
            volume_usdc: mint_block,
            chain_id,
            block_number,
            vault: None,
        });
    }

    let ph_mint_limit = config.global_per_hour_mint_limit();
    if mint_hour > ph_mint_limit {
        warn!(
            chain_id,
            block_number,
            volume = mint_hour,
            threshold = ph_mint_limit,
            "per-hour mint threshold breached"
        );
        breaches.push(BreachEvent {
            kind: ThresholdKind::PerHourMint,
            threshold_usdc: ph_mint_limit,
            volume_usdc: mint_hour,
            chain_id,
            block_number,
            vault: None,
        });
    }

    let pb_burn_limit = config.global_per_block_burn_limit();
    if burn_block > pb_burn_limit {
        warn!(
            chain_id,
            block_number,
            volume = burn_block,
            threshold = pb_burn_limit,
            "per-block burn threshold breached"
        );
        breaches.push(BreachEvent {
            kind: ThresholdKind::PerBlockBurn,
            threshold_usdc: pb_burn_limit,
            volume_usdc: burn_block,
            chain_id,
            block_number,
            vault: None,
        });
    }

    let ph_burn_limit = config.global_per_hour_burn_limit();
    if burn_hour > ph_burn_limit {
        warn!(
            chain_id,
            block_number,
            volume = burn_hour,
            threshold = ph_burn_limit,
            "per-hour burn threshold breached"
        );
        breaches.push(BreachEvent {
            kind: ThresholdKind::PerHourBurn,
            threshold_usdc: ph_burn_limit,
            volume_usdc: burn_hour,
            chain_id,
            block_number,
            vault: None,
        });
    }

    // --- Per-vault threshold checks (scan finding WD-4) ---
    //
    // The global checks above measure the all-vault aggregate. A vault configured
    // with a *tighter* override (`[vault."<hex>"]` in config) must additionally be
    // measured against its own limit so a spike confined to that vault is caught
    // even when the global aggregate stays under the global cap. Volume is scoped
    // per vault (GROUP-BY-vault semantics) so one vault's spike never dilutes into
    // another's per-vault check.
    for vault_hex in config.vault.keys() {
        let Some(vault_bytes) = decode_vault_hex(vault_hex) else {
            warn!(
                vault = %vault_hex,
                "per-vault threshold key is not valid hex; skipping this vault"
            );
            continue;
        };

        evaluate_vault_breaches(
            pool,
            config,
            chain_id,
            block_number,
            now_unix,
            vault_hex,
            &vault_bytes,
            &mut breaches,
        )
        .await?;
    }

    if breaches.is_empty() {
        info!(
            chain_id,
            block_number,
            mint_block,
            mint_hour,
            burn_block,
            burn_hour,
            "watchdog cycle: all volumes within thresholds"
        );
        return Ok(CycleResult::Ok);
    }

    // --- Dispatch actions ---
    //
    // Alert FIRST, pause SECOND, and bound every action by the SLA budget (scan
    // finding WD-1). Previously the pause RPC was awaited before the alert with no
    // timeout, so a hung gateway RPC stalled the single-threaded cycle: the alert
    // never fired and the next poll never ran. Dispatching the on-call alert
    // before the pause guarantees a human is paged even if the automated pause is
    // wedged, and the per-action timeout caps how long any one RPC can block.
    let mode = config.action.mode;
    let sla = Duration::from_secs(config.sla.max_response_secs);

    if mode.includes_alert() {
        if let Some(webhook_url) = &config.action.webhook_url {
            for event in &breaches {
                match timeout(sla, dispatch_alert(client, webhook_url, event)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => error!("alert dispatch failed: {e}"),
                    Err(_) => error!(
                        sla_secs = config.sla.max_response_secs,
                        "alert dispatch exceeded SLA budget; aborted"
                    ),
                }
            }
        }
    }

    if mode.includes_pause() {
        let rpc_url = config
            .action
            .gateway_rpc_url
            .as_deref()
            .unwrap_or("")
            .to_owned();
        let gw_addr_str = config.action.gateway_address.as_deref().unwrap_or("");

        match pauser {
            Some(signer) => match parse_pause_params(
                rpc_url,
                gw_addr_str,
                signer,
                chain_id,
                config.action.pause_fee_bump_bps,
            ) {
                Ok(params) => match timeout(sla, trigger_pause(client, &params)).await {
                    Ok(Ok(tx)) => info!(tx_hash = %tx, "gateway.pause() submitted"),
                    Ok(Err(e)) => error!("gateway.pause() failed: {e}"),
                    Err(_) => error!(
                        sla_secs = config.sla.max_response_secs,
                        "gateway.pause() exceeded SLA budget; aborted (alert already dispatched)"
                    ),
                },
                Err(e) => error!("pause params invalid: {e}"),
            },
            // Startup validation rejects a pause mode with no key, so reaching
            // here means the caller did not thread the derived signer through.
            // Never silent: the pause simply did not happen.
            None => error!(
                "action.mode includes pause but no pauser signing key was derived \
                 at startup; gateway.pause() NOT attempted (alert already dispatched)"
            ),
        }
    }

    let kinds: Vec<ThresholdKind> = breaches.into_iter().map(|b| b.kind).collect();
    Ok(CycleResult::Breached(kinds))
}

/// Fetch the latest indexed block number for `chain_id` from `indexer_runs`.
///
/// Returns `None` if no successful run has been recorded yet.
pub async fn latest_indexed_block(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Option<i64>, WatchdogError> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        "SELECT MAX(last_indexed_block) FROM indexer_runs \
         WHERE chain_id = $1 AND error IS NULL",
    )
    .bind(chain_id)
    .fetch_optional(pool)
    .await
    .map_err(WatchdogError::Db)?;

    Ok(row.and_then(|(v,)| v))
}

/// Look up the on-chain timestamp (Unix seconds) of a single block.
///
/// Returns `None` if the indexer has not persisted a header row for that block.
pub async fn block_timestamp(
    pool: &PgPool,
    chain_id: i64,
    block_number: i64,
) -> Result<Option<i64>, WatchdogError> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT timestamp FROM blocks WHERE chain_id = $1 AND block_number = $2")
            .bind(chain_id)
            .bind(block_number)
            .fetch_optional(pool)
            .await
            .map_err(WatchdogError::Db)?;
    Ok(row.map(|(t,)| t))
}

/// Look up the on-chain timestamp of the highest persisted block for `chain_id`.
async fn latest_block_timestamp(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Option<i64>, WatchdogError> {
    let row: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT MAX(timestamp) FROM blocks WHERE chain_id = $1")
            .bind(chain_id)
            .fetch_optional(pool)
            .await
            .map_err(WatchdogError::Db)?;
    Ok(row.and_then(|(v,)| v))
}

/// Load the watchdog's last-processed-block cursor for `chain_id`.
///
/// Returns `None` if the watchdog has never recorded progress for this chain.
pub async fn load_cursor(pool: &PgPool, chain_id: i64) -> Result<Option<i64>, WatchdogError> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT last_processed_block FROM watchdog_cursor WHERE chain_id = $1")
            .bind(chain_id)
            .fetch_optional(pool)
            .await
            .map_err(WatchdogError::Db)?;
    Ok(row.map(|(b,)| b))
}

/// Persist the watchdog's last-processed-block cursor for `chain_id`.
///
/// Idempotent upsert keyed on `chain_id`; safe to call after every evaluated
/// block so a crash mid-range re-processes the remaining blocks rather than
/// skipping them.
pub async fn store_cursor(
    pool: &PgPool,
    chain_id: i64,
    last_processed_block: i64,
) -> Result<(), WatchdogError> {
    sqlx::query(
        "INSERT INTO watchdog_cursor (chain_id, last_processed_block, updated_at) \
         VALUES ($1, $2, now()) \
         ON CONFLICT (chain_id) DO UPDATE \
           SET last_processed_block = EXCLUDED.last_processed_block, updated_at = now()",
    )
    .bind(chain_id)
    .bind(last_processed_block)
    .execute(pool)
    .await
    .map_err(WatchdogError::Db)?;
    Ok(())
}

/// Evaluate **every** block newly indexed since the watchdog's cursor, advancing
/// the cursor past each block it successfully evaluates (scan finding WD-6).
///
/// On the first run for a chain (no cursor row) the cursor is seeded at
/// `latest_indexed_block - 1`, so the latest block is evaluated exactly once and
/// the watchdog does not replay the entire indexed history.
///
/// Returns the aggregate result: `NoData` if there is nothing new to evaluate,
/// `Breached` with the union of all kinds breached across the range, otherwise
/// `Ok`. The cursor advances block-by-block, so a mid-range error leaves the
/// remaining blocks to be retried on the next cycle rather than skipped.
pub async fn run_cycles_since_cursor(
    pool: &PgPool,
    config: &Config,
    client: &Client,
    chain_id: i64,
    latest_indexed_block: i64,
    pauser: Option<&PauserSigningKey>,
) -> Result<CycleResult, WatchdogError> {
    let cursor = load_cursor(pool, chain_id).await?;
    // First run: start one below the latest indexed block so we evaluate exactly
    // that block, not the whole history.
    let from_block = cursor.map(|c| c + 1).unwrap_or(latest_indexed_block);

    if from_block > latest_indexed_block {
        // Nothing new since last cycle.
        return Ok(CycleResult::NoData);
    }

    let mut all_kinds: Vec<ThresholdKind> = Vec::new();
    for block_number in from_block..=latest_indexed_block {
        match run_cycle(pool, config, client, chain_id, block_number, pauser).await {
            Ok(CycleResult::Breached(kinds)) => {
                for k in kinds {
                    if !all_kinds.contains(&k) {
                        all_kinds.push(k);
                    }
                }
            }
            Ok(CycleResult::Ok) | Ok(CycleResult::NoData) => {}
            Err(e) => {
                // Do NOT advance the cursor past a block we failed to evaluate;
                // return so the next cycle retries from here.
                error!(chain_id, block_number, "cycle error mid-range: {e}");
                return Err(e);
            }
        }
        // Advance the cursor only after a successful evaluation of this block.
        store_cursor(pool, chain_id, block_number).await?;
    }

    if all_kinds.is_empty() {
        Ok(CycleResult::Ok)
    } else {
        Ok(CycleResult::Breached(all_kinds))
    }
}

/// Decode a per-vault config key (lowercase hex address, no `0x` prefix) to raw
/// bytes for binding against the BYTEA `vault` column.
///
/// Tolerates an optional `0x` prefix so a mis-keyed config entry still resolves.
/// Returns `None` if the key is not valid hex.
fn decode_vault_hex(vault_hex: &str) -> Option<Vec<u8>> {
    let trimmed = vault_hex.trim_start_matches("0x");
    hex::decode(trimmed).ok()
}

/// Evaluate the four per-vault thresholds for a single configured vault and push
/// any breaches (with `vault` set) onto `breaches`.
///
/// Each metric is compared against the vault's *effective* limit from
/// [`Config::per_block_mint_limit`] and siblings, which fall back to the global
/// limit when the vault omits that override. A breach is recorded only when the
/// vault's own scoped volume exceeds its effective limit.
#[allow(clippy::too_many_arguments)]
async fn evaluate_vault_breaches(
    pool: &PgPool,
    config: &Config,
    chain_id: i64,
    block_number: i64,
    now_unix: i64,
    vault_hex: &str,
    vault_bytes: &[u8],
    breaches: &mut Vec<BreachEvent>,
) -> Result<(), WatchdogError> {
    let mint_block =
        mint_volume_per_block_for_vault(pool, chain_id, block_number, vault_bytes).await?;
    let mint_hour =
        mint_volume_per_hour_for_vault(pool, chain_id, now_unix, HOUR_WINDOW_SECS, vault_bytes)
            .await?;
    let burn_block =
        burn_volume_per_block_for_vault(pool, chain_id, block_number, vault_bytes).await?;
    let burn_hour =
        burn_volume_per_hour_for_vault(pool, chain_id, now_unix, HOUR_WINDOW_SECS, vault_bytes)
            .await?;

    let checks = [
        (
            ThresholdKind::PerBlockMint,
            mint_block,
            config.per_block_mint_limit(vault_hex),
        ),
        (
            ThresholdKind::PerHourMint,
            mint_hour,
            config.per_hour_mint_limit(vault_hex),
        ),
        (
            ThresholdKind::PerBlockBurn,
            burn_block,
            config.per_block_burn_limit(vault_hex),
        ),
        (
            ThresholdKind::PerHourBurn,
            burn_hour,
            config.per_hour_burn_limit(vault_hex),
        ),
    ];

    for (kind, volume, limit) in checks {
        if volume > limit {
            warn!(
                chain_id,
                block_number,
                vault = %vault_hex,
                volume,
                threshold = limit,
                kind = kind.label(),
                "per-vault threshold breached"
            );
            breaches.push(BreachEvent {
                kind,
                threshold_usdc: limit,
                volume_usdc: volume,
                chain_id,
                block_number,
                vault: Some(vault_hex.to_owned()),
            });
        }
    }

    Ok(())
}

/// Parse and validate the gateway pause parameters from config strings.
///
/// Takes the startup-derived signing state by reference; no key material is
/// decoded here, so a breach cycle never touches raw secret bytes.
fn parse_pause_params<'a>(
    rpc_url: String,
    gw_addr_str: &str,
    signer: &'a PauserSigningKey,
    chain_id: i64,
    fee_bump_bps: u64,
) -> Result<PauseParams<'a>, WatchdogError> {
    use alloy_primitives::Address;
    use std::str::FromStr;

    let gateway_address = Address::from_str(gw_addr_str).map_err(|e| {
        WatchdogError::Pause(format!("invalid gateway address {gw_addr_str:?}: {e}"))
    })?;

    Ok(PauseParams {
        rpc_url,
        gateway_address,
        chain_id: chain_id as u64,
        signer,
        fee_bump_bps,
    })
}
