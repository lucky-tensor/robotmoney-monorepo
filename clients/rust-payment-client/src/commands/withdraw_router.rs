//! Canonical: docs/architecture.md §5 — On-Chain Gateway
//!
//! `rmpc withdraw-router` — sign and broadcast a gateway multi-vault proportional
//! redemption through the Portfolio Router (agent-initiated).
//!
//! Mirrors the structure of `rmpc withdraw` with router-specific preflight:
//!
//! 1. Load config + signer.
//! 2. Acquire the per-agent file lock (single-flight CLI).
//! 3. Run gateway preflight via [`Preflight::run_withdraw_gateway`]:
//!    a. chain id
//!    b. code hash pin
//!    c. gateway paused
//!    d. agent policy active + not expired
//!    e. maxWithdrawPerPayment >= sum(sharesPerLeg)
//!    f. effectiveWithdrawWindowGross + totalShares <= maxWithdrawPerWindow
//! 4. Preview: read router effective weights and display per-leg shares out.
//! 5. Require explicit `--confirm` flag before signing.
//! 6. Compute fees from `eth_feeHistory`.
//! 7. Build the EIP-1559 envelope for
//!    `gateway.withdrawFromRouter(orderId, vaults, sharesPerLeg, deadline,
//!    idempotencyKey)`. The redeem legs are driven by the caller-supplied
//!    `vaults[]`: `vaults[i]` is identity-bound to `sharesPerLeg[i]`
//!    (issue #967), not the router's live weight vector.
//! 8. Sign, broadcast, wait for receipt.
//! 9. Decode `AgentWithdrawalRouted` event log → emit stable JSON on stdout.
//!
//! Exit codes mirror `rmpc withdraw`:
//! - 0 — success.
//! - 2 — preflight refusal, missing --confirm, or on-chain failure.
//! - 3 — startup failure.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, Bytes, LogData, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use serde::Serialize;

use crate::commands::deposit::MAX_DEADLINE_SKEW_SECS;
use crate::commands::self_check::ChecksOutput;
use crate::commands::withdraw::withdraw_vault_preflight;
use crate::config::Config;
use crate::errors::RmpcError;
use crate::fees::compute_fees;
use crate::gateway::RobotMoneyGateway;
use crate::logging::{record_audit, AuditDecision, AuditRecordBuilder};
use crate::network_env::NetworkEnv;
use crate::nonce::AgentLock;
use crate::output::emit;
use crate::policy::{Preflight, PreflightInputs};
use crate::signer::software::{SoftwareSigner, PASSPHRASE_ENV_VAR};
use crate::signer::{require_production_grade_for_write, AgentSigner, SignerBackendKind};
use crate::tx::{
    broadcast, build_eip1559, encode_signed, signing_hash, wait_for_receipt_with, Eip1559Inputs,
};

const EXIT_OK: i32 = 0;
const EXIT_REFUSAL: i32 = 2;
const EXIT_STARTUP_FAIL: i32 = 3;

/// Inputs collected by `main.rs` from the CLI parser.
#[derive(Debug, Clone)]
pub struct Args {
    pub config_path: PathBuf,
    /// Comma-separated vault shares per router leg (decimal strings).
    /// Must match the `vaults` length (one share amount per vault).
    pub shares_per_leg: Vec<String>,
    /// Vault addresses to redeem from (0x-prefixed hex), one per leg,
    /// parallel to `shares_per_leg`. Identity-bound: `shares_per_leg[i]` is
    /// redeemed from `vaults[i]` (issue #967). Drives the redeem legs
    /// directly instead of the router's live weight vector.
    pub vaults: Vec<String>,
    /// Per-leg minimum USDC out (slippage floor), decimal strings, parallel to
    /// `shares_per_leg` (GW-5 / F-11). Empty ⇒ an all-zero floor of the same
    /// length is sent (back-compat). Otherwise the length must equal
    /// `shares_per_leg`.
    pub min_assets_per_leg: Vec<String>,
    /// 32-byte order id, 0x-prefixed hex.
    pub order_id: String,
    /// 32-byte idempotency key. Defaults to order_id when omitted.
    pub idempotency_key: Option<String>,
    pub deadline_secs: u64,
    pub receipt_timeout_secs: u64,
    pub gas_limit: u64,
    /// Optional CLI override for `max_fee_per_gas_cap` in wei.
    pub fee_cap_wei: Option<u64>,
    /// Must be true to proceed past the preview. Without --confirm the
    /// command prints a preview and exits 2.
    pub confirm: bool,
    pub pretty: bool,
}

/// Stable JSON shape emitted on a successful router withdrawal.
#[derive(Debug, Serialize)]
pub struct WithdrawRouterOutput {
    pub status: &'static str,
    pub payment_id: String,
    pub order_id: String,
    pub agent: String,
    pub asset_recipient: String,
    pub router: String,
    pub share_holder: String,
    pub shares_per_leg: Vec<String>,
    pub assets_per_leg: Vec<String>,
    pub block_number: u64,
    pub tx_hash: String,
    pub gas_used: String,
    pub effective_gas_price: String,
}

/// Stable JSON shape emitted on a refusal.
#[derive(Debug, Serialize)]
pub struct WithdrawRouterFailure {
    pub status: &'static str,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<ChecksOutput>,
}

/// Entry point invoked from `main.rs`. Returns the desired process exit code.
pub fn run(args: Args) -> i32 {
    let cfg = match Config::from_path(&args.config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc withdraw-router: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // Parse shares_per_leg.
    let shares_per_leg: Vec<U256> = {
        let mut out = Vec::with_capacity(args.shares_per_leg.len());
        for s in &args.shares_per_leg {
            match U256::from_str(s) {
                Ok(v) => out.push(v),
                Err(e) => {
                    log::error!("rmpc withdraw-router: --shares-per-leg entry {s:?} is not a valid decimal U256: {e}");
                    return EXIT_STARTUP_FAIL;
                }
            }
        }
        out
    };

    let total_shares: U256 = shares_per_leg.iter().fold(U256::ZERO, |a, &b| a + b);
    if total_shares == U256::ZERO {
        log::error!("rmpc withdraw-router: --shares-per-leg must have at least one non-zero entry");
        return EXIT_STARTUP_FAIL;
    }

    // Parse --vaults. The redeem legs are driven by the caller-supplied
    // vaults[] (issue #967): vaults[i] is identity-bound to
    // sharesPerLeg[i], so the two arrays must be the same non-empty length.
    let vaults: Vec<Address> = {
        let mut out = Vec::with_capacity(args.vaults.len());
        for v in &args.vaults {
            match Address::from_str(v) {
                Ok(a) => out.push(a),
                Err(e) => {
                    log::error!(
                        "rmpc withdraw-router: --vaults entry {v:?} is not a valid 0x address: {e}"
                    );
                    return EXIT_REFUSAL;
                }
            }
        }
        out
    };
    if vaults.is_empty() {
        log::error!(
            "rmpc withdraw-router: --vaults must list at least one vault address, one per leg"
        );
        return EXIT_REFUSAL;
    }
    if vaults.len() != shares_per_leg.len() {
        log::error!(
            "rmpc withdraw-router: --vaults length ({}) must equal --shares-per-leg length ({}); \
             vaults[i] is identity-bound to shares_per_leg[i] (issue #967)",
            vaults.len(),
            shares_per_leg.len()
        );
        return EXIT_REFUSAL;
    }

    // Parse the per-leg slippage floor (GW-5 / F-11). Empty ⇒ all-zero of the
    // same length (back-compat); otherwise the length must equal shares_per_leg.
    let min_assets_per_leg: Vec<U256> = if args.min_assets_per_leg.is_empty() {
        vec![U256::ZERO; shares_per_leg.len()]
    } else {
        if args.min_assets_per_leg.len() != shares_per_leg.len() {
            log::error!(
                "rmpc withdraw-router: --min-assets-per-leg length ({}) must equal --shares-per-leg length ({})",
                args.min_assets_per_leg.len(),
                shares_per_leg.len()
            );
            return EXIT_STARTUP_FAIL;
        }
        let mut out = Vec::with_capacity(args.min_assets_per_leg.len());
        for s in &args.min_assets_per_leg {
            match U256::from_str(s) {
                Ok(v) => out.push(v),
                Err(e) => {
                    log::error!("rmpc withdraw-router: --min-assets-per-leg entry {s:?} is not a valid decimal U256: {e}");
                    return EXIT_STARTUP_FAIL;
                }
            }
        }
        out
    };

    let order_id = match B256::from_str(&args.order_id) {
        Ok(b) => b,
        Err(e) => {
            log::error!("rmpc withdraw-router: --order-id is not a 32-byte hex string: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let idempotency_key = match args.idempotency_key.as_deref() {
        None => order_id,
        Some(s) => match B256::from_str(s) {
            Ok(b) => b,
            Err(e) => {
                log::error!(
                    "rmpc withdraw-router: --idempotency-key is not a 32-byte hex string: {e}"
                );
                return EXIT_STARTUP_FAIL;
            }
        },
    };

    let gateway_addr = match Address::from_str(&cfg.gateway_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc withdraw-router: gateway_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let deadline_secs = args.deadline_secs.min(MAX_DEADLINE_SKEW_SECS);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let deadline = now.saturating_add(deadline_secs);

    if let Err(err) = require_production_grade_for_write(cfg.chain_id, SignerBackendKind::Software)
    {
        log::error!("rmpc withdraw-router: {err}");
        emit_refusal(
            &WithdrawRouterFailure {
                status: "refused",
                error: err.name().to_string(),
                message: Some(format!("{err}")),
                agent: None,
                order_id: Some(format!("{order_id:#x}")),
                tx_hash: None,
                checks: None,
            },
            args.pretty,
        );
        return EXIT_REFUSAL;
    }

    // Decrypt keystore.
    let passphrase = match std::env::var(PASSPHRASE_ENV_VAR) {
        Ok(s) => s,
        Err(_) => {
            log::error!(
                "rmpc withdraw-router: ${PASSPHRASE_ENV_VAR} is unset; refusing to prompt on stdin"
            );
            return EXIT_STARTUP_FAIL;
        }
    };
    let signer = match SoftwareSigner::load_with_passphrase(
        &cfg.signer.keystore_path,
        passphrase.as_bytes(),
        cfg.signer.allow_software_fallback,
    ) {
        Ok(s) => s,
        Err(crate::signer::SignerError::ErrSoftwareSignerDisallowed) => {
            log::error!(
                "rmpc withdraw-router: ErrSoftwareSignerDisallowed: [signer].allow_software_fallback must be true"
            );
            emit_refusal(
                &WithdrawRouterFailure {
                    status: "refused",
                    error: "ErrSoftwareSignerDisallowed".to_string(),
                    message: Some(
                        "[signer].allow_software_fallback must be true to use the software keystore"
                            .to_string(),
                    ),
                    agent: None,
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: None,
                    checks: None,
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
        Err(e) => {
            log::error!("rmpc withdraw-router: signer load failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let agent_address = signer.public_address();
    let backend_label = match signer.backend_kind() {
        crate::signer::SignerBackendKind::Software => "software",
        crate::signer::SignerBackendKind::Hsm => "hsm",
        crate::signer::SignerBackendKind::Kms => "kms",
    };

    let mut audit = AuditRecordBuilder {
        agent: format!("{agent_address:#x}"),
        backend: backend_label.to_string(),
        request_type: "withdraw-router".to_string(),
        order_id: format!("{order_id:#x}"),
        idempotency_key: format!("{idempotency_key:#x}"),
        amount: total_shares.to_string(),
        deadline,
        gateway: format!("{gateway_addr:#x}"),
        chain_id: cfg.chain_id,
        tx_hash: None,
        payment_id: None,
    };
    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "withdraw-router: starting agent={} order_id={} total_shares={} chain_id={} network_env={}",
        audit.agent,
        audit.order_id,
        audit.amount,
        audit.chain_id,
        network_env.as_str()
    );
    if let Some(warn) = network_env.production_warning() {
        log::warn!("withdraw-router: {warn}");
    }

    let state_dir = match cfg.resolve_state_dir() {
        Ok(p) => p,
        Err(e) => {
            log::error!("rmpc withdraw-router: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let _lock = match AgentLock::acquire(&state_dir, &agent_address) {
        Ok(l) => l,
        Err(RmpcError::ErrConcurrentInvocation) => {
            record_audit(&audit.build(
                AuditDecision::Refused,
                Some("ErrConcurrentInvocation".to_string()),
            ));
            emit_refusal(
                &WithdrawRouterFailure {
                    status: "refused",
                    error: "ErrConcurrentInvocation".to_string(),
                    message: Some(format!(
                        "another rmpc invocation already holds the lock for agent {agent_address:#x}"
                    )),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: None,
                    checks: None,
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
        Err(e) => {
            log::error!("rmpc withdraw-router: lock acquire failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Replay cache (AZ-RPC-2) -----------------------------------------
    // Look up the paymentId before signing. Uses OP_WITHDRAW and total_shares
    // as the amount field so that router-withdraw entries are disjoint from
    // deposit entries (PAYMENTID-001).
    let replay = match crate::replay_cache::ReplayCache::open(&state_dir) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc withdraw-router: replay cache open failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    match replay.lookup_op(
        crate::replay_cache::OP_WITHDRAW,
        cfg.chain_id,
        gateway_addr,
        agent_address,
        order_id,
        total_shares,
        idempotency_key,
    ) {
        Ok(Some(prior_tx)) => {
            let err = RmpcError::ErrOrderIdAlreadySubmitted {
                tx_hash: prior_tx.clone(),
            };
            record_audit(&audit.build(
                AuditDecision::Refused,
                Some("ErrOrderIdAlreadySubmitted".to_string()),
            ));
            emit_refusal(
                &WithdrawRouterFailure {
                    status: "refused",
                    error: "ErrOrderIdAlreadySubmitted".to_string(),
                    message: Some(format!("{err}")),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: Some(prior_tx),
                    checks: None,
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
        Ok(None) => {}
        Err(e) => {
            log::error!("rmpc withdraw-router: replay cache lookup failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc withdraw-router: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc withdraw-router: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Preflight --------------------------------------------------------
    // Run the withdrawal-specific gateway preflight (chain id, code hash,
    // gateway paused, agent active+expiry, withdrawal window cap) with
    // totalShares as the amount.
    let preflight_result = rt.block_on(async {
        let pf = Preflight::new(&rpc, &cfg);
        pf.run_withdraw_gateway(PreflightInputs {
            signer_address: agent_address,
            amount: total_shares,
        })
        .await
    });
    let report = match preflight_result {
        Ok(r) => r,
        Err(err) => {
            record_audit(&audit.build(AuditDecision::Refused, Some(err.name().to_string())));
            let checks = ChecksOutput::from_err_partial(&err);
            emit_refusal(
                &WithdrawRouterFailure {
                    status: "refused",
                    error: err.name().to_string(),
                    message: Some(format!("{err}")),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: None,
                    checks: Some(checks),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    // -- Per-leg vault share preflight (RPC-7) ----------------------------
    // The gateway preflight above intentionally leaves USDC allowance/balance
    // at zero (N/A for withdrawals). The redeem itself burns the agent's
    // *vault shares*, which the gateway pulls from each source vault, so the
    // agent must (a) hold enough shares in each vault and (b) have approved
    // the gateway to spend them. Previously this command checked neither and
    // reported success right up to an on-chain revert. Run the same per-vault
    // share allowance/balance/paused check the single-vault `withdraw` path
    // uses, once per identity-bound (vault, shares) leg.
    for (vault, leg_shares) in vaults.iter().zip(shares_per_leg.iter()) {
        let leg_result = rt.block_on(async {
            withdraw_vault_preflight(&rpc, *vault, gateway_addr, agent_address, *leg_shares).await
        });
        if let Err(err) = leg_result {
            record_audit(&audit.build(AuditDecision::Refused, Some(err.name().to_string())));
            emit_refusal(
                &WithdrawRouterFailure {
                    status: "refused",
                    error: err.name().to_string(),
                    message: Some(format!("vault {vault:#x}: {err}")),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: None,
                    checks: Some(ChecksOutput::from_report(&report)),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    }

    // -- Explicit confirmation required -----------------------------------
    if !args.confirm {
        eprintln!(
            "rmpc withdraw-router: preview — {} legs, total_shares={total_shares}. Re-run with --confirm to proceed.",
            shares_per_leg.len()
        );
        record_audit(&audit.build(
            AuditDecision::Refused,
            Some("ErrConfirmNotProvided".to_string()),
        ));
        emit_refusal(
            &WithdrawRouterFailure {
                status: "refused",
                error: "ErrConfirmNotProvided".to_string(),
                message: Some(
                    "Pass --confirm to execute. Inspect --get-router first to verify leg amounts."
                        .to_string(),
                ),
                agent: Some(format!("{agent_address:#x}")),
                order_id: Some(format!("{order_id:#x}")),
                tx_hash: None,
                checks: Some(ChecksOutput::from_report(&report)),
            },
            args.pretty,
        );
        return EXIT_REFUSAL;
    }

    // -- Fees -------------------------------------------------------------
    let fee_history_res = rt.block_on(async { rpc.fee_history(5, "latest", &[50.0]).await });
    let fees = match fee_history_res {
        Ok(fh) => match compute_fees(
            &fh,
            cfg.effective_max_fee_per_gas_cap(args.fee_cap_wei) as u128,
            cfg.max_priority_fee_per_gas_cap
                .map_or(u128::MAX, |v| v as u128),
        ) {
            Ok(b) => b,
            Err(e) => {
                record_audit(&audit.build(AuditDecision::Refused, Some(e.name().to_string())));
                emit_refusal(
                    &WithdrawRouterFailure {
                        status: "refused",
                        error: e.name().to_string(),
                        message: Some(format!("{e}")),
                        agent: Some(format!("{agent_address:#x}")),
                        order_id: Some(format!("{order_id:#x}")),
                        tx_hash: None,
                        checks: Some(ChecksOutput::from_report(&report)),
                    },
                    args.pretty,
                );
                return EXIT_REFUSAL;
            }
        },
        Err(e) => {
            log::error!("rmpc withdraw-router: eth_feeHistory failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Nonce ------------------------------------------------------------
    let nonce_res = rt.block_on(async {
        rpc.get_transaction_count(agent_address, Some("pending"))
            .await
    });
    let nonce = match nonce_res {
        Ok(n) => n,
        Err(e) => {
            log::error!("rmpc withdraw-router: eth_getTransactionCount failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Build + sign envelope -------------------------------------------
    let calldata = RobotMoneyGateway::withdrawFromRouterCall {
        orderId: order_id,
        vaults: vaults.clone(),
        sharesPerLeg: shares_per_leg.clone(),
        minAssetsPerLeg: min_assets_per_leg.clone(),
        deadline,
        idempotencyKey: idempotency_key,
    }
    .abi_encode();

    let tx = build_eip1559(Eip1559Inputs {
        chain_id: cfg.chain_id,
        nonce,
        to: gateway_addr,
        gas_limit: args.gas_limit,
        fees,
        value: U256::ZERO,
        input: Bytes::from(calldata),
    });
    let hash = signing_hash(&tx);
    let mut hash_bytes = [0u8; 32];
    hash_bytes.copy_from_slice(hash.as_slice());
    let alloy_sig = match signer.sign_eip1559_hash(&hash_bytes) {
        Ok(s) => s,
        Err(e) => {
            log::error!("rmpc withdraw-router: envelope signing failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let raw = encode_signed(tx, alloy_sig);

    // -- Broadcast --------------------------------------------------------
    let tx_hash = match rt.block_on(async { broadcast(&rpc, &raw).await }) {
        Ok(h) => h,
        Err(e) => {
            log::error!("rmpc withdraw-router: eth_sendRawTransaction failed: {e}");
            record_audit(&audit.build(AuditDecision::BroadcastFailed, Some(e.name().to_string())));
            emit_refusal(
                &WithdrawRouterFailure {
                    status: "refused",
                    error: e.name().to_string(),
                    message: Some(format!("{e}")),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: None,
                    checks: Some(ChecksOutput::from_report(&report)),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    let tx_hash_hex = format!("{tx_hash:#x}");
    audit.tx_hash = Some(tx_hash_hex.clone());

    // Insert into replay cache immediately after broadcast (optimistic).
    if let Err(e) = replay.insert_op(
        crate::replay_cache::OP_WITHDRAW,
        cfg.chain_id,
        gateway_addr,
        agent_address,
        order_id,
        total_shares,
        idempotency_key,
        deadline,
        &tx_hash_hex,
    ) {
        log::warn!("rmpc withdraw-router: replay cache insert failed (non-fatal): {e}");
    }

    // -- Receipt ----------------------------------------------------------
    let max_attempts = args.receipt_timeout_secs.min(u32::MAX as u64) as u32;
    let receipt_res = rt.block_on(async {
        wait_for_receipt_with(&rpc, tx_hash, Duration::from_secs(1), max_attempts.max(1)).await
    });
    let receipt = match receipt_res {
        Ok(r) => r,
        Err(e) => {
            // AZ-RPC-1: timeout ≠ failure — do NOT remove the replay-cache
            // entry. The tx may still land.
            record_audit(&audit.build(AuditDecision::Refused, Some(e.name().to_string())));
            emit_refusal(
                &WithdrawRouterFailure {
                    status: "refused",
                    error: e.name().to_string(),
                    message: Some(format!("{e}")),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: Some(tx_hash_hex.clone()),
                    checks: None,
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    if !receipt.inner.status() {
        // Confirmed on-chain failure: remove the optimistic replay-cache entry.
        withdraw_router_finalize_on_failure(
            &replay,
            cfg.chain_id,
            gateway_addr,
            agent_address,
            order_id,
            total_shares,
            idempotency_key,
        );
        let err = RmpcError::ErrTxReverted {
            tx_hash: tx_hash_hex.clone(),
        };
        record_audit(&audit.build(AuditDecision::Reverted, Some("ErrTxReverted".to_string())));
        emit_refusal(
            &WithdrawRouterFailure {
                status: "refused",
                error: "ErrTxReverted".to_string(),
                message: Some(format!("{err}")),
                agent: Some(format!("{agent_address:#x}")),
                order_id: Some(format!("{order_id:#x}")),
                tx_hash: Some(tx_hash_hex.clone()),
                checks: None,
            },
            args.pretty,
        );
        return EXIT_REFUSAL;
    }

    // -- Decode AgentWithdrawalRouted log ---------------------------------
    let topic0 = RobotMoneyGateway::AgentWithdrawalRouted::SIGNATURE_HASH;
    let log = receipt
        .inner
        .logs()
        .iter()
        .find(|l| l.address() == gateway_addr && l.topics().first() == Some(&topic0));
    let log = match log {
        Some(l) => l,
        None => {
            log::error!("rmpc withdraw-router: AgentWithdrawalRouted event not found in receipt");
            record_audit(&audit.build(
                AuditDecision::Refused,
                Some("ErrRouterWithdrawLogMissing".to_string()),
            ));
            emit_refusal(
                &WithdrawRouterFailure {
                    status: "refused",
                    error: "ErrRouterWithdrawLogMissing".to_string(),
                    message: Some("AgentWithdrawalRouted event not found in receipt".to_string()),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: Some(tx_hash_hex.clone()),
                    checks: None,
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };
    let log_data = LogData::new_unchecked(log.topics().to_vec(), log.data().data.clone());
    let decoded = match RobotMoneyGateway::AgentWithdrawalRouted::decode_log_data(&log_data, true) {
        Ok(d) => d,
        Err(e) => {
            log::error!("rmpc withdraw-router: failed to decode AgentWithdrawalRouted log: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let block_number = receipt.block_number.unwrap_or(0);
    audit.payment_id = Some(format!("{:#x}", decoded.paymentId));
    record_audit(&audit.build(AuditDecision::Signed, None));
    let out = WithdrawRouterOutput {
        status: "success",
        payment_id: format!("{:#x}", decoded.paymentId),
        order_id: format!("{:#x}", decoded.orderId),
        agent: format!("{:#x}", decoded.agent),
        asset_recipient: format!("{:#x}", decoded.assetRecipient),
        router: format!("{:#x}", decoded.router),
        share_holder: format!("{:#x}", decoded.shareHolder),
        shares_per_leg: decoded.sharesPerLeg.iter().map(|s| s.to_string()).collect(),
        assets_per_leg: decoded.assetsPerLeg.iter().map(|a| a.to_string()).collect(),
        block_number,
        tx_hash: tx_hash_hex,
        gas_used: receipt.gas_used.to_string(),
        effective_gas_price: receipt.effective_gas_price.to_string(),
    };
    emit(&out, args.pretty);
    EXIT_OK
}

/// AZ-RPC-2 finalize-on-confirmed-failure: remove the optimistic router-
/// withdraw replay-cache entry when the on-chain receipt shows status == 0.
/// Only call on confirmed revert, never on timeout.
#[allow(clippy::too_many_arguments)]
fn withdraw_router_finalize_on_failure(
    replay: &crate::replay_cache::ReplayCache,
    chain_id: u64,
    gateway: Address,
    agent: Address,
    order_id: B256,
    total_shares: U256,
    idempotency_key: B256,
) {
    if let Err(e) = replay.remove_op(
        crate::replay_cache::OP_WITHDRAW,
        chain_id,
        gateway,
        agent,
        order_id,
        total_shares,
        idempotency_key,
    ) {
        log::warn!(
            "rmpc withdraw-router: replay cache finalize-on-failure remove failed (non-fatal): {e}"
        );
    }
}

fn emit_refusal(out: &WithdrawRouterFailure, pretty: bool) {
    emit(out, pretty);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{Erc20, MockVault};
    use crate::rpc::FailoverRpcClient;
    use alloy_primitives::{address, hex as ahex, keccak256, U256};
    use alloy_sol_types::SolCall;
    use mockito::Matcher;
    use serde_json::json;

    const SIGNER: Address = address!("00000000000000000000000000000000000000aa");
    const GATEWAY: Address = address!("0000000000000000000000000000000000000b00");
    const VAULT: Address = address!("0000000000000000000000000000000000000d00");

    fn enc_bool(v: bool) -> String {
        let mut w = [0u8; 32];
        w[31] = u8::from(v);
        format!("0x{}", ahex::encode(w))
    }

    fn enc_u256(v: U256) -> String {
        format!("0x{}", ahex::encode(v.to_be_bytes::<32>()))
    }

    fn jrpc_result(s: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{s}"}}"#)
    }

    fn match_eth_call_selector(selector: &str) -> Matcher {
        Matcher::AllOf(vec![
            Matcher::PartialJson(json!({"method": "eth_call"})),
            Matcher::Regex(format!(r#""data":"{selector}"#)),
        ])
    }

    fn selector_hex<C: SolCall>() -> String {
        format!("0x{}", ahex::encode(C::SELECTOR))
    }

    async fn install_vault_mocks(
        server: &mut mockito::ServerGuard,
        paused: bool,
        allowance: U256,
        balance: U256,
    ) {
        server
            .mock("POST", "/")
            .match_body(match_eth_call_selector(&selector_hex::<
                MockVault::pausedCall,
            >()))
            .with_status(200)
            .with_body(jrpc_result(&enc_bool(paused)))
            .expect_at_least(0)
            .create_async()
            .await;
        server
            .mock("POST", "/")
            .match_body(match_eth_call_selector(
                &selector_hex::<Erc20::allowanceCall>(),
            ))
            .with_status(200)
            .with_body(jrpc_result(&enc_u256(allowance)))
            .expect_at_least(0)
            .create_async()
            .await;
        server
            .mock("POST", "/")
            .match_body(match_eth_call_selector(
                &selector_hex::<Erc20::balanceOfCall>(),
            ))
            .with_status(200)
            .with_body(jrpc_result(&enc_u256(balance)))
            .expect_at_least(0)
            .create_async()
            .await;
    }

    /// RPC-7: the router-withdraw preflight runs the per-leg share check and
    /// REFUSES when the agent's vault-share allowance to the gateway is
    /// insufficient, rather than returning success against a hard-coded zero.
    #[tokio::test]
    async fn router_leg_preflight_refuses_on_insufficient_allowance() {
        let mut server = mockito::Server::new_async().await;
        // not paused, allowance too low, balance ample.
        install_vault_mocks(&mut server, false, U256::from(1u64), U256::from(u128::MAX)).await;
        let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();
        let err = withdraw_vault_preflight(&rpc, VAULT, GATEWAY, SIGNER, U256::from(1_000u64))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RmpcError::ErrShareAllowanceInsufficient),
            "router leg must refuse on insufficient share allowance; got {err:?}"
        );
    }

    /// RPC-7: the router-withdraw preflight REFUSES when the agent's vault
    /// share balance cannot cover the leg's shares.
    #[tokio::test]
    async fn router_leg_preflight_refuses_on_insufficient_balance() {
        let mut server = mockito::Server::new_async().await;
        // not paused, ample allowance, balance too low.
        install_vault_mocks(&mut server, false, U256::from(u128::MAX), U256::from(1u64)).await;
        let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();
        let err = withdraw_vault_preflight(&rpc, VAULT, GATEWAY, SIGNER, U256::from(1_000u64))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RmpcError::ErrShareBalanceInsufficient),
            "router leg must refuse on insufficient share balance; got {err:?}"
        );
    }

    /// With ample allowance and balance, a leg preflight passes — confirming
    /// the check reads real values rather than always refusing.
    #[tokio::test]
    async fn router_leg_preflight_passes_with_ample_allowance_and_balance() {
        let mut server = mockito::Server::new_async().await;
        install_vault_mocks(
            &mut server,
            false,
            U256::from(u128::MAX),
            U256::from(u128::MAX),
        )
        .await;
        let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();
        let result =
            withdraw_vault_preflight(&rpc, VAULT, GATEWAY, SIGNER, U256::from(100u64)).await;
        assert!(result.is_ok(), "expected ok, got {result:?}");
    }

    #[test]
    fn withdraw_from_router_selector_matches_canonical_signature() {
        let canonical = "withdrawFromRouter(bytes32,address[],uint256[],uint256[],uint64,bytes32)";
        let expected = &keccak256(canonical.as_bytes())[..4];
        let actual = RobotMoneyGateway::withdrawFromRouterCall::SELECTOR;
        assert_eq!(&actual, expected, "withdrawFromRouter selector drift");
    }

    #[test]
    fn agent_withdrawal_routed_event_topic0_matches() {
        let canonical =
            b"AgentWithdrawalRouted(bytes32,bytes32,address,address,address,uint256[],uint256[],address,uint64)";
        let expected = keccak256(canonical);
        let actual = RobotMoneyGateway::AgentWithdrawalRouted::SIGNATURE_HASH;
        assert_eq!(actual, expected, "AgentWithdrawalRouted topic0 drift");
    }

    #[test]
    fn shares_per_leg_encodes_correctly() {
        // Verify the ABI encoding is stable for a two-leg call.
        let legs = vec![U256::from(60_000_000u64), U256::from(40_000_000u64)];
        let vaults = vec![
            alloy_primitives::Address::repeat_byte(1),
            alloy_primitives::Address::repeat_byte(2),
        ];
        let floors = vec![U256::from(59_700_000u64), U256::from(39_800_000u64)];
        let call = RobotMoneyGateway::withdrawFromRouterCall {
            orderId: alloy_primitives::B256::ZERO,
            vaults,
            sharesPerLeg: legs,
            minAssetsPerLeg: floors,
            deadline: 0u64,
            idempotencyKey: alloy_primitives::B256::ZERO,
        };
        let encoded = call.abi_encode();
        // Must start with the 4-byte selector.
        assert_eq!(
            &encoded[..4],
            RobotMoneyGateway::withdrawFromRouterCall::SELECTOR
        );
        assert!(encoded.len() > 4, "encoded call must have non-trivial body");
    }
}
