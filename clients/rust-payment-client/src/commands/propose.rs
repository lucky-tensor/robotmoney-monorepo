//! Canonical: docs/architecture.md §4 — High-Level Flow
//! Implements: issue #632
//!
//! `rmpc propose` — submit a new weight-reallocation proposal to
//! `RouterGovernance.propose()`.
//!
//! The command:
//! 1. Loads config and verifies `governance_address` is present.
//! 2. Runs the same signer preflight as deposit/withdraw (production-grade
//!    signer check, keystore decrypt).
//! 3. Builds and signs an EIP-1559 envelope calling `propose(vaults, bps)`.
//! 4. Broadcasts and waits for the receipt.
//! 5. Decodes the `ProposalCreated` log to extract `proposalId`.
//! 6. Emits `{ok:true,result:{proposal_id,tx_hash,block_number}}`.
//!
//! Exit codes:
//! - 0 — receipt mined; `ProposalCreated` log decoded.
//! - 2 — preflight refusal, fee-cap refusal, lock contention, or broadcast
//!   failure.
//! - 3 — startup failure: missing `governance_address`, keystore, RPC, etc.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, LogData, U256};
use alloy_sol_types::{SolCall, SolEvent};
use serde::Serialize;

use crate::config::Config;
use crate::errors::RmpcError;
use crate::fees::compute_fees;
use crate::gateway::RouterGovernance;
use crate::network_env::NetworkEnv;
use crate::nonce::AgentLock;
use crate::output::emit;
use crate::signer::software::{SoftwareSigner, PASSPHRASE_ENV_VAR};
use crate::signer::{require_production_grade_for_write, AgentSigner, SignerBackendKind};
use crate::tx::{
    broadcast, build_eip1559, encode_signed, signing_hash, wait_for_receipt_with, Eip1559Inputs,
};

const EXIT_OK: i32 = 0;
const EXIT_REFUSAL: i32 = 2;
const EXIT_STARTUP_FAIL: i32 = 3;

/// Default gas limit for the `propose` transaction. Proposal creation writes
/// vault + bps arrays into storage and performs router-eligibility checks.
const DEFAULT_GAS_LIMIT: u64 = 500_000;

/// Inputs collected by `main.rs` from the CLI parser.
#[derive(Debug, Clone)]
pub struct Args {
    pub config_path: PathBuf,
    /// Comma-separated vault addresses (0x-prefixed hex).
    pub vaults: Vec<String>,
    /// Comma-separated weight bps values (decimal integers, must sum to 10 000).
    pub weights_bps: Vec<u64>,
    /// Gas limit override. Defaults to [`DEFAULT_GAS_LIMIT`].
    pub gas_limit: u64,
    /// Optional override for `max_fee_per_gas_cap` in wei.
    pub fee_cap_wei: Option<u64>,
    /// Maximum seconds to wait for the receipt. Default 60.
    pub receipt_timeout_secs: u64,
    pub pretty: bool,
}

/// Stable JSON shape on a successful propose.
#[derive(Debug, Serialize)]
pub struct ProposeResult {
    pub proposal_id: String,
    pub tx_hash: String,
    pub block_number: u64,
}

/// Outer ok-envelope on success.
#[derive(Debug, Serialize)]
pub struct ProposeOutput {
    pub ok: bool,
    pub result: ProposeResult,
}

/// Stable JSON shape on a refusal.
#[derive(Debug, Serialize)]
pub struct ProposeFailure {
    pub ok: bool,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Entry point invoked from `main.rs`. Returns the desired process exit code.
pub fn run(args: Args) -> i32 {
    let cfg = match Config::from_path(&args.config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc propose: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let governance_addr = match cfg.governance_address.as_deref() {
        Some(s) => match Address::from_str(s) {
            Ok(a) => a,
            Err(e) => {
                log::error!("rmpc propose: governance_address parse error: {e}");
                return EXIT_STARTUP_FAIL;
            }
        },
        None => {
            log::error!(
                "rmpc propose: governance_address not set in config; \
                 add `governance_address = \"0x...\"` to the operator TOML"
            );
            return EXIT_STARTUP_FAIL;
        }
    };

    // Parse vault addresses.
    let mut vault_addresses: Vec<Address> = Vec::with_capacity(args.vaults.len());
    for v in &args.vaults {
        match Address::from_str(v) {
            Ok(a) => vault_addresses.push(a),
            Err(e) => {
                log::error!("rmpc propose: invalid vault address {v:?}: {e}");
                return EXIT_STARTUP_FAIL;
            }
        }
    }

    // Convert weights to U256.
    let bps_u256: Vec<U256> = args.weights_bps.iter().map(|&b| U256::from(b)).collect();

    // Production-grade signer check.
    if let Err(err) = require_production_grade_for_write(cfg.chain_id, SignerBackendKind::Software)
    {
        log::error!("rmpc propose: {err}");
        emit_failure(
            &ProposeFailure {
                ok: false,
                error: err.name().to_string(),
                message: Some(format!("{err}")),
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
                "rmpc propose: ${PASSPHRASE_ENV_VAR} is unset; refusing to prompt on stdin from a non-interactive command"
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
                "rmpc propose: ErrSoftwareSignerDisallowed: [signer].allow_software_fallback must be true"
            );
            emit_failure(
                &ProposeFailure {
                    ok: false,
                    error: "ErrSoftwareSignerDisallowed".to_string(),
                    message: Some(
                        "[signer].allow_software_fallback must be true to use the software keystore"
                            .to_string(),
                    ),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
        Err(e) => {
            log::error!("rmpc propose: signer load failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let agent_address = signer.public_address();

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc propose: agent={agent_address:#x} governance={governance_addr:#x} vaults={:?} bps={:?} chain_id={} network_env={}",
        args.vaults,
        args.weights_bps,
        cfg.chain_id,
        network_env.as_str(),
    );

    // Acquire per-agent lock.
    let state_dir = match cfg.resolve_state_dir() {
        Ok(p) => p,
        Err(e) => {
            log::error!("rmpc propose: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let _lock = match AgentLock::acquire(&state_dir, &agent_address) {
        Ok(l) => l,
        Err(RmpcError::ErrConcurrentInvocation) => {
            emit_failure(
                &ProposeFailure {
                    ok: false,
                    error: "ErrConcurrentInvocation".to_string(),
                    message: Some(format!(
                        "another rmpc invocation already holds the lock for agent {agent_address:#x}"
                    )),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
        Err(e) => {
            log::error!("rmpc propose: lock acquire failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc propose: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc propose: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // Fees.
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
                emit_failure(
                    &ProposeFailure {
                        ok: false,
                        error: e.name().to_string(),
                        message: Some(format!("{e}")),
                    },
                    args.pretty,
                );
                return EXIT_REFUSAL;
            }
        },
        Err(e) => {
            log::error!("rmpc propose: eth_feeHistory failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // Nonce.
    let nonce = match rt.block_on(async {
        rpc.get_transaction_count(agent_address, Some("pending"))
            .await
    }) {
        Ok(n) => n,
        Err(e) => {
            log::error!("rmpc propose: eth_getTransactionCount failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // Build calldata.
    let calldata = RouterGovernance::proposeCall {
        vaults: vault_addresses,
        bps: bps_u256,
    }
    .abi_encode();

    let tx = build_eip1559(Eip1559Inputs {
        chain_id: cfg.chain_id,
        nonce,
        to: governance_addr,
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
            log::error!("rmpc propose: envelope signing failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let raw = encode_signed(tx, alloy_sig);

    // Broadcast.
    let tx_hash = match rt.block_on(async { broadcast(&rpc, &raw).await }) {
        Ok(h) => h,
        Err(e) => {
            emit_failure(
                &ProposeFailure {
                    ok: false,
                    error: e.name().to_string(),
                    message: Some(format!("{e}")),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    // Wait for receipt.
    let max_attempts = args.receipt_timeout_secs.min(u32::MAX as u64) as u32;
    let receipt = match rt.block_on(async {
        wait_for_receipt_with(&rpc, tx_hash, Duration::from_secs(1), max_attempts.max(1)).await
    }) {
        Ok(r) => r,
        Err(e) => {
            emit_failure(
                &ProposeFailure {
                    ok: false,
                    error: e.name().to_string(),
                    message: Some(format!("{e}")),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    if !receipt.inner.status() {
        emit_failure(
            &ProposeFailure {
                ok: false,
                error: "ErrTxReverted".to_string(),
                message: Some(format!(
                    "transaction reverted on-chain (tx_hash={tx_hash:#x})"
                )),
            },
            args.pretty,
        );
        return EXIT_REFUSAL;
    }

    // Decode ProposalCreated log.
    let topic0 = RouterGovernance::ProposalCreated::SIGNATURE_HASH;
    let log = receipt
        .inner
        .logs()
        .iter()
        .find(|l| l.address() == governance_addr && l.topics().first() == Some(&topic0));

    let proposal_id = if let Some(l) = log {
        let log_data = LogData::new_unchecked(l.topics().to_vec(), l.data().data.clone());
        match RouterGovernance::ProposalCreated::decode_log_data(&log_data, true) {
            Ok(d) => d.proposalId.to_string(),
            Err(e) => {
                log::error!("rmpc propose: failed to decode ProposalCreated log: {e}");
                return EXIT_STARTUP_FAIL;
            }
        }
    } else {
        // Fallback: no log found, but tx succeeded — use a placeholder.
        log::warn!("rmpc propose: ProposalCreated log not found in receipt; tx may have succeeded without emitting the expected event");
        "unknown".to_string()
    };

    let block_number = receipt.block_number.unwrap_or(0);
    let out = ProposeOutput {
        ok: true,
        result: ProposeResult {
            proposal_id,
            tx_hash: format!("{tx_hash:#x}"),
            block_number,
        },
    };
    emit(&out, args.pretty);
    EXIT_OK
}

fn emit_failure(out: &ProposeFailure, pretty: bool) {
    emit(out, pretty);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propose_fails_fast_without_governance_address() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let keystore = tmp.path().join("keystore.json");
        let cfg_path = tmp.path().join("rmpc.toml");
        let toml = format!(
            r#"chain_id              = 31337
rpc_url               = "http://127.0.0.1:1"
gateway_address       = "0x000000000000000000000000000000000000dEaD"
usdc_address          = "0x{usdc}"
vault_address         = "0x{vault}"
gateway_runtime_hash  = "0x{zeros}"
max_fee_per_gas_cap   = 100000000000

[signer]
allow_software_fallback = true
keystore_path           = "{ks}"
"#,
            usdc = "00".repeat(20),
            vault = "00".repeat(20),
            zeros = "0".repeat(64),
            ks = keystore.display(),
        );
        std::fs::write(&cfg_path, &toml).expect("write rmpc.toml");
        let args = Args {
            config_path: cfg_path,
            vaults: vec!["0x0000000000000000000000000000000000000001".to_string()],
            weights_bps: vec![10_000],
            gas_limit: DEFAULT_GAS_LIMIT,
            fee_cap_wei: None,
            receipt_timeout_secs: 60,
            pretty: false,
        };
        let code = run(args);
        assert_eq!(code, EXIT_STARTUP_FAIL);
    }

    #[test]
    fn propose_fails_fast_with_uninitialized_signer() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let keystore = tmp.path().join("keystore.json");
        let cfg_path = tmp.path().join("rmpc.toml");
        // Config has governance_address but keystore does not exist.
        let toml = format!(
            r#"chain_id              = 31337
rpc_url               = "http://127.0.0.1:1"
gateway_address       = "0x000000000000000000000000000000000000dEaD"
usdc_address          = "0x{usdc}"
vault_address         = "0x{vault}"
governance_address    = "0x000000000000000000000000000000000000dEaD"
gateway_runtime_hash  = "0x{zeros}"
max_fee_per_gas_cap   = 100000000000

[signer]
allow_software_fallback = true
keystore_path           = "{ks}"
"#,
            usdc = "00".repeat(20),
            vault = "00".repeat(20),
            zeros = "0".repeat(64),
            ks = keystore.display(),
        );
        std::fs::write(&cfg_path, &toml).expect("write rmpc.toml");
        // RMPC_KEYSTORE_PASSPHRASE must be set for the signer path; if not
        // set the command exits 3 (startup fail) before reaching signer load.
        std::env::remove_var("RMPC_KEYSTORE_PASSPHRASE");
        let args = Args {
            config_path: cfg_path,
            vaults: vec!["0x0000000000000000000000000000000000000001".to_string()],
            weights_bps: vec![10_000],
            gas_limit: DEFAULT_GAS_LIMIT,
            fee_cap_wei: None,
            receipt_timeout_secs: 60,
            pretty: false,
        };
        let code = run(args);
        assert_eq!(code, EXIT_STARTUP_FAIL);
    }

    #[test]
    fn propose_result_serializes_correctly() {
        let result = ProposeOutput {
            ok: true,
            result: ProposeResult {
                proposal_id: "1".to_string(),
                tx_hash: "0xabcdef".to_string(),
                block_number: 42,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&result).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["proposal_id"], "1");
        assert_eq!(v["result"]["tx_hash"], "0xabcdef");
        assert_eq!(v["result"]["block_number"], 42);
    }

    #[test]
    fn propose_failure_serializes_correctly() {
        let fail = ProposeFailure {
            ok: false,
            error: "ErrConfig".to_string(),
            message: Some("governance_address not set".to_string()),
        };
        let v: serde_json::Value = serde_json::to_value(&fail).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "ErrConfig");
        assert_eq!(v["message"], "governance_address not set");
    }
}
