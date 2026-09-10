//! Canonical: docs/architecture.md §4 — High-Level Flow
//! Implements: issue #632
//!
//! `rmpc vote` — cast a vote on an active `RouterGovernance` proposal.
//!
//! The contract supports only FOR votes (`vote(proposalId)`). The `--choice`
//! flag accepts `yes`, `no`, or `abstain` for API symmetry, but `no` and
//! `abstain` do not submit an on-chain transaction (the contract has no
//! mechanism to record them). Calling `rmpc vote --choice yes` casts a
//! FOR vote on-chain.
//!
//! Idempotency:
//! - Calling with `--choice yes` when the caller has already voted `yes`
//!   exits 0 (no-op check via `hasVoted`).
//! - Calling with `--choice no` or `--choice abstain` when the caller has
//!   already voted `yes` exits 2 with `ErrVoteAlreadyCast`.
//!
//! Exit codes:
//! - 0 — vote cast (or idempotent no-op).
//! - 2 — `ErrVoteAlreadyCast` (different direction after an on-chain yes).
//! - 3 — startup failure: missing `governance_address`, keystore, RPC, etc.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::errors::RmpcError;
use crate::fees::compute_fees;
use crate::gateway::RouterGovernance;
use crate::network_env::NetworkEnv;
use crate::nonce::AgentLock;
use crate::output::emit;
use crate::rpc::{CallRequest, FailoverRpcClient};
use crate::signer::software::{SoftwareSigner, PASSPHRASE_ENV_VAR};
use crate::signer::{require_production_grade_for_write, AgentSigner, SignerBackendKind};
use crate::tx::{
    broadcast, build_eip1559, encode_signed, signing_hash, wait_for_receipt_with, Eip1559Inputs,
};

const EXIT_OK: i32 = 0;
const EXIT_REFUSAL: i32 = 2;
const EXIT_STARTUP_FAIL: i32 = 3;

/// Default gas limit for the `vote` transaction.
const DEFAULT_GAS_LIMIT: u64 = 200_000;

/// Vote choice supplied by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteChoice {
    Yes,
    No,
    Abstain,
}

impl VoteChoice {
    /// Parse from a string; case-insensitive.
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "yes" => Some(VoteChoice::Yes),
            "no" => Some(VoteChoice::No),
            "abstain" => Some(VoteChoice::Abstain),
            _ => None,
        }
    }

    /// Wire string for JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            VoteChoice::Yes => "yes",
            VoteChoice::No => "no",
            VoteChoice::Abstain => "abstain",
        }
    }
}

/// Inputs collected by `main.rs` from the CLI parser.
#[derive(Debug, Clone)]
pub struct Args {
    pub config_path: PathBuf,
    /// The proposal id to vote on.
    pub proposal_id: String,
    /// The vote direction.
    pub choice: VoteChoice,
    /// Gas limit override.
    pub gas_limit: u64,
    /// Optional override for `max_fee_per_gas_cap` in wei.
    pub fee_cap_wei: Option<u64>,
    /// Maximum seconds to wait for the receipt. Default 60.
    pub receipt_timeout_secs: u64,
    pub pretty: bool,
}

/// Receipt details in the ok-envelope.
#[derive(Debug, Serialize)]
pub struct VoteReceipt {
    pub proposal_id: String,
    pub choice: String,
    pub tx_hash: String,
    pub block_number: u64,
}

/// Ok-envelope (submitted or idempotent no-op).
#[derive(Debug, Serialize)]
pub struct VoteOutput {
    pub ok: bool,
    /// `"cast"` if the vote was submitted; `"noop"` if already voted same direction.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<VoteReceipt>,
}

/// Failure envelope.
#[derive(Debug, Serialize)]
pub struct VoteFailure {
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
            log::error!("rmpc vote: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let governance_addr = match cfg.governance_address.as_deref() {
        Some(s) => match Address::from_str(s) {
            Ok(a) => a,
            Err(e) => {
                log::error!("rmpc vote: governance_address parse error: {e}");
                return EXIT_STARTUP_FAIL;
            }
        },
        None => {
            log::error!(
                "rmpc vote: governance_address not set in config; \
                 add `governance_address = \"0x...\"` to the operator TOML"
            );
            return EXIT_STARTUP_FAIL;
        }
    };

    let proposal_id_u256 = match parse_proposal_id(&args.proposal_id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("rmpc vote: --proposal-id is not a valid integer: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // Production-grade signer check (only needed when submitting on-chain).
    if args.choice == VoteChoice::Yes {
        if let Err(err) =
            require_production_grade_for_write(cfg.chain_id, SignerBackendKind::Software)
        {
            log::error!("rmpc vote: {err}");
            emit_failure(
                &VoteFailure {
                    ok: false,
                    error: err.name().to_string(),
                    message: Some(format!("{err}")),
                },
                args.pretty,
            );
            return EXIT_REFUSAL;
        }
    }

    // Decrypt keystore.
    let passphrase = match std::env::var(PASSPHRASE_ENV_VAR) {
        Ok(s) => s,
        Err(_) => {
            log::error!(
                "rmpc vote: ${PASSPHRASE_ENV_VAR} is unset; refusing to prompt on stdin from a non-interactive command"
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
                "rmpc vote: ErrSoftwareSignerDisallowed: [signer].allow_software_fallback must be true"
            );
            emit_failure(
                &VoteFailure {
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
            log::error!("rmpc vote: signer load failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let agent_address = signer.public_address();

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc vote: agent={agent_address:#x} governance={governance_addr:#x} proposal_id={} choice={} chain_id={} network_env={}",
        args.proposal_id,
        args.choice.as_str(),
        cfg.chain_id,
        network_env.as_str(),
    );

    // Acquire per-agent lock.
    let state_dir = match cfg.resolve_state_dir() {
        Ok(p) => p,
        Err(e) => {
            log::error!("rmpc vote: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let _lock = match AgentLock::acquire(&state_dir, &agent_address) {
        Ok(l) => l,
        Err(RmpcError::ErrConcurrentInvocation) => {
            emit_failure(
                &VoteFailure {
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
            log::error!("rmpc vote: lock acquire failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc vote: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc vote: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // Check hasVoted on-chain.
    let already_voted = rt.block_on(async {
        call_has_voted(
            &rpc,
            governance_addr,
            proposal_id_u256,
            agent_address,
            "latest",
        )
        .await
    });
    let already_voted = match already_voted {
        Ok(v) => v,
        Err(e) => {
            log::error!("rmpc vote: hasVoted call failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    match args.choice {
        VoteChoice::Yes => {
            if already_voted {
                // Idempotent: same choice as on-chain state (they previously cast YES).
                log::info!(
                    "rmpc vote: already voted FOR proposal_id={}; returning no-op",
                    args.proposal_id
                );
                emit(
                    &VoteOutput {
                        ok: true,
                        status: "noop".to_string(),
                        receipt: None,
                    },
                    args.pretty,
                );
                return EXIT_OK;
            }
            // Submit the on-chain vote.
        }
        VoteChoice::No | VoteChoice::Abstain => {
            if already_voted {
                // Already voted YES on-chain; different direction = ErrVoteAlreadyCast.
                let err = RmpcError::ErrVoteAlreadyCast {
                    proposal_id: args.proposal_id.clone(),
                };
                emit_failure(
                    &VoteFailure {
                        ok: false,
                        error: "ErrVoteAlreadyCast".to_string(),
                        message: Some(format!("{err}")),
                    },
                    args.pretty,
                );
                return EXIT_REFUSAL;
            }
            // No on-chain action for no/abstain when not yet voted.
            // The contract only supports FOR votes; no-op exit 0.
            log::info!(
                "rmpc vote: choice={} for proposal_id={}; no on-chain action (contract supports FOR only)",
                args.choice.as_str(),
                args.proposal_id
            );
            emit(
                &VoteOutput {
                    ok: true,
                    status: "noop".to_string(),
                    receipt: None,
                },
                args.pretty,
            );
            return EXIT_OK;
        }
    }

    // Submit on-chain YES vote.
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
                    &VoteFailure {
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
            log::error!("rmpc vote: eth_feeHistory failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let nonce = match rt.block_on(async {
        rpc.get_transaction_count(agent_address, Some("pending"))
            .await
    }) {
        Ok(n) => n,
        Err(e) => {
            log::error!("rmpc vote: eth_getTransactionCount failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let calldata = RouterGovernance::voteCall {
        proposalId: proposal_id_u256,
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
            log::error!("rmpc vote: envelope signing failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let raw = encode_signed(tx, alloy_sig);

    let tx_hash = match rt.block_on(async { broadcast(&rpc, &raw).await }) {
        Ok(h) => h,
        Err(e) => {
            emit_failure(
                &VoteFailure {
                    ok: false,
                    error: e.name().to_string(),
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
                &VoteFailure {
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
            &VoteFailure {
                ok: false,
                error: "ErrTxReverted".to_string(),
                message: Some(format!(
                    "vote transaction reverted on-chain (tx_hash={tx_hash:#x})"
                )),
            },
            args.pretty,
        );
        return EXIT_REFUSAL;
    }

    let block_number = receipt.block_number.unwrap_or(0);
    emit(
        &VoteOutput {
            ok: true,
            status: "cast".to_string(),
            receipt: Some(VoteReceipt {
                proposal_id: args.proposal_id,
                choice: args.choice.as_str().to_string(),
                tx_hash: format!("{tx_hash:#x}"),
                block_number,
            }),
        },
        args.pretty,
    );
    EXIT_OK
}

/// `eth_call` to `RouterGovernance.hasVoted(proposalId, voter)`.
async fn call_has_voted(
    rpc: &FailoverRpcClient,
    governance: Address,
    proposal_id: U256,
    voter: Address,
    block_tag: &str,
) -> crate::errors::Result<bool> {
    let data = RouterGovernance::hasVotedCall {
        proposalId: proposal_id,
        voter,
    }
    .abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: governance,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await?;
    let r = RouterGovernance::hasVotedCall::abi_decode_returns(&out, true)
        .map_err(|e| crate::errors::RmpcError::ErrRpcDecode(format!("hasVoted abi decode: {e}")))?;
    Ok(r._0)
}

/// Parse `--proposal-id` decimal-first (RPC-5).
///
/// The CLI contract documents `--proposal-id` as a decimal integer, so bare
/// digits must be interpreted base-10: `"10"` → proposal 10, `"100"` → 100.
/// A leading `0x` (case-insensitive) opts into hex: `"0x10"` → 16. The old
/// behaviour parsed hex-first, so `"10"` silently voted proposal 16 — voting
/// the wrong proposal with no error surfaced.
fn parse_proposal_id(raw: &str) -> crate::errors::Result<U256> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(RmpcError::ErrConfig("proposal-id is empty".to_string()));
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        U256::from_str_radix(hex, 16)
            .map_err(|e| RmpcError::ErrConfig(format!("proposal-id hex parse: {e}")))
    } else {
        U256::from_str(s)
            .map_err(|e| RmpcError::ErrConfig(format!("proposal-id decimal parse: {e}")))
    }
}

fn emit_failure(out: &VoteFailure, pretty: bool) {
    emit(out, pretty);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RPC-5: bare digits are decimal; only an explicit `0x` prefix is hex.
    /// `"10"` must encode proposalId 10 (not 16); `"0x10"` must encode 16.
    #[test]
    fn proposal_id_is_parsed_decimal_first() {
        assert_eq!(parse_proposal_id("10").unwrap(), U256::from(10u64));
        assert_eq!(parse_proposal_id("0x10").unwrap(), U256::from(16u64));
        assert_eq!(parse_proposal_id("0X10").unwrap(), U256::from(16u64));
        assert_eq!(parse_proposal_id("100").unwrap(), U256::from(100u64));
        assert_eq!(parse_proposal_id("0").unwrap(), U256::ZERO);
        // Surrounding whitespace is tolerated.
        assert_eq!(parse_proposal_id("  42  ").unwrap(), U256::from(42u64));
        // Hex digits without the 0x prefix are NOT valid decimal → error,
        // rather than being silently reinterpreted as hex.
        assert!(parse_proposal_id("1a").is_err());
        assert!(parse_proposal_id("").is_err());
    }

    #[test]
    fn vote_choice_parses_all_variants() {
        assert_eq!(VoteChoice::from_str_ci("yes"), Some(VoteChoice::Yes));
        assert_eq!(VoteChoice::from_str_ci("YES"), Some(VoteChoice::Yes));
        assert_eq!(VoteChoice::from_str_ci("no"), Some(VoteChoice::No));
        assert_eq!(
            VoteChoice::from_str_ci("abstain"),
            Some(VoteChoice::Abstain)
        );
        assert_eq!(VoteChoice::from_str_ci("invalid"), None);
    }

    #[test]
    fn vote_fails_fast_without_governance_address() {
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
            proposal_id: "1".to_string(),
            choice: VoteChoice::Yes,
            gas_limit: DEFAULT_GAS_LIMIT,
            fee_cap_wei: None,
            receipt_timeout_secs: 60,
            pretty: false,
        };
        let code = run(args);
        assert_eq!(code, EXIT_STARTUP_FAIL);
    }

    #[test]
    fn vote_fails_fast_with_uninitialized_signer() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let keystore = tmp.path().join("keystore.json");
        let cfg_path = tmp.path().join("rmpc.toml");
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
        std::env::remove_var("RMPC_KEYSTORE_PASSPHRASE");
        let args = Args {
            config_path: cfg_path,
            proposal_id: "1".to_string(),
            choice: VoteChoice::Yes,
            gas_limit: DEFAULT_GAS_LIMIT,
            fee_cap_wei: None,
            receipt_timeout_secs: 60,
            pretty: false,
        };
        let code = run(args);
        assert_eq!(code, EXIT_STARTUP_FAIL);
    }

    #[test]
    fn vote_result_serializes_correctly() {
        let out = VoteOutput {
            ok: true,
            status: "cast".to_string(),
            receipt: Some(VoteReceipt {
                proposal_id: "1".to_string(),
                choice: "yes".to_string(),
                tx_hash: "0xabcdef".to_string(),
                block_number: 99,
            }),
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "cast");
        assert_eq!(v["receipt"]["proposal_id"], "1");
        assert_eq!(v["receipt"]["choice"], "yes");
        assert_eq!(v["receipt"]["block_number"], 99);
    }

    #[test]
    fn vote_noop_serializes_without_receipt() {
        let out = VoteOutput {
            ok: true,
            status: "noop".to_string(),
            receipt: None,
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "noop");
        assert!(v.get("receipt").is_none() || v["receipt"].is_null());
    }

    #[test]
    fn vote_failure_serializes_correctly() {
        let fail = VoteFailure {
            ok: false,
            error: "ErrVoteAlreadyCast".to_string(),
            message: Some(
                "a different vote direction was already cast for proposal_id=1".to_string(),
            ),
        };
        let v: serde_json::Value = serde_json::to_value(&fail).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "ErrVoteAlreadyCast");
    }
}
