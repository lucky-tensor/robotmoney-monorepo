//! Canonical: docs/architecture.md §4 — High-Level Flow
//! (See also: docs/technical/rmpc-read-output-contract.md)
//!
//! `rmpc withdraw` — sign and broadcast a gateway redemption (agent-initiated).
//!
//! Per issue #312. Mirrors the structure of `rmpc deposit` with a
//! withdraw-specific preflight:
//!
//! 1. Load config + signer.
//! 2. Acquire the per-agent file lock (single-flight CLI).
//! 3. Run gateway preflight via [`Preflight::run_withdraw_gateway`]:
//!    a. chain id
//!    b. code hash pin
//!    c. gateway paused
//!    d. agent policy active + not expired
//!    e. shares <= agent maxWithdrawPerPayment (withdrawal-specific cap)
//!    f. effectiveWithdrawWindowGross + shares <= maxWithdrawPerWindow
//!    (issue #449: rolling-window cap; preserves loss-budget at boundaries)
//!    g. vault.paused() == false
//!    h. vault.allowance(agent, gateway) >= shares
//!    i. vault.balanceOf(agent) >= shares
//! 4. Compute fees from `eth_feeHistory`.
//! 5. Build the EIP-1559 envelope for `gateway.withdraw(...)`.
//! 6. Sign, broadcast, wait for receipt.
//! 7. Decode the `AgentWithdrawal` event log → emit stable JSON on stdout.
//!
//! Exit codes mirror `rmpc deposit`:
//! - 0 — success.
//! - 2 — preflight refusal or on-chain failure.
//! - 3 — startup failure.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, LogData, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use serde::Serialize;

use crate::commands::deposit::MAX_DEADLINE_SKEW_SECS;
use crate::commands::self_check::ChecksOutput;
use crate::config::Config;
use crate::errors::RmpcError;
use crate::fees::compute_fees;
use crate::gateway::{Erc20, MockVault, RobotMoneyGateway};
use crate::logging::{record_audit, AuditDecision, AuditRecordBuilder};
use crate::network_env::NetworkEnv;
use crate::nonce::AgentLock;
use crate::policy::{Preflight, PreflightInputs};
use crate::rpc::{CallRequest, FailoverRpcClient};
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
    /// Vault shares to redeem (in share units).
    pub shares: String,
    /// Source vault address (0x-prefixed hex).
    pub source_vault: String,
    /// 32-byte order id, 0x-prefixed hex.
    pub order_id: String,
    /// 32-byte idempotency key. Defaults to order_id when omitted.
    pub idempotency_key: Option<String>,
    pub deadline_secs: u64,
    pub receipt_timeout_secs: u64,
    pub gas_limit: u64,
    /// Optional CLI override for `max_fee_per_gas_cap` in wei.
    pub fee_cap_wei: Option<u64>,
    pub pretty: bool,
}

/// Stable JSON shape emitted on a successful withdrawal.
#[derive(Debug, Serialize)]
pub struct WithdrawOutput {
    pub status: &'static str,
    pub payment_id: String,
    pub order_id: String,
    pub agent: String,
    pub asset_recipient: String,
    pub source_vault: String,
    pub shares: String,
    pub assets_out: String,
    pub block_number: u64,
    pub tx_hash: String,
    pub gas_used: String,
    pub effective_gas_price: String,
}

/// Stable JSON shape emitted on a refusal.
#[derive(Debug, Serialize)]
pub struct WithdrawFailure {
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
            log::error!("rmpc withdraw: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let shares = match U256::from_str(&args.shares) {
        Ok(v) => v,
        Err(e) => {
            log::error!("rmpc withdraw: --shares must be a decimal U256: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let source_vault = match Address::from_str(&args.source_vault) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc withdraw: --source-vault is not a valid address: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let order_id = match B256::from_str(&args.order_id) {
        Ok(b) => b,
        Err(e) => {
            log::error!("rmpc withdraw: --order-id is not a 32-byte hex string: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let idempotency_key = match args.idempotency_key.as_deref() {
        None => order_id,
        Some(s) => match B256::from_str(s) {
            Ok(b) => b,
            Err(e) => {
                log::error!("rmpc withdraw: --idempotency-key is not a 32-byte hex string: {e}");
                return EXIT_STARTUP_FAIL;
            }
        },
    };

    let gateway_addr = match Address::from_str(&cfg.gateway_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc withdraw: gateway_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let deadline_secs = args.deadline_secs.min(MAX_DEADLINE_SKEW_SECS);

    if let Err(err) = require_production_grade_for_write(cfg.chain_id, SignerBackendKind::Software)
    {
        log::error!("rmpc withdraw: {err}");
        emit_refusal(
            &WithdrawFailure {
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
                "rmpc withdraw: ${PASSPHRASE_ENV_VAR} is unset; refusing to prompt on stdin"
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
                "rmpc withdraw: ErrSoftwareSignerDisallowed: [signer].allow_software_fallback must be true"
            );
            emit_refusal(
                &WithdrawFailure {
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
            log::error!("rmpc withdraw: signer load failed: {e}");
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
        request_type: "withdraw".to_string(),
        order_id: format!("{order_id:#x}"),
        idempotency_key: format!("{idempotency_key:#x}"),
        amount: shares.to_string(),
        deadline: 0,
        gateway: format!("{gateway_addr:#x}"),
        chain_id: cfg.chain_id,
        tx_hash: None,
        payment_id: None,
    };
    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "withdraw: starting agent={} order_id={} shares={} chain_id={} network_env={}",
        audit.agent,
        audit.order_id,
        audit.amount,
        audit.chain_id,
        network_env.as_str()
    );
    if let Some(warn) = network_env.production_warning() {
        log::warn!("withdraw: {warn}");
    }

    let state_dir = match cfg.resolve_state_dir() {
        Ok(p) => p,
        Err(e) => {
            log::error!("rmpc withdraw: {e}");
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
                &WithdrawFailure {
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
            log::error!("rmpc withdraw: lock acquire failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Replay cache (AZ-RPC-2) -----------------------------------------
    // Look up the paymentId in the client-side replay cache before signing.
    // Uses OP_WITHDRAW so withdraw entries are disjoint from deposit entries
    // even when all other hash inputs are identical (PAYMENTID-001).
    // `shares` is the cache amount field (mirrors the on-chain formula).
    let replay = match crate::replay_cache::ReplayCache::open(&state_dir) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc withdraw: replay cache open failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    match replay.lookup_op(
        crate::replay_cache::OP_WITHDRAW,
        cfg.chain_id,
        gateway_addr,
        agent_address,
        order_id,
        shares,
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
                &WithdrawFailure {
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
            log::error!("rmpc withdraw: replay cache lookup failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc withdraw: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc withdraw: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Deadline from block timestamp ------------------------------------
    let deadline = match rt.block_on(async {
        let block_number = rpc.block_number().await?;
        rpc.block_timestamp(block_number).await
    }) {
        Ok(ts) => {
            let d = ts.saturating_add(deadline_secs);
            audit.deadline = d;
            d
        }
        Err(e) => {
            log::error!("rmpc withdraw: failed to fetch block timestamp for deadline: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Preflight --------------------------------------------------------
    // Run the withdrawal-specific gateway preflight (chain id, code hash,
    // gateway paused, agent active+expiry, withdrawal window cap).
    // Unlike the deposit path this checks maxWithdrawPerPayment,
    // maxWithdrawPerWindow, and effectiveWithdrawWindowGross — not the
    // deposit caps — so valid withdrawals are not refused and out-of-policy
    // withdrawals are caught before signing (issue #371, #449).
    let preflight_result = rt.block_on(async {
        let pf = Preflight::new(&rpc, &cfg);
        pf.run_withdraw_gateway(PreflightInputs {
            signer_address: agent_address,
            amount: shares,
        })
        .await
    });
    let report = match preflight_result {
        Ok(r) => r,
        Err(err) => {
            record_audit(&audit.build(AuditDecision::Refused, Some(err.name().to_string())));
            let checks = ChecksOutput::from_err_partial(&err);
            emit_refusal(
                &WithdrawFailure {
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

    // -- Withdraw-specific preflight: vault checks -------------------------
    let vault_preflight_result = rt.block_on(async {
        withdraw_vault_preflight(&rpc, source_vault, gateway_addr, agent_address, shares).await
    });
    if let Err(err) = vault_preflight_result {
        record_audit(&audit.build(AuditDecision::Refused, Some(err.name().to_string())));
        let checks = ChecksOutput::from_report(&report);
        emit_refusal(
            &WithdrawFailure {
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
                record_audit(
                    &audit.build(AuditDecision::Refused, Some(e.name().to_string())),
                );
                emit_refusal(
                    &WithdrawFailure {
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
            log::error!("rmpc withdraw: eth_feeHistory failed: {e}");
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
            log::error!("rmpc withdraw: eth_getTransactionCount failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Build + sign envelope -------------------------------------------
    let calldata = RobotMoneyGateway::withdrawCall {
        orderId: order_id,
        shares,
        sourceVault: source_vault,
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
            log::error!("rmpc withdraw: envelope signing failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let raw = encode_signed(tx, alloy_sig);

    // -- Broadcast --------------------------------------------------------
    let tx_hash = match rt.block_on(async { broadcast(&rpc, &raw).await }) {
        Ok(h) => h,
        Err(e) => {
            log::error!("rmpc withdraw: eth_sendRawTransaction failed: {e}");
            record_audit(&audit.build(
                AuditDecision::BroadcastFailed,
                Some(e.name().to_string()),
            ));
            emit_refusal(
                &WithdrawFailure {
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
    // deadline = 0 here because the withdraw deadline was derived from the
    // block timestamp inside the tokio runtime; store 0 as audit metadata
    // since the on-chain paymentId formula excludes the deadline anyway.
    if let Err(e) = replay.insert_op(
        crate::replay_cache::OP_WITHDRAW,
        cfg.chain_id,
        gateway_addr,
        agent_address,
        order_id,
        shares,
        idempotency_key,
        0,
        &tx_hash_hex,
    ) {
        log::warn!("rmpc withdraw: replay cache insert failed (non-fatal): {e}");
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
            // entry. The tx may still land; the operator must inspect the
            // original tx_hash before re-submitting.
            record_audit(&audit.build(AuditDecision::Refused, Some(e.name().to_string())));
            emit_refusal(
                &WithdrawFailure {
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
        // Confirmed on-chain failure (status == 0): remove the optimistic
        // replay-cache entry so the operator can retry with the same paymentId.
        withdraw_finalize_on_failure(
            &replay,
            cfg.chain_id,
            gateway_addr,
            agent_address,
            order_id,
            shares,
            idempotency_key,
        );
        let err = RmpcError::ErrTxReverted {
            tx_hash: tx_hash_hex.clone(),
        };
        record_audit(&audit.build(AuditDecision::Reverted, Some("ErrTxReverted".to_string())));
        emit_refusal(
            &WithdrawFailure {
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

    // -- Decode AgentWithdrawal log ----------------------------------------
    let topic0 = RobotMoneyGateway::AgentWithdrawal::SIGNATURE_HASH;
    let log = receipt
        .inner
        .logs()
        .iter()
        .find(|l| l.address() == gateway_addr && l.topics().first() == Some(&topic0));
    let log = match log {
        Some(l) => l,
        None => {
            let err = RmpcError::ErrAgentWithdrawLogMissing {
                tx_hash: tx_hash_hex.clone(),
            };
            record_audit(&audit.build(
                AuditDecision::Refused,
                Some("ErrAgentWithdrawLogMissing".to_string()),
            ));
            emit_refusal(
                &WithdrawFailure {
                    status: "refused",
                    error: "ErrAgentWithdrawLogMissing".to_string(),
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
    };
    let log_data = LogData::new_unchecked(log.topics().to_vec(), log.data().data.clone());
    let decoded = match RobotMoneyGateway::AgentWithdrawal::decode_log_data(&log_data, true) {
        Ok(d) => d,
        Err(e) => {
            log::error!("rmpc withdraw: failed to decode AgentWithdrawal log: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let block_number = receipt.block_number.unwrap_or(0);
    audit.payment_id = Some(format!("{:#x}", decoded.paymentId));
    record_audit(&audit.build(AuditDecision::Signed, None));
    let out = WithdrawOutput {
        status: "success",
        payment_id: format!("{:#x}", decoded.paymentId),
        order_id: format!("{:#x}", decoded.orderId),
        agent: format!("{:#x}", decoded.agent),
        asset_recipient: format!("{:#x}", decoded.assetRecipient),
        source_vault: format!("{:#x}", decoded.sourceVault),
        shares: decoded.shares.to_string(),
        assets_out: decoded.assetsOut.to_string(),
        block_number,
        tx_hash: tx_hash_hex,
        gas_used: receipt.gas_used.to_string(),
        effective_gas_price: receipt.effective_gas_price.to_string(),
    };
    emit(&out, args.pretty);
    EXIT_OK
}

/// AZ-RPC-2 finalize-on-confirmed-failure: remove the optimistic withdraw
/// replay-cache entry when the on-chain receipt shows status == 0. Only
/// call this on a confirmed revert, never on a receipt-wait timeout.
#[allow(clippy::too_many_arguments)]
fn withdraw_finalize_on_failure(
    replay: &crate::replay_cache::ReplayCache,
    chain_id: u64,
    gateway: Address,
    agent: Address,
    order_id: B256,
    shares: U256,
    idempotency_key: B256,
) {
    if let Err(e) = replay.remove_op(
        crate::replay_cache::OP_WITHDRAW,
        chain_id,
        gateway,
        agent,
        order_id,
        shares,
        idempotency_key,
    ) {
        log::warn!(
            "rmpc withdraw: replay cache finalize-on-failure remove failed (non-fatal): {e}"
        );
    }
}

/// Withdraw-specific vault preflight checks:
/// 1. vault.paused() == false
/// 2. vault.allowance(agent, gateway) >= shares
/// 3. vault.balanceOf(agent) >= shares
///
/// `pub(crate)` so the router-withdraw path (RPC-7) can run the identical
/// share allowance/balance check per redeem leg instead of hard-coding zero.
pub(crate) async fn withdraw_vault_preflight(
    rpc: &FailoverRpcClient,
    source_vault: Address,
    gateway: Address,
    agent: Address,
    shares: U256,
) -> Result<(), RmpcError> {
    // 1. vault paused
    let paused = call_vault_paused(rpc, source_vault).await?;
    if paused {
        return Err(RmpcError::ErrVaultPaused);
    }

    // 2. vault share allowance(agent, gateway) >= shares
    let allowance = call_erc20_allowance(rpc, source_vault, agent, gateway).await?;
    if allowance < shares {
        return Err(RmpcError::ErrShareAllowanceInsufficient);
    }

    // 3. vault share balance >= shares
    let balance = call_erc20_balance_of(rpc, source_vault, agent).await?;
    if balance < shares {
        return Err(RmpcError::ErrShareBalanceInsufficient);
    }

    Ok(())
}

async fn call_vault_paused(rpc: &FailoverRpcClient, vault: Address) -> Result<bool, RmpcError> {
    let data = MockVault::pausedCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: vault,
                from: None,
                data: data.into(),
            },
            None,
        )
        .await?;
    let decoded = MockVault::pausedCall::abi_decode_returns(&out, true)
        .map_err(|e| RmpcError::ErrRpcDecode(format!("vault.paused() decode: {e}")))?;
    Ok(decoded._0)
}

async fn call_erc20_allowance(
    rpc: &FailoverRpcClient,
    token: Address,
    owner: Address,
    spender: Address,
) -> Result<U256, RmpcError> {
    let data = Erc20::allowanceCall { owner, spender }.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: token,
                from: None,
                data: data.into(),
            },
            None,
        )
        .await?;
    let decoded = Erc20::allowanceCall::abi_decode_returns(&out, true)
        .map_err(|e| RmpcError::ErrRpcDecode(format!("allowance decode: {e}")))?;
    Ok(decoded._0)
}

async fn call_erc20_balance_of(
    rpc: &FailoverRpcClient,
    token: Address,
    who: Address,
) -> Result<U256, RmpcError> {
    let data = Erc20::balanceOfCall { account: who }.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: token,
                from: None,
                data: data.into(),
            },
            None,
        )
        .await?;
    let decoded = Erc20::balanceOfCall::abi_decode_returns(&out, true)
        .map_err(|e| RmpcError::ErrRpcDecode(format!("balanceOf decode: {e}")))?;
    Ok(decoded._0)
}

fn emit<T: serde::Serialize>(out: &T, pretty: bool) {
    let json = if pretty {
        serde_json::to_string_pretty(out)
    } else {
        serde_json::to_string(out)
    }
    .expect("withdraw output serialises");
    println!("{json}");
}

fn emit_refusal(out: &WithdrawFailure, pretty: bool) {
    emit(out, pretty);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, hex as ahex, keccak256, Address, U256};
    use alloy_sol_types::SolCall;
    use mockito::Matcher;
    use serde_json::json;

    const SIGNER: Address = address!("00000000000000000000000000000000000000aa");
    const GATEWAY: Address = address!("0000000000000000000000000000000000000b00");
    const VAULT: Address = address!("0000000000000000000000000000000000000d00");

    fn enc_bool(v: bool) -> String {
        let mut w = [0u8; 32];
        w[31] = if v { 1 } else { 0 };
        format!("0x{}", ahex::encode(w))
    }

    fn enc_u256(v: U256) -> String {
        format!("0x{}", ahex::encode(v.to_be_bytes::<32>()))
    }

    fn jrpc_result(s: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{s}"}}"#)
    }

    fn match_eth_call_selector(selector: &str) -> Matcher {
        let prefix = selector.to_string();
        Matcher::AllOf(vec![
            Matcher::PartialJson(json!({"method": "eth_call"})),
            Matcher::Regex(format!(r#""data":"{prefix}"#)),
        ])
    }

    fn selector_hex<C: SolCall>() -> String {
        format!("0x{}", ahex::encode(C::SELECTOR))
    }

    /// Install mocks for the three vault preflight calls.
    async fn install_vault_mocks(
        server: &mut mockito::ServerGuard,
        paused: bool,
        allowance: U256,
        balance: U256,
    ) {
        // vault.paused()
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
        // vault.allowance(agent, gateway)
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
        // vault.balanceOf(agent)
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

    #[tokio::test]
    async fn vault_paused_refuses() {
        let mut server = mockito::Server::new_async().await;
        // paused = true, ample allowance + balance
        install_vault_mocks(
            &mut server,
            true,
            U256::from(u128::MAX),
            U256::from(u128::MAX),
        )
        .await;
        let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();
        let err = withdraw_vault_preflight(&rpc, VAULT, GATEWAY, SIGNER, U256::from(100u64))
            .await
            .unwrap_err();
        assert!(matches!(err, RmpcError::ErrVaultPaused), "got {err:?}");
    }

    #[tokio::test]
    async fn vault_allowance_insufficient_refuses() {
        let mut server = mockito::Server::new_async().await;
        // paused = false, allowance too low, balance ample
        install_vault_mocks(&mut server, false, U256::from(1u64), U256::from(u128::MAX)).await;
        let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();
        let err = withdraw_vault_preflight(&rpc, VAULT, GATEWAY, SIGNER, U256::from(1_000u64))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RmpcError::ErrShareAllowanceInsufficient),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn vault_balance_insufficient_refuses() {
        let mut server = mockito::Server::new_async().await;
        // paused = false, ample allowance, balance too low
        install_vault_mocks(&mut server, false, U256::from(u128::MAX), U256::from(1u64)).await;
        let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();
        let err = withdraw_vault_preflight(&rpc, VAULT, GATEWAY, SIGNER, U256::from(1_000u64))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RmpcError::ErrShareBalanceInsufficient),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn vault_preflight_happy_path() {
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
    fn withdraw_selector_matches_canonical_signature() {
        let canonical = "withdraw(bytes32,uint256,address,uint64,bytes32)";
        let expected = &keccak256(canonical.as_bytes())[..4];
        let actual = RobotMoneyGateway::withdrawCall::SELECTOR;
        assert_eq!(&actual, expected, "withdraw selector drift");
    }

    #[test]
    fn agent_withdraw_event_topic0_matches() {
        // Canonical signature from contracts/gateway/RobotMoneyGateway.sol (AgentWithdrawal).
        // Field order: paymentId, orderId, agent(indexed), sourceVault, shares, assetsOut,
        // assetRecipient, windowId — matches the Foundry artifact ABI.
        let canonical =
            b"AgentWithdrawal(bytes32,bytes32,address,address,uint256,uint256,address,uint64)";
        let expected = keccak256(canonical);
        let actual = RobotMoneyGateway::AgentWithdrawal::SIGNATURE_HASH;
        assert_eq!(actual, expected, "AgentWithdrawal topic0 drift");
    }
}
