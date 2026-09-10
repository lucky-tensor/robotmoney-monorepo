//! Canonical: docs/product/20260623-product-proposal-investment-committee-v0.md §3
//! Implements: issue #1044 — Investment Committee v0
//!
//! `rmpc committee` — register a committee agent or submit a signed
//! allocation vote through the `InvestmentCommitteePolicy` contract,
//! routed via `RobotMoneyGateway`.
//!
//! Subcommands:
//! - `register`     — ADMIN_ROLE: allowlist a new committee agent address.
//! - `vote-submit`  — COMMITTEE_AGENT_ROLE: submit a signed allocation vote.
//!
//! Both subcommands encode their calldata against the IC policy ABI and
//! broadcast the transaction directly to the IC policy contract. No
//! committee action bypasses the gateway (the contract itself enforces
//! `onlyGateway`; the transaction sender is the gateway AGENT).
//!
//! Exit codes:
//! - 0 — success.
//! - 2 — refusal (unauthorized, non-allowlisted agent, bad input).
//! - 3 — startup failure (missing config, keystore, RPC, etc.).

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::errors::RmpcError;
use crate::fees::{compute_fees, FeeBid};
use crate::gateway::InvestmentCommitteePolicy;
use crate::network_env::NetworkEnv;
use crate::nonce::AgentLock;
use crate::output::emit;
use crate::rpc::FailoverRpcClient;
use crate::signer::software::{SoftwareSigner, PASSPHRASE_ENV_VAR};
use crate::signer::{require_production_grade_for_write, AgentSigner, SignerBackendKind};
use crate::tx::{
    broadcast, build_eip1559, encode_signed, signing_hash, wait_for_receipt_with, Eip1559Inputs,
};

const EXIT_OK: i32 = 0;
const EXIT_REFUSAL: i32 = 2;
const EXIT_STARTUP_FAIL: i32 = 3;

/// Allocation stance supplied by the caller.
/// ABI encoding: Overweight=0, Neutral=1, Underweight=2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stance {
    Overweight,
    Neutral,
    Underweight,
}

impl Stance {
    /// Parse from a string; case-insensitive.
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "overweight" => Some(Stance::Overweight),
            "neutral" => Some(Stance::Neutral),
            "underweight" => Some(Stance::Underweight),
            _ => None,
        }
    }

    /// Wire string for JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Stance::Overweight => "overweight",
            Stance::Neutral => "neutral",
            Stance::Underweight => "underweight",
        }
    }

    /// ABI-encoded u8: Overweight=0, Neutral=1, Underweight=2.
    pub fn to_abi_u8(self) -> u8 {
        match self {
            Stance::Overweight => 0,
            Stance::Neutral => 1,
            Stance::Underweight => 2,
        }
    }
}

// ─── Args ────────────────────────────────────────────────────────────────────

/// Args for `rmpc committee register`.
#[derive(Debug, Clone)]
pub struct RegisterArgs {
    pub config_path: PathBuf,
    pub agent: String,
    pub agent_id: String,
    pub order_id: String,
    pub deadline_secs: u64,
    pub receipt_timeout_secs: u64,
    pub gas_limit: u64,
    pub fee_cap_wei: Option<u64>,
    pub pretty: bool,
}

/// Args for `rmpc committee vote-submit`.
#[derive(Debug, Clone)]
pub struct VoteSubmitArgs {
    pub config_path: PathBuf,
    pub vault: String,
    pub stance: Stance,
    pub target_weight_bps: u16,
    pub confidence: u8,
    pub rationale_uri: String,
    pub vote_json_hash: String,
    pub prompt_hash: String,
    pub inputs_digest: String,
    pub schema_version: String,
    pub timestamp: u64,
    pub order_id: String,
    pub deadline_secs: u64,
    pub receipt_timeout_secs: u64,
    pub gas_limit: u64,
    pub fee_cap_wei: Option<u64>,
    pub pretty: bool,
}

// ─── Output types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CommitteeOutput {
    pub ok: bool,
    pub action: String,
    pub tx_hash: String,
    pub block_number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vote_id: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct CommitteeFailure {
    pub ok: bool,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ─── Public entry points ─────────────────────────────────────────────────────

/// Run `rmpc committee register`. Returns the process exit code.
pub fn run_register(args: RegisterArgs) -> i32 {
    let cfg = match Config::from_path(&args.config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc committee register: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let ic_addr = match resolve_ic_address(&cfg, "register", args.pretty) {
        Ok(a) => a,
        Err(code) => return code,
    };

    let agent_addr = match parse_address(&args.agent, "rmpc committee register", "--agent") {
        Ok(a) => a,
        Err(code) => return code,
    };

    let signer = match load_signer(&cfg, "register", args.pretty) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let caller = signer.public_address();

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc committee register: caller={caller:#x} ic={ic_addr:#x} agent={agent_addr:#x} chain_id={} env={}",
        cfg.chain_id,
        network_env.as_str()
    );

    let state_dir = match cfg.resolve_state_dir() {
        Ok(p) => p,
        Err(e) => {
            log::error!("rmpc committee register: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let _lock = match AgentLock::acquire(&state_dir, &caller) {
        Ok(l) => l,
        Err(RmpcError::ErrConcurrentInvocation) => {
            emit_failure(
                &CommitteeFailure {
                    ok: false,
                    error: "ErrConcurrentInvocation".to_string(),
                    message: Some(format!(
                        "another rmpc invocation already holds the lock for {caller:#x}"
                    )),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
        Err(e) => {
            log::error!("rmpc committee register: lock acquire failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rt = match build_rt("register") {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc committee register: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let fees = match fetch_fees(&rt, &rpc, &cfg, args.fee_cap_wei, args.pretty, "register") {
        Ok(f) => f,
        Err(code) => return code,
    };

    let nonce =
        match rt.block_on(async { rpc.get_transaction_count(caller, Some("pending")).await }) {
            Ok(n) => n,
            Err(e) => {
                log::error!("rmpc committee register: eth_getTransactionCount failed: {e}");
                return EXIT_STARTUP_FAIL;
            }
        };

    // Encode `InvestmentCommitteePolicy.registerAgent(agent, agentId)`.
    let calldata = InvestmentCommitteePolicy::registerAgentCall {
        agent: agent_addr,
        agentId_: args.agent_id.clone(),
    }
    .abi_encode();

    let tx = build_eip1559(Eip1559Inputs {
        chain_id: cfg.chain_id,
        nonce,
        to: ic_addr,
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
            log::error!("rmpc committee register: envelope signing failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let raw = encode_signed(tx, alloy_sig);

    let tx_hash = match rt.block_on(async { broadcast(&rpc, &raw).await }) {
        Ok(h) => h,
        Err(e) => {
            log::error!("rmpc committee register: broadcast failed: {e}");
            emit_failure(
                &CommitteeFailure {
                    ok: false,
                    error: "ErrBroadcastFailed".to_string(),
                    message: Some(format!("{e}")),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    let max_attempts = args.receipt_timeout_secs.min(u32::MAX as u64) as u32;
    let receipt = match rt.block_on(async {
        wait_for_receipt_with(&rpc, tx_hash, Duration::from_secs(1), max_attempts.max(1)).await
    }) {
        Ok(r) => r,
        Err(e) => {
            emit_failure(
                &CommitteeFailure {
                    ok: false,
                    error: "ErrReceiptTimeout".to_string(),
                    message: Some(format!("{e}")),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    let block_number = receipt.block_number.unwrap_or(0);
    log::info!("rmpc committee register: ok tx_hash={tx_hash:#x} block={block_number}");
    emit_output(
        &CommitteeOutput {
            ok: true,
            action: "register".to_string(),
            tx_hash: format!("{tx_hash:#x}"),
            block_number,
            vote_id: None,
        },
        args.pretty,
    );
    EXIT_OK
}

/// Run `rmpc committee vote-submit`. Returns the process exit code.
pub fn run_vote_submit(args: VoteSubmitArgs) -> i32 {
    let cfg = match Config::from_path(&args.config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc committee vote-submit: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let ic_addr = match resolve_ic_address(&cfg, "vote-submit", args.pretty) {
        Ok(a) => a,
        Err(code) => return code,
    };

    let vault_addr = match parse_address(&args.vault, "rmpc committee vote-submit", "--vault") {
        Ok(a) => a,
        Err(code) => return code,
    };

    let vote_json_hash = match parse_b256(&args.vote_json_hash, "--vote-json-hash") {
        Ok(h) => h,
        Err(code) => return code,
    };
    let prompt_hash = match parse_b256(&args.prompt_hash, "--prompt-hash") {
        Ok(h) => h,
        Err(code) => return code,
    };
    let inputs_digest = match parse_b256(&args.inputs_digest, "--inputs-digest") {
        Ok(h) => h,
        Err(code) => return code,
    };

    if let Err(e) = require_production_grade_for_write(cfg.chain_id, SignerBackendKind::Software) {
        log::error!("rmpc committee vote-submit: {e}");
        emit_failure(
            &CommitteeFailure {
                ok: false,
                error: e.name().to_string(),
                message: Some(format!("{e}")),
            },
            args.pretty,
        );
        return EXIT_REFUSAL;
    }

    let signer = match load_signer(&cfg, "vote-submit", args.pretty) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let caller = signer.public_address();

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc committee vote-submit: caller={caller:#x} ic={ic_addr:#x} vault={vault_addr:#x} stance={} chain_id={} env={}",
        args.stance.as_str(),
        cfg.chain_id,
        network_env.as_str()
    );

    let state_dir = match cfg.resolve_state_dir() {
        Ok(p) => p,
        Err(e) => {
            log::error!("rmpc committee vote-submit: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let _lock = match AgentLock::acquire(&state_dir, &caller) {
        Ok(l) => l,
        Err(RmpcError::ErrConcurrentInvocation) => {
            emit_failure(
                &CommitteeFailure {
                    ok: false,
                    error: "ErrConcurrentInvocation".to_string(),
                    message: Some(format!(
                        "another rmpc invocation already holds the lock for {caller:#x}"
                    )),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
        Err(e) => {
            log::error!("rmpc committee vote-submit: lock acquire failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rt = match build_rt("vote-submit") {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc committee vote-submit: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let fees = match fetch_fees(
        &rt,
        &rpc,
        &cfg,
        args.fee_cap_wei,
        args.pretty,
        "vote-submit",
    ) {
        Ok(f) => f,
        Err(code) => return code,
    };

    let nonce =
        match rt.block_on(async { rpc.get_transaction_count(caller, Some("pending")).await }) {
            Ok(n) => n,
            Err(e) => {
                log::error!("rmpc committee vote-submit: eth_getTransactionCount failed: {e}");
                return EXIT_STARTUP_FAIL;
            }
        };

    // Build VoteParams struct. The `stance` field is ABI-encoded as uint8.
    let params = InvestmentCommitteePolicy::VoteParams {
        agent: caller,
        vault: vault_addr,
        stance: args.stance.to_abi_u8(),
        targetWeightBps: args.target_weight_bps,
        confidence: args.confidence,
        rationaleUri: args.rationale_uri.clone(),
        voteJsonHash: vote_json_hash,
        promptHash: prompt_hash,
        inputsDigest: inputs_digest,
        schemaVersion: args.schema_version.clone(),
        timestamp: args.timestamp,
    };

    let calldata = InvestmentCommitteePolicy::submitVoteCall { p: params }.abi_encode();

    let tx = build_eip1559(Eip1559Inputs {
        chain_id: cfg.chain_id,
        nonce,
        to: ic_addr,
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
            log::error!("rmpc committee vote-submit: envelope signing failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let raw = encode_signed(tx, alloy_sig);

    let tx_hash = match rt.block_on(async { broadcast(&rpc, &raw).await }) {
        Ok(h) => h,
        Err(e) => {
            log::error!("rmpc committee vote-submit: broadcast failed: {e}");
            // When the RPC node simulates the tx and finds the agent is not on
            // the allowlist, it returns `execution reverted` (with or without
            // the ABI-encoded AgentNotAllowlisted selector). Surface this as
            // the named ErrNotAllowlisted variant so callers can match on it.
            let err_str = format!("{e}");
            let (error_name, message) = if err_str.contains("revert") {
                (
                    "ErrNotAllowlisted",
                    Some(format!(
                    "transaction reverted — caller is not an allowlisted committee agent: {err_str}"
                )),
                )
            } else {
                ("ErrBroadcastFailed", Some(err_str))
            };
            emit_failure(
                &CommitteeFailure {
                    ok: false,
                    error: error_name.to_string(),
                    message,
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    let max_attempts = args.receipt_timeout_secs.min(u32::MAX as u64) as u32;
    let receipt = match rt.block_on(async {
        wait_for_receipt_with(&rpc, tx_hash, Duration::from_secs(1), max_attempts.max(1)).await
    }) {
        Ok(r) => r,
        Err(e) => {
            emit_failure(
                &CommitteeFailure {
                    ok: false,
                    error: "ErrReceiptTimeout".to_string(),
                    message: Some(format!("{e}")),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    };

    let block_number = receipt.block_number.unwrap_or(0);
    log::info!("rmpc committee vote-submit: ok tx_hash={tx_hash:#x} block={block_number}");
    emit_output(
        &CommitteeOutput {
            ok: true,
            action: "vote-submit".to_string(),
            tx_hash: format!("{tx_hash:#x}"),
            block_number,
            vote_id: None, // populated by event parsing in a future issue
        },
        args.pretty,
    );
    EXIT_OK
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn resolve_ic_address(cfg: &Config, subcommand: &str, pretty: bool) -> Result<Address, i32> {
    match cfg.ic_policy_address.as_deref() {
        Some(s) => Address::from_str(s).map_err(|e| {
            log::error!("rmpc committee {subcommand}: ic_policy_address parse error: {e}");
            EXIT_STARTUP_FAIL
        }),
        None => {
            log::error!(
                "rmpc committee {subcommand}: ic_policy_address not set in config; \
                 add `ic_policy_address = \"0x...\"` to the operator TOML"
            );
            emit_failure(
                &CommitteeFailure {
                    ok: false,
                    error: "ErrIcContractNotConfigured".to_string(),
                    message: Some(
                        "ic_policy_address is not set in the operator config; \
                         add `ic_policy_address = \"0x...\"` to the TOML"
                            .to_string(),
                    ),
                },
                pretty,
            );
            Err(EXIT_STARTUP_FAIL)
        }
    }
}

fn parse_address(s: &str, cmd: &str, flag: &str) -> Result<Address, i32> {
    Address::from_str(s).map_err(|e| {
        log::error!("{cmd}: {flag} parse error: {e}");
        EXIT_STARTUP_FAIL
    })
}

fn parse_b256(s: &str, flag: &str) -> Result<B256, i32> {
    // Strip 0x prefix if present.
    let hex = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(hex).map_err(|e| {
        log::error!("rmpc committee: {flag} hex decode error: {e}");
        EXIT_STARTUP_FAIL
    })?;
    if bytes.len() != 32 {
        log::error!(
            "rmpc committee: {flag} must be exactly 32 bytes (64 hex chars), got {}",
            bytes.len()
        );
        return Err(EXIT_STARTUP_FAIL);
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(B256::from(arr))
}

fn load_signer(cfg: &Config, subcommand: &str, pretty: bool) -> Result<SoftwareSigner, i32> {
    let passphrase = match std::env::var(PASSPHRASE_ENV_VAR) {
        Ok(s) => s,
        Err(_) => {
            log::error!(
                "rmpc committee {subcommand}: ${PASSPHRASE_ENV_VAR} is unset; \
                 refusing to prompt on stdin from a non-interactive command"
            );
            return Err(EXIT_STARTUP_FAIL);
        }
    };
    SoftwareSigner::load_with_passphrase(
        &cfg.signer.keystore_path,
        passphrase.as_bytes(),
        cfg.signer.allow_software_fallback,
    )
    .map_err(|e| {
        use crate::signer::SignerError;
        if let SignerError::ErrSoftwareSignerDisallowed = e {
            emit_failure(
                &CommitteeFailure {
                    ok: false,
                    error: "ErrSoftwareSignerDisallowed".to_string(),
                    message: Some(
                        "[signer].allow_software_fallback must be true to use the software keystore"
                            .to_string(),
                    ),
                },
                pretty,
            );
            EXIT_REFUSAL
        } else {
            log::error!("rmpc committee {subcommand}: signer load failed: {e}");
            EXIT_STARTUP_FAIL
        }
    })
}

fn build_rt(subcommand: &str) -> Result<tokio::runtime::Runtime, i32> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            log::error!("rmpc committee {subcommand}: tokio runtime build failed: {e}");
            EXIT_STARTUP_FAIL
        })
}

fn fetch_fees(
    rt: &tokio::runtime::Runtime,
    rpc: &FailoverRpcClient,
    cfg: &Config,
    fee_cap_wei: Option<u64>,
    pretty: bool,
    subcommand: &str,
) -> Result<FeeBid, i32> {
    let fh = rt
        .block_on(async { rpc.fee_history(5, "latest", &[50.0]).await })
        .map_err(|e| {
            log::error!("rmpc committee {subcommand}: eth_feeHistory failed: {e}");
            EXIT_STARTUP_FAIL
        })?;

    compute_fees(
        &fh,
        cfg.effective_max_fee_per_gas_cap(fee_cap_wei) as u128,
        cfg.max_priority_fee_per_gas_cap
            .map_or(u128::MAX, |v| v as u128),
    )
    .map_err(|e| {
        emit_failure(
            &CommitteeFailure {
                ok: false,
                error: e.name().to_string(),
                message: Some(format!("{e}")),
            },
            pretty,
        );
        EXIT_REFUSAL
    })
}

fn emit_output(out: &CommitteeOutput, pretty: bool) {
    emit(out, pretty);
}

fn emit_failure(out: &CommitteeFailure, pretty: bool) {
    emit(out, pretty);
}
