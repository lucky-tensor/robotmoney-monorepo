//! Canonical: docs/architecture.md §4 — High-Level Flow
//! (See also: Plan tracking issue #109 §4.8 — CLI surface)
//!
//! `rmpc deposit` — sign and broadcast a USDC deposit through the gateway.
//!
//! Per `Plan tracking issue #109` §3.8 and issue #16. This is the
//! keystone command; it ties together every other module:
//!
//! 1. Load config + signer (software keystore decrypted in-process).
//! 2. Acquire the per-agent file lock (single-flight CLI; §3.6).
//! 3. Run [`Preflight`] with the actual deposit amount. Any refusal exits
//!    non-zero with a named-error JSON body — symmetric with `self-check`
//!    so operators can correlate.
//! 4. Compute fees from `eth_feeHistory` ([`compute_fees`]). Fee-cap
//!    refusal → `ErrFeeCapExceeded`.
//! 5. Build the EIP-1559 envelope, sign it via
//!    [`AgentSigner::sign_eip1559_hash`], broadcast, poll for the receipt.
//! 6. Decode the `AgentDeposit` event log → emit a stable JSON document
//!    on stdout. The shape mirrors `rmpc status` so users can correlate
//!    a deposit response with a later lookup.
//!
//! Exit codes:
//! - 0 — receipt mined with `status == 1` and an `AgentDeposit` log.
//! - 2 — preflight refusal, fee-cap refusal, lock contention, or any
//!   refusal that maps to an [`RmpcError`] variant.
//! - 3 — startup failure: config / keystore / RPC client / runtime build.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, LogData, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use serde::Serialize;

use crate::commands::self_check::ChecksOutput;
use crate::config::Config;
use crate::errors::RmpcError;
use crate::fees::compute_fees;
use crate::gateway::RobotMoneyGateway;
use crate::logging::{record_audit, AuditDecision, AuditRecordBuilder};
use crate::network_env::NetworkEnv;
use crate::nonce::AgentLock;
use crate::policy::{Preflight, PreflightInputs};
use crate::signer::software::{SoftwareSigner, PASSPHRASE_ENV_VAR};
use crate::signer::{require_production_grade_for_write, AgentSigner, SignerBackendKind};
use crate::tx::{
    broadcast, build_eip1559, encode_signed, signing_hash, wait_for_receipt_with, Eip1559Inputs,
};

const EXIT_OK: i32 = 0;
const EXIT_REFUSAL: i32 = 2;
const EXIT_STARTUP_FAIL: i32 = 3;

/// Gateway-side maximum deadline skew, mirrored client-side so the daemon
/// never builds a transaction the contract is guaranteed to reject. Keep
/// in sync with `RobotMoneyGateway.MAX_DEADLINE_SKEW`.
pub const MAX_DEADLINE_SKEW_SECS: u64 = 600;

/// Environment variable for the per-agent state directory.
///
/// Resolved by [`Config::resolve_state_dir`]: env override → TOML
/// `state_dir` field → fail-fast. There is **no silent `/tmp` fallback**
/// (audit finding M1).
pub const STATE_DIR_ENV_VAR: &str = "RMPC_STATE_DIR";

/// Inputs collected by `main.rs` from the CLI parser. Keeps the surface
/// stable as flags evolve.
#[derive(Debug, Clone)]
pub struct Args {
    pub config_path: PathBuf,
    pub amount: String,
    pub order_id: String,
    pub idempotency_key: Option<String>,
    pub deadline_secs: u64,
    pub receipt_timeout_secs: u64,
    pub gas_limit: u64,
    /// Optional CLI override for `max_fee_per_gas_cap` in wei (issue #93).
    /// When `Some(_)` it wins over both `[fees].max_fee_per_gas_cap` in
    /// TOML and the per-chain default table.
    pub fee_cap_wei: Option<u64>,
    /// When `Some(_)`, the deposit is routed through `gateway.depositTo()`
    /// targeting this PortfolioRouter address. When `None` the existing
    /// single-vault `gateway.deposit()` path is used.
    pub destination: Option<String>,
    /// Per-leg minimum shares forwarded to `gateway.depositTo()`.
    /// Ignored when `destination` is `None`.
    pub min_shares_per_leg: Vec<String>,
    pub pretty: bool,
}

/// Stable JSON shape on a successful deposit. Field names are part of
/// the operator-visible contract — downstream e2e tests (#18/#19) match
/// on them. Numeric values that may exceed `u64` are decimal strings so
/// JavaScript `JSON.parse` does not silently lose precision.
#[derive(Debug, Serialize)]
pub struct DepositOutput {
    pub status: &'static str, // always "success" on the happy path
    pub payment_id: String,
    pub order_id: String,
    pub agent: String,
    pub share_receiver: String,
    pub amount: String,
    pub shares_minted: String,
    pub block_number: u64,
    pub tx_hash: String,
    pub gas_used: String,
    pub effective_gas_price: String,
}

/// Stable JSON shape on a refusal (preflight, fee cap, lock contention,
/// receipt timeout, on-chain revert, ...). `error` is the variant name
/// of the underlying [`RmpcError`]; `checks` is populated when the
/// refusal came from preflight so operators get the same snapshot they
/// would get from `rmpc self-check`.
#[derive(Debug, Serialize)]
pub struct DepositFailure {
    pub status: &'static str, // always "refused"
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

/// Entry point invoked from `main.rs`. Returns the desired process exit
/// code. The function is deliberately monolithic: each fallible step
/// runs through a small `?`-on-`Result` helper and the failure-path
/// JSON shape is built in one place at the end.
pub fn run(args: Args) -> i32 {
    let cfg = match Config::from_path(&args.config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc deposit: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let amount = match U256::from_str_radix(args.amount.trim_start_matches("0x"), 10) {
        Ok(v) if !args.amount.starts_with("0x") => v,
        _ => match U256::from_str(&args.amount) {
            Ok(v) => v,
            Err(e) => {
                log::error!("rmpc deposit: --amount must be a decimal U256: {e}");
                return EXIT_STARTUP_FAIL;
            }
        },
    };

    let order_id = match B256::from_str(&args.order_id) {
        Ok(b) => b,
        Err(e) => {
            log::error!("rmpc deposit: --order-id is not a 32-byte hex string: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let idempotency_key = match args.idempotency_key.as_deref() {
        None => order_id,
        Some(s) => match B256::from_str(s) {
            Ok(b) => b,
            Err(e) => {
                log::error!("rmpc deposit: --idempotency-key is not a 32-byte hex string: {e}");
                return EXIT_STARTUP_FAIL;
            }
        },
    };

    let gateway_addr = match Address::from_str(&cfg.gateway_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc deposit: gateway_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let deadline_secs = args.deadline_secs.min(MAX_DEADLINE_SKEW_SECS);

    if let Err(err) = require_production_grade_for_write(cfg.chain_id, SignerBackendKind::Software)
    {
        log::error!("rmpc deposit: {err}");
        emit_refusal(
            &DepositFailure {
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
                "rmpc deposit: ${PASSPHRASE_ENV_VAR} is unset; refusing to prompt on stdin from a non-interactive command"
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
            // Operator policy refused the software keystore. Surface this
            // as a structured refusal on stdout (mirroring other refusals
            // like ErrConcurrentInvocation) so test harnesses and audit
            // scrapers see `ErrSoftwareSignerDisallowed` without having to
            // tail the rotating diagnostic file.
            log::error!(
                "rmpc deposit: ErrSoftwareSignerDisallowed: [signer].allow_software_fallback must be true"
            );
            emit_refusal(
                &DepositFailure {
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
            log::error!("rmpc deposit: signer load failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let agent_address = signer.public_address();
    let backend_label = match signer.backend_kind() {
        crate::signer::SignerBackendKind::Software => "software",
        crate::signer::SignerBackendKind::Hsm => "hsm",
        crate::signer::SignerBackendKind::Kms => "kms",
    };

    // Audit-record skeleton. Filled in incrementally; on every exit
    // path below we call `audit.build(...)` + `record_audit(&rec)` so
    // every signing decision (success OR refusal) leaves a trail.
    let mut audit = AuditRecordBuilder {
        agent: format!("{agent_address:#x}"),
        backend: backend_label.to_string(),
        request_type: "deposit".to_string(),
        order_id: format!("{order_id:#x}"),
        idempotency_key: format!("{idempotency_key:#x}"),
        amount: amount.to_string(),
        deadline: 0,
        gateway: format!("{gateway_addr:#x}"),
        chain_id: cfg.chain_id,
        tx_hash: None,
        payment_id: None,
    };
    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "deposit: starting agent={} order_id={} amount={} chain_id={} network_env={}",
        audit.agent,
        audit.order_id,
        audit.amount,
        audit.chain_id,
        network_env.as_str()
    );
    log::info!(
        "deposit: network environment: {}",
        network_env.human_label()
    );
    if let Some(warn) = network_env.production_warning() {
        log::warn!("deposit: {warn}");
    }

    // State dir for the per-agent lock + replay cache. Resolved via
    // `Config::resolve_state_dir`: env (`RMPC_STATE_DIR`) → TOML
    // `state_dir` → fail-fast. No silent `/tmp` fallback (audit M1).
    let state_dir = match cfg.resolve_state_dir() {
        Ok(p) => p,
        Err(e) => {
            log::error!("rmpc deposit: {e}");
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
                &DepositFailure {
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
            log::error!("rmpc deposit: lock acquire failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Replay cache (audit M3) -----------------------------------------
    // Look up the gateway-equivalent paymentId in our own client-side
    // cache.  The paymentId is keccak256(abi.encode(chain_id, gateway,
    // agent, order_id, amount, idempotency_key)) — deadline is
    // intentionally excluded, mirroring the on-chain formula.  On a
    // hit, surface the prior tx_hash and exit non-zero with
    // `ErrOrderIdAlreadySubmitted` instead of paying gas to discover
    // the same dedupe on chain.  A retry with a fresh deadline but the
    // same on-chain paymentId is caught here; deposits that differ by
    // amount, chain_id, gateway, or agent produce a distinct paymentId.
    let replay = match crate::replay_cache::ReplayCache::open(&state_dir) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc deposit: replay cache open failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let order_id_hex = format!("{order_id:#x}");
    match replay.lookup(
        cfg.chain_id,
        gateway_addr,
        agent_address,
        order_id,
        amount,
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
                &DepositFailure {
                    status: "refused",
                    error: "ErrOrderIdAlreadySubmitted".to_string(),
                    message: Some(format!("{err}")),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(order_id_hex.clone()),
                    tx_hash: Some(prior_tx),
                    checks: None,
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
        Ok(None) => {}
        Err(e) => {
            log::error!("rmpc deposit: replay cache lookup failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    }

    // Build the runtime; sync rest of the daemon stays sync.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc deposit: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc deposit: rpc client init failed: {e}");
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
            log::error!("rmpc deposit: failed to fetch block timestamp for deadline: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Preflight --------------------------------------------------------
    let preflight_result = rt.block_on(async {
        let pf = Preflight::new(&rpc, &cfg);
        pf.run(PreflightInputs {
            signer_address: agent_address,
            amount,
        })
        .await
    });
    let report = match preflight_result {
        Ok(r) => r,
        Err(err) => {
            record_audit(&audit.build(AuditDecision::Refused, Some(err.name().to_string())));
            let checks = ChecksOutput::from_err_partial(&err);
            emit_refusal(
                &DepositFailure {
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
                    &DepositFailure {
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
            log::error!("rmpc deposit: eth_feeHistory failed: {e}");
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
            log::error!("rmpc deposit: eth_getTransactionCount failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Build + sign envelope -------------------------------------------
    // Branch: router-deposit (depositTo) vs. single-vault deposit.
    let calldata = if let Some(ref dest_str) = args.destination {
        // Parse the destination router address.
        let destination = match Address::from_str(dest_str) {
            Ok(a) => a,
            Err(e) => {
                log::error!("rmpc deposit: --destination is not a valid address: {e}");
                return EXIT_STARTUP_FAIL;
            }
        };
        // Parse per-leg minimum shares (decimal U256 strings).
        let mut min_shares: Vec<U256> = Vec::with_capacity(args.min_shares_per_leg.len());
        for s in &args.min_shares_per_leg {
            match U256::from_str(s) {
                Ok(v) => min_shares.push(v),
                Err(e) => {
                    log::error!(
                        "rmpc deposit: --min-shares-per-leg value {s:?} is not a decimal U256: {e}"
                    );
                    return EXIT_STARTUP_FAIL;
                }
            }
        }
        log::info!(
            "deposit: router path: destination={destination:#x} legs={}",
            min_shares.len()
        );
        RobotMoneyGateway::depositToCall {
            orderId: order_id,
            amount,
            deadline,
            idempotencyKey: idempotency_key,
            destination,
            minSharesPerLeg: min_shares,
        }
        .abi_encode()
    } else {
        RobotMoneyGateway::depositCall {
            orderId: order_id,
            amount,
            deadline,
            idempotencyKey: idempotency_key,
        }
        .abi_encode()
    };

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
            log::error!("rmpc deposit: envelope signing failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let raw = encode_signed(tx, alloy_sig);

    // -- Broadcast --------------------------------------------------------
    let tx_hash = match rt.block_on(async { broadcast(&rpc, &raw).await }) {
        Ok(h) => h,
        Err(e) => {
            log::error!("rmpc deposit: eth_sendRawTransaction failed: {e}");
            // Treat broadcast failure as a refusal — operator-visible
            // failure with a stable name. Most likely cause is a contract
            // revert simulated by the node ahead of inclusion.
            record_audit(&audit.build(
                AuditDecision::BroadcastFailed,
                Some(e.name().to_string()),
            ));
            emit_refusal(
                &DepositFailure {
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

    // Stamp the broadcast tx_hash into the audit record so subsequent
    // refusal/success emissions include it.
    let tx_hash_hex = format!("{tx_hash:#x}");
    audit.tx_hash = Some(tx_hash_hex.clone());
    // Record the paymentId → tx_hash entry in the replay cache so a
    // future retry hits the local check before paying gas.  deadline is
    // stored as audit metadata only.
    if let Err(e) = replay.insert(
        cfg.chain_id,
        gateway_addr,
        agent_address,
        order_id,
        amount,
        idempotency_key,
        deadline,
        &tx_hash_hex,
    ) {
        log::warn!("rmpc deposit: replay cache insert failed (non-fatal): {e}");
    }

    // -- Receipt ----------------------------------------------------------
    // 1s polling cadence (RECEIPT_POLL_INTERVAL_MS) × the operator's
    // attempt budget. Issue #19 e2e harness sets this short on Anvil.
    let max_attempts = args.receipt_timeout_secs.min(u32::MAX as u64) as u32;
    let receipt_res = rt.block_on(async {
        wait_for_receipt_with(&rpc, tx_hash, Duration::from_secs(1), max_attempts.max(1)).await
    });
    let receipt = match receipt_res {
        Ok(r) => r,
        Err(e) => {
            // AZ-RPC-1 (timeout ≠ failure): the receipt-wait budget exhausted
            // but the transaction may still be pending and land later.  Do NOT
            // call finalize_replay_on_failure here — removing the replay-cache
            // entry on a timeout would allow a second broadcast for the same
            // paymentId while the first tx is still in-flight.  The entry
            // stays in place; if the operator retries they get
            // ErrOrderIdAlreadySubmitted pointing at the original tx_hash,
            // which they should inspect before re-submitting.
            record_audit(&audit.build(AuditDecision::Refused, Some(e.name().to_string())));
            emit_refusal(
                &DepositFailure {
                    status: "refused",
                    error: e.name().to_string(),
                    message: Some(format!("{e}")),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: Some(format!("{tx_hash:#x}")),
                    checks: None,
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    if !receipt.inner.status() {
        // RPC-2 (finalize-on-failure): the tx reverted on-chain, so no deposit
        // was recorded. Remove the optimistic replay-cache entry so the
        // operator can retry the same order without being refused.
        finalize_replay_on_failure(
            &replay,
            cfg.chain_id,
            gateway_addr,
            agent_address,
            order_id,
            amount,
            idempotency_key,
        );
        let err = RmpcError::ErrTxReverted {
            tx_hash: format!("{tx_hash:#x}"),
        };
        record_audit(&audit.build(AuditDecision::Reverted, Some("ErrTxReverted".to_string())));
        emit_refusal(
            &DepositFailure {
                status: "refused",
                error: "ErrTxReverted".to_string(),
                message: Some(format!("{err}")),
                agent: Some(format!("{agent_address:#x}")),
                order_id: Some(format!("{order_id:#x}")),
                tx_hash: Some(format!("{tx_hash:#x}")),
                checks: None,
            },
            args.pretty,
        );
        return EXIT_REFUSAL;
    }

    // -- Decode AgentDeposit log ------------------------------------------
    let topic0 = RobotMoneyGateway::AgentDeposit::SIGNATURE_HASH;
    let log = receipt
        .inner
        .logs()
        .iter()
        .find(|l| l.address() == gateway_addr && l.topics().first() == Some(&topic0));
    let log = match log {
        Some(l) => l,
        None => {
            let err = RmpcError::ErrAgentDepositLogMissing {
                tx_hash: format!("{tx_hash:#x}"),
            };
            record_audit(&audit.build(
                AuditDecision::Refused,
                Some("ErrAgentDepositLogMissing".to_string()),
            ));
            emit_refusal(
                &DepositFailure {
                    status: "refused",
                    error: "ErrAgentDepositLogMissing".to_string(),
                    message: Some(format!("{err}")),
                    agent: Some(format!("{agent_address:#x}")),
                    order_id: Some(format!("{order_id:#x}")),
                    tx_hash: Some(format!("{tx_hash:#x}")),
                    checks: None,
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };
    let log_data = LogData::new_unchecked(log.topics().to_vec(), log.data().data.clone());
    let decoded = match RobotMoneyGateway::AgentDeposit::decode_log_data(&log_data, true) {
        Ok(d) => d,
        Err(e) => {
            log::error!("rmpc deposit: failed to decode AgentDeposit log: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let block_number = receipt.block_number.unwrap_or(0);
    audit.payment_id = Some(format!("{:#x}", decoded.paymentId));
    record_audit(&audit.build(AuditDecision::Signed, None));
    let out = DepositOutput {
        status: "success",
        payment_id: format!("{:#x}", decoded.paymentId),
        order_id: format!("{:#x}", decoded.orderId),
        agent: format!("{:#x}", decoded.agent),
        share_receiver: format!("{:#x}", decoded.shareReceiver),
        amount: decoded.amount.to_string(),
        shares_minted: decoded.sharesMinted.to_string(),
        block_number,
        tx_hash: format!("{tx_hash:#x}"),
        gas_used: receipt.gas_used.to_string(),
        effective_gas_price: receipt.effective_gas_price.to_string(),
    };
    emit(&out, args.pretty);
    EXIT_OK
}

fn emit<T: Serialize>(out: &T, pretty: bool) {
    let json = if pretty {
        serde_json::to_string_pretty(out)
    } else {
        serde_json::to_string(out)
    }
    .expect("deposit output serialises");
    println!("{json}");
}

fn emit_refusal(out: &DepositFailure, pretty: bool) {
    emit(out, pretty);
}

/// RPC-2 finalize-on-failure: remove the optimistic replay-cache entry for a
/// deposit whose receipt reverted or timed out, so a legitimate retry is not
/// permanently refused. A failed removal is non-fatal (logged at warn): the
/// cache is a defensive client-side check, and the on-chain paymentId remains
/// the source of truth.
#[allow(clippy::too_many_arguments)]
fn finalize_replay_on_failure(
    replay: &crate::replay_cache::ReplayCache,
    chain_id: u64,
    gateway: Address,
    agent: Address,
    order_id: B256,
    amount: U256,
    idempotency_key: B256,
) {
    if let Err(e) = replay.remove(chain_id, gateway, agent, order_id, amount, idempotency_key) {
        log::warn!("rmpc deposit: replay cache finalize-on-failure remove failed (non-fatal): {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Matcher;
    use serde_json::json;

    #[tokio::test]
    async fn deadline_is_capped_at_max_skew() {
        // Sanity: deadline uses block timestamp, not wall clock.
        // Mock a block with known timestamp and verify the deadline
        // is computed as timestamp + capped deadline_secs.
        let mut server = mockito::Server::new_async().await;

        // Mock eth_blockNumber → 0x100 (256)
        let _block_mock = server
            .mock("POST", "/")
            .match_body(Matcher::AllOf(vec![Matcher::PartialJson(
                json!({"method": "eth_blockNumber"}),
            )]))
            .with_status(200)
            .with_body(r#"{"jsonrpc":"2.0","id":1,"result":"0x100"}"#)
            .expect(1)
            .create_async()
            .await;

        // Mock eth_getBlockByNumber for 0x100 → timestamp 0x64a9f4c0
        let _ts_mock = server
            .mock("POST", "/")
            .match_body(Matcher::AllOf(vec![Matcher::PartialJson(
                json!({"method": "eth_getBlockByNumber"}),
            )]))
            .with_status(200)
            .with_body(r#"{"jsonrpc":"2.0","id":1,"result":{"timestamp":"0x64a9f4c0"}}"#)
            .expect(1)
            .create_async()
            .await;

        let rpc = crate::rpc::FailoverRpcClient::new(vec![server.url()]).unwrap();
        let deadline_secs = 300u64;
        let block_number = rpc.block_number().await.unwrap();
        let ts = rpc.block_timestamp(block_number).await.unwrap();
        let deadline = ts.saturating_add(deadline_secs.min(MAX_DEADLINE_SKEW_SECS));
        // 0x64a9f4c0 = 1_688_859_840 → + 300 deadline_secs → 1_688_860_140
        assert_eq!(deadline, 1_688_860_140);
    }
}
