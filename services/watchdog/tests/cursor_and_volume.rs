//! Reliable volume accounting + breach actuation (issue #990).
//!
//! Covers the four acceptance criteria for the off-chain scan-remediation work:
//!
//! - **AC-1 / WD-6:** advancing the indexed head by N>1 blocks with a spike in a
//!   non-latest block makes the watchdog raise a per-block breach for *that* block
//!   (the cursor loop evaluates every block, not just the latest).
//! - **AC-2 / IDX-7:** a single routed deposit contributes its amount exactly once
//!   to mint volume (no parent + legs double count).
//! - **AC-3 / IDX-4:** a direct ERC-4626 deposit and a (registered-vault)
//!   withdrawal are reflected in mint / burn volume.
//! - **AC-4 / WD-1:** a pause RPC exceeding `sla.max_response_secs` is aborted and
//!   the alert path still dispatches.
//!
//! Every test skips cleanly when Docker is unavailable (no Postgres fixture).

mod common;

use bigdecimal::BigDecimal;
use common::{try_pg_fixture, HangingServer, MockWebhookServer};
use reqwest::Client;
use std::collections::HashMap;
use watchdog::{
    alert::ThresholdKind,
    config::{
        ActionConfig, ActionMode, Config, GlobalThresholds, PauserKeyHex, SlaConfig,
        VaultThresholds,
    },
    receipt_liveness::ReceiptLivenessConfig,
    volume::{
        burn_volume_per_block, mint_volume_per_block, mint_volume_per_block_for_vault,
        mint_volume_per_hour_for_vault,
    },
    watchdog::{run_cycle, run_cycles_since_cursor, CycleResult},
};

const CHAIN_ID: i64 = 8453;
const VAULT_A: [u8; 20] = [0xA1; 20];
const VAULT_B: [u8; 20] = [0xB2; 20];

/// Lowercase hex (no `0x`) of a 20-byte vault address, as the config keys it.
fn vault_hex(addr: &[u8; 20]) -> String {
    addr.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- seed helpers ----------------------------------------------------------

async fn seed_chain(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO chains (chain_id, name, rpc_label) VALUES ($1, 'test', 'test') ON CONFLICT DO NOTHING",
    )
    .bind(CHAIN_ID)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_block(pool: &sqlx::PgPool, block_number: i64, timestamp: i64) {
    let mut hash = [0u8; 32];
    hash[0] = (block_number & 0xff) as u8;
    hash[1] = ((block_number >> 8) & 0xff) as u8;
    sqlx::query(
        "INSERT INTO blocks (chain_id, block_number, hash, parent_hash, timestamp) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
    )
    .bind(CHAIN_ID)
    .bind(block_number)
    .bind(&hash[..])
    .bind(&[0u8; 32][..])
    .bind(timestamp)
    .execute(pool)
    .await
    .unwrap();
}

/// Insert one `agent_deposits` row. `vault` NULL = routed parent; Some = single-vault.
#[allow(clippy::too_many_arguments)]
async fn insert_agent_deposit(
    pool: &sqlx::PgPool,
    block_number: i64,
    log_index: i32,
    tx_hash: &[u8; 32],
    amount: u64,
    vault: Option<&[u8; 20]>,
) {
    sqlx::query(
        "INSERT INTO agent_deposits \
         (chain_id, block_number, log_index, tx_hash, payment_id, order_id, agent, \
          share_receiver, amount, shares_minted, window_id, vault) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) ON CONFLICT DO NOTHING",
    )
    .bind(CHAIN_ID)
    .bind(block_number)
    .bind(log_index)
    .bind(&tx_hash[..])
    .bind(&[0u8; 32][..])
    .bind(&[0u8; 32][..])
    .bind(&[0xBBu8; 20][..])
    .bind(&[0xCCu8; 20][..])
    .bind(BigDecimal::from(amount))
    .bind(BigDecimal::from(0u64))
    .bind(1i64)
    .bind(vault.map(|v| &v[..]))
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_router_leg(
    pool: &sqlx::PgPool,
    block_number: i64,
    log_index: i32,
    tx_hash: &[u8; 32],
    vault: &[u8; 20],
    amount: u64,
) {
    sqlx::query(
        "INSERT INTO router_deposit_legs \
         (chain_id, block_number, log_index, tx_hash, payment_id, depositor, vault, amount, shares, weight_bps) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT DO NOTHING",
    )
    .bind(CHAIN_ID)
    .bind(block_number)
    .bind(log_index)
    .bind(&tx_hash[..])
    .bind(&tx_hash[..])
    .bind(&[0xBBu8; 20][..])
    .bind(&vault[..])
    .bind(BigDecimal::from(amount))
    .bind(BigDecimal::from(0u64))
    .bind(BigDecimal::from(5000u64))
    .execute(pool)
    .await
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn insert_vault_transfer(
    pool: &sqlx::PgPool,
    block_number: i64,
    log_index: i32,
    tx_hash: &[u8; 32],
    vault: &[u8; 20],
    direction: &str,
    assets: u64,
) {
    sqlx::query(
        "INSERT INTO vault_transfer_events \
         (chain_id, block_number, log_index, tx_hash, vault, direction, caller, owner_or_receiver, assets, shares) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT DO NOTHING",
    )
    .bind(CHAIN_ID)
    .bind(block_number)
    .bind(log_index)
    .bind(&tx_hash[..])
    .bind(&vault[..])
    .bind(direction)
    .bind(&[0xDDu8; 20][..])
    .bind(&[0xEEu8; 20][..])
    .bind(BigDecimal::from(assets))
    .bind(BigDecimal::from(0u64))
    .execute(pool)
    .await
    .unwrap();
}

fn tx_hash(seed: u8) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[0] = seed;
    h
}

fn alert_only_config(per_block_mint: u64, per_block_burn: u64, webhook_url: &str) -> Config {
    Config {
        global: GlobalThresholds {
            per_block_mint_limit_usdc: per_block_mint.to_string(),
            per_hour_mint_limit_usdc: "999999999999".to_owned(),
            per_block_burn_limit_usdc: per_block_burn.to_string(),
            per_hour_burn_limit_usdc: "999999999999".to_owned(),
        },
        action: ActionConfig {
            mode: ActionMode::Alert,
            webhook_url: Some(webhook_url.to_owned()),
            gateway_rpc_url: None,
            gateway_address: None,
            pauser_private_key_hex: None,
            pause_fee_bump_bps: 1500,
        },
        sla: SlaConfig {
            max_response_secs: 300,
        },
        vault: HashMap::new(),
        // The consensus-receipt liveness monitor is off by default, so these
        // volume-path fixtures are unaffected by it (issue #1247 task 4.13).
        consensus_receipts: ReceiptLivenessConfig::default(),
    }
}

/// Build an alert-only config with a single per-vault override for `vault`.
///
/// The global limits are set deliberately loose so the per-vault path is the only
/// thing that can fire — exercising the WD-4 enforcement in isolation.
fn per_vault_config(
    global_limit: u64,
    vault: &[u8; 20],
    vt: VaultThresholds,
    webhook_url: &str,
) -> Config {
    let mut vaults = HashMap::new();
    vaults.insert(vault_hex(vault), vt);
    Config {
        global: GlobalThresholds {
            per_block_mint_limit_usdc: global_limit.to_string(),
            per_hour_mint_limit_usdc: global_limit.to_string(),
            per_block_burn_limit_usdc: global_limit.to_string(),
            per_hour_burn_limit_usdc: global_limit.to_string(),
        },
        action: ActionConfig {
            mode: ActionMode::Alert,
            webhook_url: Some(webhook_url.to_owned()),
            gateway_rpc_url: None,
            gateway_address: None,
            pauser_private_key_hex: None,
            pause_fee_bump_bps: 1500,
        },
        sla: SlaConfig {
            max_response_secs: 300,
        },
        vault: vaults,
        // The consensus-receipt liveness monitor is off by default, so these
        // volume-path fixtures are unaffected by it (issue #1247 task 4.13).
        consensus_receipts: ReceiptLivenessConfig::default(),
    }
}

// ---- AC: per-vault breach under a passing global limit (WD-4) ---------------

/// A single vault whose per-block mint exceeds *its* tighter per-vault threshold
/// raises a breach naming that vault, even though the all-vault aggregate stays
/// under the (looser) global limit.
#[tokio::test]
async fn per_vault_breach_under_passing_global_limit() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
    let server = MockWebhookServer::start().await;
    let client = Client::new();

    // Global cap 1_000_000; vault A capped at 100_000. The spike (150_000) is below
    // global but above vault A's override.
    let vt = VaultThresholds {
        per_block_mint_limit_usdc: Some("100000".to_owned()),
        per_hour_mint_limit_usdc: Some("1000000".to_owned()),
        per_block_burn_limit_usdc: Some("1000000".to_owned()),
        per_hour_burn_limit_usdc: Some("1000000".to_owned()),
    };
    let config = per_vault_config(1_000_000, &VAULT_A, vt, &server.url);

    seed_chain(&fx.pool).await;
    seed_block(&fx.pool, 500, 1_700_000_000).await;
    // 150_000 in vault A: under the 1_000_000 global, over the 100_000 vault cap.
    insert_agent_deposit(&fx.pool, 500, 0, &tx_hash(60), 150_000, Some(&VAULT_A)).await;

    let result = run_cycle(&fx.pool, &config, &client, CHAIN_ID, 500, None)
        .await
        .expect("cycle must not error");

    match result {
        CycleResult::Breached(kinds) => assert!(
            kinds.contains(&ThresholdKind::PerBlockMint),
            "per-vault mint cap breach must surface as PerBlockMint; got {kinds:?}"
        ),
        other => panic!("expected Breached on per-vault cap, got {other:?}"),
    }

    // The dispatched alert must name vault A, not "global".
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    let captures = server.drain_captures();
    assert!(
        !captures.is_empty(),
        "per-vault breach must dispatch an alert"
    );
    let expected = vault_hex(&VAULT_A);
    let named = captures.iter().any(|c| {
        serde_json::from_slice::<serde_json::Value>(&c.body)
            .ok()
            .and_then(|v| {
                v["payload"]["custom_details"]["vault"]
                    .as_str()
                    .map(|s| s == expected)
            })
            .unwrap_or(false)
    });
    assert!(named, "alert must identify the breaching vault {expected}");

    server.shutdown();
}

/// Volume must be accounted *per vault*: a spike in vault B does not dilute into
/// vault A's per-vault check, and vault A's scoped volume only counts vault A.
#[tokio::test]
async fn per_vault_volume_is_accounted_per_vault() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
    seed_chain(&fx.pool).await;
    seed_block(&fx.pool, 600, 1_700_000_000).await;

    // Vault A: 120_000. Vault B: 900_000 (a spike). The all-vault aggregate is
    // 1_020_000, but the per-vault A figure must be exactly 120_000.
    insert_agent_deposit(&fx.pool, 600, 0, &tx_hash(70), 120_000, Some(&VAULT_A)).await;
    insert_agent_deposit(&fx.pool, 600, 1, &tx_hash(71), 900_000, Some(&VAULT_B)).await;

    let agg = mint_volume_per_block(&fx.pool, CHAIN_ID, 600)
        .await
        .unwrap();
    assert_eq!(agg, 1_020_000, "all-vault aggregate sums both vaults");

    let vault_a = mint_volume_per_block_for_vault(&fx.pool, CHAIN_ID, 600, &VAULT_A[..])
        .await
        .unwrap();
    assert_eq!(
        vault_a, 120_000,
        "vault A per-vault volume must NOT include vault B's spike"
    );

    let vault_b = mint_volume_per_block_for_vault(&fx.pool, CHAIN_ID, 600, &VAULT_B[..])
        .await
        .unwrap();
    assert_eq!(
        vault_b, 900_000,
        "vault B per-vault volume counts only vault B"
    );

    // Per-hour scoping is likewise per vault.
    let vault_a_hour =
        mint_volume_per_hour_for_vault(&fx.pool, CHAIN_ID, 1_700_000_000, 3600, &VAULT_A[..])
            .await
            .unwrap();
    assert_eq!(
        vault_a_hour, 120_000,
        "vault A per-hour volume excludes vault B"
    );
}

// ---- AC-1 / WD-6 -----------------------------------------------------------

/// The indexed head advances by N>1 blocks; a spike lives in a NON-latest block.
/// The cursor loop must evaluate that block and raise a per-block breach for it.
#[tokio::test]
async fn cursor_loop_breaches_on_spike_in_non_latest_block() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
    let server = MockWebhookServer::start().await;
    let config = alert_only_config(500_000, 999_999_999, &server.url);
    let client = Client::new();

    seed_chain(&fx.pool).await;
    // Three consecutive blocks; the SPIKE is in the middle block (101), not the
    // latest (102). A latest-block-only watchdog would never see it.
    seed_block(&fx.pool, 100, 1_700_000_000).await;
    seed_block(&fx.pool, 101, 1_700_000_012).await;
    seed_block(&fx.pool, 102, 1_700_000_024).await;

    insert_agent_deposit(&fx.pool, 100, 0, &tx_hash(10), 100_000, Some(&VAULT_A)).await;
    insert_agent_deposit(&fx.pool, 101, 0, &tx_hash(11), 600_000, Some(&VAULT_A)).await; // SPIKE
    insert_agent_deposit(&fx.pool, 102, 0, &tx_hash(12), 100_000, Some(&VAULT_A)).await;

    // Cursor starts at 99 so the loop evaluates 100, 101, 102.
    watchdog::watchdog::store_cursor(&fx.pool, CHAIN_ID, 99)
        .await
        .unwrap();

    let result = run_cycles_since_cursor(&fx.pool, &config, &client, CHAIN_ID, 102, None)
        .await
        .expect("cycle must not error");

    match result {
        CycleResult::Breached(kinds) => assert!(
            kinds.contains(&ThresholdKind::PerBlockMint),
            "spike in block 101 must produce a per-block mint breach; got {kinds:?}"
        ),
        other => panic!("expected Breached, got {other:?}"),
    }

    // The cursor must have advanced to the latest evaluated block.
    let cursor = watchdog::watchdog::load_cursor(&fx.pool, CHAIN_ID)
        .await
        .unwrap();
    assert_eq!(
        cursor,
        Some(102),
        "cursor must advance past the whole range"
    );

    // A second run with no new blocks reports NoData and does not re-alert.
    let again = run_cycles_since_cursor(&fx.pool, &config, &client, CHAIN_ID, 102, None)
        .await
        .unwrap();
    assert_eq!(again, CycleResult::NoData, "no new blocks ⇒ NoData");

    server.shutdown();
}

// ---- AC-2 / IDX-7 ----------------------------------------------------------

/// A single routed deposit writes a parent `agent_deposits` row (vault NULL) plus
/// per-leg `router_deposit_legs` rows summing to the same amount. Mint volume must
/// count the amount exactly once (legs only), not parent + legs (~2×).
#[tokio::test]
async fn routed_deposit_counted_once_in_mint_volume() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
    seed_chain(&fx.pool).await;
    seed_block(&fx.pool, 200, 1_700_000_000).await;

    let tx = tx_hash(20);
    // Parent: vault NULL, full amount 300_000.
    insert_agent_deposit(&fx.pool, 200, 0, &tx, 300_000, None).await;
    // Two legs summing to 300_000.
    insert_router_leg(&fx.pool, 200, 1, &tx, &VAULT_A, 200_000).await;
    insert_router_leg(&fx.pool, 200, 2, &tx, &[0xA2; 20], 100_000).await;

    let mint = mint_volume_per_block(&fx.pool, CHAIN_ID, 200)
        .await
        .unwrap();
    assert_eq!(
        mint, 300_000,
        "routed deposit must be counted once (legs), not parent+legs"
    );
}

// ---- AC-3 / IDX-4 + IDX-6 --------------------------------------------------

/// A direct ERC-4626 deposit (no gateway `agent_deposits` row) must contribute to
/// mint volume (IDX-4); a vault withdrawal must contribute to burn volume.
#[tokio::test]
async fn direct_deposit_and_withdrawal_reflected_in_volume() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
    seed_chain(&fx.pool).await;
    seed_block(&fx.pool, 300, 1_700_000_000).await;

    // Direct deposit: vault_transfer_events deposit with NO agent_deposits tx.
    insert_vault_transfer(&fx.pool, 300, 0, &tx_hash(30), &VAULT_A, "deposit", 250_000).await;
    // A registered-vault withdrawal.
    insert_vault_transfer(
        &fx.pool,
        300,
        1,
        &tx_hash(31),
        &VAULT_A,
        "withdrawal",
        175_000,
    )
    .await;

    let mint = mint_volume_per_block(&fx.pool, CHAIN_ID, 300)
        .await
        .unwrap();
    let burn = burn_volume_per_block(&fx.pool, CHAIN_ID, 300)
        .await
        .unwrap();
    assert_eq!(
        mint, 250_000,
        "direct ERC-4626 deposit must be in mint volume"
    );
    assert_eq!(burn, 175_000, "withdrawal must be in burn volume");
}

/// A gateway-originated ERC-4626 Deposit log (same tx as an `agent_deposits` row)
/// must NOT be double-counted on top of the gateway deposit.
#[tokio::test]
async fn gateway_deposit_not_double_counted_via_vault_transfer() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
    seed_chain(&fx.pool).await;
    seed_block(&fx.pool, 310, 1_700_000_000).await;

    let tx = tx_hash(40);
    // Single-vault gateway deposit.
    insert_agent_deposit(&fx.pool, 310, 0, &tx, 400_000, Some(&VAULT_A)).await;
    // The vault emits an ERC-4626 Deposit in the SAME tx; must be deduped out.
    insert_vault_transfer(&fx.pool, 310, 1, &tx, &VAULT_A, "deposit", 400_000).await;

    let mint = mint_volume_per_block(&fx.pool, CHAIN_ID, 310)
        .await
        .unwrap();
    assert_eq!(
        mint, 400_000,
        "gateway deposit's vault Deposit log must not double-count"
    );
}

// ---- AC-4 / WD-1 -----------------------------------------------------------

/// A pause RPC that hangs past the SLA budget must be aborted, and the alert must
/// still be dispatched (alert is sent before the pause and is not starved).
#[tokio::test]
async fn pause_rpc_timeout_does_not_starve_alert() {
    let Some(fx) = try_pg_fixture().await else {
        return;
    };
    let webhook = MockWebhookServer::start().await;
    let hung_rpc = HangingServer::start().await;

    // pause_and_alert with a 1-second SLA; the hung RPC will exceed it.
    let mut config = Config {
        global: GlobalThresholds {
            per_block_mint_limit_usdc: "500000".to_owned(),
            per_hour_mint_limit_usdc: "999999999999".to_owned(),
            per_block_burn_limit_usdc: "999999999999".to_owned(),
            per_hour_burn_limit_usdc: "999999999999".to_owned(),
        },
        action: ActionConfig {
            mode: ActionMode::PauseAndAlert,
            webhook_url: Some(webhook.url.clone()),
            gateway_rpc_url: Some(hung_rpc.url.clone()),
            gateway_address: Some("0x000000000000000000000000000000000000dEaD".to_owned()),
            pauser_private_key_hex: Some(PauserKeyHex::new(
                "1111111111111111111111111111111111111111111111111111111111111111".to_owned(),
            )),
            pause_fee_bump_bps: 1500,
        },
        sla: SlaConfig {
            max_response_secs: 1,
        },
        vault: HashMap::new(),
        // The consensus-receipt liveness monitor is off by default, so these
        // volume-path fixtures are unaffected by it (issue #1247 task 4.13).
        consensus_receipts: ReceiptLivenessConfig::default(),
    };
    // Derive the signing state the way the daemon does — once, at "startup" —
    // and confirm the raw hex is gone from the config afterwards (issue #1357).
    let pauser = config
        .take_pauser_signing_key()
        .expect("configured pauser key must derive")
        .expect("a pauser key was configured");
    assert!(
        !config.action.has_pauser_key(),
        "the raw pauser key must not outlive the startup extraction"
    );
    let config = config;
    let client = Client::new();

    seed_chain(&fx.pool).await;
    seed_block(&fx.pool, 400, 1_700_000_000).await;
    insert_agent_deposit(&fx.pool, 400, 0, &tx_hash(50), 600_000, Some(&VAULT_A)).await;

    let start = std::time::Instant::now();
    let result = run_cycle(&fx.pool, &config, &client, CHAIN_ID, 400, Some(&pauser))
        .await
        .expect("cycle must not error even when pause RPC hangs");
    let elapsed = start.elapsed();

    // Breach detected.
    match result {
        CycleResult::Breached(kinds) => assert!(kinds.contains(&ThresholdKind::PerBlockMint)),
        other => panic!("expected Breached, got {other:?}"),
    }

    // The alert must have landed despite the hung pause.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    let captures = webhook.drain_captures();
    assert!(
        !captures.is_empty(),
        "alert must dispatch even though the pause RPC hung"
    );

    // The cycle must not hang forever: it returns within a small multiple of the
    // SLA budget (alert + pause each bounded by 1s).
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "cycle must return promptly after SLA abort, took {elapsed:?}"
    );

    webhook.shutdown();
    hung_rpc.shutdown();
}
