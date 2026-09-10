//! Canonical: docs/architecture.md §4.9 — Consensus Recommendation Receipt Contract
//! Implements: issue #1247 — anchor the consensus receipt on chain (rmpc side).
//!
//! `rmpc receipt` — verify a consensus recommendation receipt off-chain, and anchor
//! its digest on chain through `RobotMoneyGateway.consensusRecordReceipt`.
//!
//! Subcommands:
//! - `verify` — fetch/read the receipt JSON, canonicalize it, print the derived
//!   `payload_digest` and `receipt_id`, and report per-analyst Ed25519
//!   verification. Read-only: no signer, no nonce, no chain.
//! - `submit` — the same checks, and only if EVERY one passes, broadcast
//!   `consensusRecordReceipt(receiptId, payloadDigest, payloadUri)`.
//!
//! # Why the refusal is load-bearing (issue #1247 AC4)
//!
//! The chain proves that one submitter attested to a receipt; it cannot prove
//! that each named analyst signed, because the EVM has no Ed25519 precompile
//! (ADR-0012 §5) and the analyst signatures ride inside the payload as data.
//! A compromised submitter could therefore publish a receipt the analysts never
//! agreed to. `submit` is the check that closes that: it refuses to broadcast
//! when the derived digest disagrees with `--expected-digest`, or when any
//! embedded signature fails. Both refusals happen **before** the signer is
//! loaded, before the nonce lock is taken and before any RPC call, so a refused
//! receipt costs nothing and cannot leave a half-built transaction behind.
//!
//! # Why the call goes to the gateway, not to the receipt contract
//!
//! `ConsensusRecommendationReceipt.recordReceipt` is `onlyGateway`. The calldata is
//! `RobotMoneyGateway::consensusRecordReceiptCall` and it is sent to
//! `cfg.gateway_address`; the caller must hold `AGENT_ROLE` on the gateway and
//! `COMMITTEE_AGENT_ROLE` on the IC policy (docs/architecture.md §4.9.2).
//!
//! Exit codes:
//! - 0 — success.
//! - 2 — refusal (bad receipt, digest mismatch, failed signature, revert).
//! - 3 — startup failure (missing config, keystore, RPC, etc.).

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::consensus_receipt::{ConsensusReceipt, ReceiptError, SignatureCheck};
use crate::errors::RmpcError;
use crate::fees::{compute_fees, FeeBid};
use crate::gateway::RobotMoneyGateway;
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

/// Largest receipt body accepted over HTTP. A consensus receipt is a few KiB;
/// the cap stops a runaway allocation on a hostile or misconfigured URL.
/// Mirrors `services/explorer-indexer/src/indexer.rs::fetch_and_verify_memo`.
const MAX_RECEIPT_BODY_BYTES: usize = 1024 * 1024;

/// HTTP timeout for the receipt fetch. Same value and same reason as the
/// indexer's memo fetch.
const RECEIPT_FETCH_TIMEOUT_SECS: u64 = 10;

// ─── Args ────────────────────────────────────────────────────────────────────

/// Where the receipt bytes come from.
#[derive(Debug, Clone)]
pub enum ReceiptSource {
    /// Fetch over HTTP from the swarm API.
    Url(String),
    /// Read from a local file. For `submit` this supplies the BYTES only; the
    /// anchored `payloadUri` still comes from `--receipt-url`.
    File(PathBuf),
}

/// Args for `rmpc receipt verify`.
#[derive(Debug, Clone)]
pub struct VerifyArgs {
    /// Path to the operator config TOML.
    pub config_path: PathBuf,
    /// Where to read the receipt from.
    pub source: ReceiptSource,
    /// Pretty-print the JSON output.
    pub pretty: bool,
}

/// Args for `rmpc receipt submit`.
#[derive(Debug, Clone)]
pub struct SubmitArgs {
    /// Path to the operator config TOML.
    pub config_path: PathBuf,
    /// The public URI serving these exact bytes. Anchored on chain as
    /// `payloadUri`, so it is required even when `--receipt-file` supplies the
    /// bytes.
    pub receipt_url: String,
    /// Optional local override for the bytes. When absent the receipt is
    /// fetched from `receipt_url`.
    pub receipt_file: Option<PathBuf>,
    /// Optional 0x-prefixed digest the derived `payload_digest` must equal.
    pub expected_digest: Option<String>,
    /// Maximum seconds to wait for the receipt.
    pub receipt_timeout_secs: u64,
    /// Gas limit for the anchor tx envelope.
    pub gas_limit: u64,
    /// Optional override for `max_fee_per_gas_cap` in wei.
    pub fee_cap_wei: Option<u64>,
    /// Pretty-print the JSON output.
    pub pretty: bool,
}

// ─── Output types ────────────────────────────────────────────────────────────

/// Successful `rmpc receipt` output.
#[derive(Debug, Serialize)]
pub struct ReceiptOutput {
    /// Always true.
    pub ok: bool,
    /// `"verify"` or `"submit"`.
    pub action: String,
    /// `keccak256("robotmoney:consensus-receipt-id:v1\n" + session_id + "\n" + subject_id)`.
    pub receipt_id: String,
    /// `keccak256` of the canonical bytes — the value anchored on chain.
    pub payload_digest: String,
    /// Length of the canonical preimage, so an operator can eyeball a
    /// truncated fetch without re-deriving anything.
    pub canonical_bytes: usize,
    /// The receipt's session.
    pub session_id: String,
    /// The receipt's subject.
    pub subject_id: String,
    /// Present on `submit`: the URI anchored beside the digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_uri: Option<String>,
    /// Per-analyst Ed25519 verification. Every entry is `verified: true` when
    /// the command succeeds — a single failure is a refusal, not a false entry.
    pub analyst_signatures: Vec<SignatureCheck>,
    /// Present on `submit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    /// Present on `submit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number: Option<u64>,
}

/// Failure envelope. `error` is the stable machine-readable code.
#[derive(Debug, Serialize)]
pub struct ReceiptFailure {
    /// Always false.
    pub ok: bool,
    /// Stable `ErrX` code.
    pub error: String,
    /// Human-readable detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ─── Public entry points ─────────────────────────────────────────────────────

/// Run `rmpc receipt verify`. Returns the process exit code.
///
/// Purely off-chain: it loads the config (for logging and for symmetry with the
/// write path) but never opens a keystore, takes a nonce lock, or calls an RPC.
pub fn run_verify(args: VerifyArgs) -> i32 {
    if let Err(e) = Config::from_path(&args.config_path) {
        log::error!("rmpc receipt verify: failed to load config: {e}");
        return EXIT_STARTUP_FAIL;
    }

    let rt = match build_rt("verify") {
        Ok(rt) => rt,
        Err(code) => return code,
    };

    let raw = match load_receipt_bytes(&rt, &args.source, "verify", args.pretty) {
        Ok(b) => b,
        Err(code) => return code,
    };

    let checked = match check_receipt(&raw, None, "verify", args.pretty) {
        Ok(c) => c,
        Err(code) => return code,
    };

    log::info!(
        "rmpc receipt verify: ok receipt_id={:#x} payload_digest={:#x} signatures={}",
        checked.receipt_id,
        checked.payload_digest,
        checked.signatures.len()
    );
    emit_output(
        &ReceiptOutput {
            ok: true,
            action: "verify".to_string(),
            receipt_id: format!("{:#x}", checked.receipt_id),
            payload_digest: format!("{:#x}", checked.payload_digest),
            canonical_bytes: checked.canonical_len,
            session_id: checked.receipt.session_id.clone(),
            subject_id: checked.receipt.subject_id.clone(),
            payload_uri: None,
            analyst_signatures: checked.signatures,
            tx_hash: None,
            block_number: None,
        },
        args.pretty,
    );
    EXIT_OK
}

/// Run `rmpc receipt submit`. Returns the process exit code.
///
/// The digest check and the Ed25519 check both run before any signer, lock or
/// RPC work; a receipt that fails either is refused with **no transaction
/// sent**.
pub fn run_submit(args: SubmitArgs) -> i32 {
    let cfg = match Config::from_path(&args.config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc receipt submit: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let gateway_addr = match Address::from_str(&cfg.gateway_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc receipt submit: gateway_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let expected_digest = match args.expected_digest.as_deref() {
        None => None,
        Some(s) => match parse_b256(s) {
            Ok(h) => Some(h),
            Err(msg) => {
                log::error!("rmpc receipt submit: --expected-digest: {msg}");
                emit_failure(
                    &ReceiptFailure {
                        ok: false,
                        error: "ErrReceiptDigestMalformed".to_string(),
                        message: Some(format!("--expected-digest: {msg}")),
                    },
                    args.pretty,
                );
                return EXIT_REFUSAL;
            }
        },
    };

    let rt = match build_rt("submit") {
        Ok(rt) => rt,
        Err(code) => return code,
    };

    let source = match &args.receipt_file {
        Some(p) => ReceiptSource::File(p.clone()),
        None => ReceiptSource::Url(args.receipt_url.clone()),
    };
    let raw = match load_receipt_bytes(&rt, &source, "submit", args.pretty) {
        Ok(b) => b,
        Err(code) => return code,
    };

    // ── THE REFUSAL GATE (issue #1247 AC4) ───────────────────────────────────
    // Canonicalize, derive the digest, compare it against --expected-digest if
    // one was supplied, and verify EVERY embedded analyst signature. Nothing
    // below this point runs unless all of it passed, so a bad receipt never
    // reaches a keystore, a nonce, or the network.
    let checked = match check_receipt(&raw, expected_digest, "submit", args.pretty) {
        Ok(c) => c,
        Err(code) => return code,
    };

    if let Err(e) = require_production_grade_for_write(cfg.chain_id, SignerBackendKind::Software) {
        log::error!("rmpc receipt submit: {e}");
        emit_failure(
            &ReceiptFailure {
                ok: false,
                error: e.name().to_string(),
                message: Some(format!("{e}")),
            },
            args.pretty,
        );
        return EXIT_REFUSAL;
    }

    let signer = match load_signer(&cfg, "submit", args.pretty) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let caller = signer.public_address();

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc receipt submit: caller={caller:#x} gateway={gateway_addr:#x} \
         receipt_id={:#x} payload_digest={:#x} chain_id={} env={}",
        checked.receipt_id,
        checked.payload_digest,
        cfg.chain_id,
        network_env.as_str()
    );

    let state_dir = match cfg.resolve_state_dir() {
        Ok(p) => p,
        Err(e) => {
            log::error!("rmpc receipt submit: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let _lock = match AgentLock::acquire(&state_dir, &caller) {
        Ok(l) => l,
        Err(RmpcError::ErrConcurrentInvocation) => {
            emit_failure(
                &ReceiptFailure {
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
            log::error!("rmpc receipt submit: lock acquire failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc receipt submit: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let fees = match fetch_fees(&rt, &rpc, &cfg, args.fee_cap_wei, args.pretty, "submit") {
        Ok(f) => f,
        Err(code) => return code,
    };

    let nonce =
        match rt.block_on(async { rpc.get_transaction_count(caller, Some("pending")).await }) {
            Ok(n) => n,
            Err(e) => {
                log::error!("rmpc receipt submit: eth_getTransactionCount failed: {e}");
                return EXIT_STARTUP_FAIL;
            }
        };

    let calldata = encode_record_receipt_call(
        checked.receipt_id,
        checked.payload_digest,
        &args.receipt_url,
    );

    // TO THE GATEWAY, never to the receipt contract: `recordReceipt` is
    // `onlyGateway` (docs/architecture.md §4.9.2).
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
            log::error!("rmpc receipt submit: envelope signing failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let raw_tx = encode_signed(tx, alloy_sig);

    let tx_hash = match rt.block_on(async { broadcast(&rpc, &raw_tx).await }) {
        Ok(h) => h,
        Err(e) => {
            log::error!("rmpc receipt submit: broadcast failed: {e}");
            let err_str = format!("{e}");
            // A node that simulates the tx returns `execution reverted` when the
            // caller lacks AGENT_ROLE/COMMITTEE_AGENT_ROLE, or when this
            // receipt_id is already recorded (one receipt per session per
            // subject). Both are refusals, not transport failures.
            let (error_name, message) = if err_str.contains("revert") {
                (
                    "ErrReceiptRecordReverted",
                    Some(format!(
                        "transaction reverted — the caller may lack AGENT_ROLE on the gateway \
                         or COMMITTEE_AGENT_ROLE on the IC policy, or this receipt_id is \
                         already recorded: {err_str}"
                    )),
                )
            } else {
                ("ErrBroadcastFailed", Some(err_str))
            };
            emit_failure(
                &ReceiptFailure {
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
                &ReceiptFailure {
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
    log::info!("rmpc receipt submit: ok tx_hash={tx_hash:#x} block={block_number}");
    emit_output(
        &ReceiptOutput {
            ok: true,
            action: "submit".to_string(),
            receipt_id: format!("{:#x}", checked.receipt_id),
            payload_digest: format!("{:#x}", checked.payload_digest),
            canonical_bytes: checked.canonical_len,
            session_id: checked.receipt.session_id.clone(),
            subject_id: checked.receipt.subject_id.clone(),
            payload_uri: Some(args.receipt_url.clone()),
            analyst_signatures: checked.signatures,
            tx_hash: Some(format!("{tx_hash:#x}")),
            block_number: Some(block_number),
        },
        args.pretty,
    );
    EXIT_OK
}

/// Encode `RobotMoneyGateway.consensusRecordReceipt(receiptId, payloadDigest,
/// payloadUri)`.
///
/// Public so the integration tests can ABI-decode the calldata this exact
/// function produces and assert the `payloadDigest` argument equals the digest
/// derived from the pinned golden — which is what discharges the second half of
/// issue #1280 on the rmpc side. A test that re-encoded the call itself would
/// only ever agree with itself.
pub fn encode_record_receipt_call(
    receipt_id: B256,
    payload_digest: B256,
    payload_uri: &str,
) -> Vec<u8> {
    RobotMoneyGateway::consensusRecordReceiptCall {
        receiptId: receipt_id,
        payloadDigest: payload_digest,
        payloadUri: payload_uri.to_string(),
    }
    .abi_encode()
}

// ─── The check ───────────────────────────────────────────────────────────────

/// Everything the refusal gate derives, carried forward so the caller never
/// re-canonicalizes (and so cannot accidentally anchor a second derivation).
struct CheckedReceipt {
    receipt: ConsensusReceipt,
    receipt_id: B256,
    payload_digest: B256,
    canonical_len: usize,
    signatures: Vec<SignatureCheck>,
}

/// Parse, validate, canonicalize, derive, compare and verify — in that order.
///
/// `expected_digest` is `Some` only when the operator supplied
/// `--expected-digest`; the comparison is then mandatory.
fn check_receipt(
    raw: &[u8],
    expected_digest: Option<B256>,
    subcommand: &str,
    pretty: bool,
) -> Result<CheckedReceipt, i32> {
    let receipt = ConsensusReceipt::from_json_slice(raw).map_err(|e| {
        refuse(&e, subcommand, pretty);
        EXIT_REFUSAL
    })?;

    // Validates before canonicalizing; a missing required field is refused
    // here rather than silently omitted from the bytes.
    let canonical = receipt.canonical_bytes().map_err(|e| {
        refuse(&e, subcommand, pretty);
        EXIT_REFUSAL
    })?;

    let payload_digest = crate::consensus_receipt::payload_digest(&canonical);
    let receipt_id = receipt.receipt_id();

    if let Some(expected) = expected_digest {
        if expected != payload_digest {
            log::error!(
                "rmpc receipt {subcommand}: derived digest {payload_digest:#x} does not equal \
                 --expected-digest {expected:#x}; refusing to submit"
            );
            emit_failure(
                &ReceiptFailure {
                    ok: false,
                    error: "ErrReceiptDigestMismatch".to_string(),
                    message: Some(format!(
                        "derived payload_digest {payload_digest:#x} does not equal \
                         --expected-digest {expected:#x} — the fetched bytes are not the bytes \
                         the caller intended to anchor"
                    )),
                },
                pretty,
            );
            return Err(EXIT_REFUSAL);
        }
    }

    let signatures = receipt.verify_analyst_signatures().map_err(|e| {
        refuse(&e, subcommand, pretty);
        EXIT_REFUSAL
    })?;

    Ok(CheckedReceipt {
        receipt,
        receipt_id,
        payload_digest,
        canonical_len: canonical.len(),
        signatures,
    })
}

fn refuse(e: &ReceiptError, subcommand: &str, pretty: bool) {
    log::error!("rmpc receipt {subcommand}: {e}");
    emit_failure(
        &ReceiptFailure {
            ok: false,
            error: e.code().to_string(),
            message: Some(format!("{e}")),
        },
        pretty,
    );
}

// ─── Receipt bytes ───────────────────────────────────────────────────────────

fn load_receipt_bytes(
    rt: &tokio::runtime::Runtime,
    source: &ReceiptSource,
    subcommand: &str,
    pretty: bool,
) -> Result<Vec<u8>, i32> {
    match source {
        ReceiptSource::File(path) => std::fs::read(path).map_err(|e| {
            log::error!("rmpc receipt {subcommand}: read {}: {e}", path.display());
            emit_failure(
                &ReceiptFailure {
                    ok: false,
                    error: "ErrReceiptReadFailed".to_string(),
                    message: Some(format!("read {}: {e}", path.display())),
                },
                pretty,
            );
            EXIT_REFUSAL
        }),
        ReceiptSource::Url(url) => rt
            .block_on(async { fetch_receipt(url).await })
            .map_err(|e| {
                log::error!("rmpc receipt {subcommand}: {e}");
                emit_failure(
                    &ReceiptFailure {
                        ok: false,
                        error: "ErrReceiptFetchFailed".to_string(),
                        message: Some(e),
                    },
                    pretty,
                );
                EXIT_REFUSAL
            }),
    }
}

/// GET the receipt JSON, capped at [`MAX_RECEIPT_BODY_BYTES`] with a
/// [`RECEIPT_FETCH_TIMEOUT_SECS`]-second timeout.
async fn fetch_receipt(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(RECEIPT_FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("GET {url} returned {}", resp.status()));
    }

    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("read body from {url}: {e}"))?;

    if body.len() > MAX_RECEIPT_BODY_BYTES {
        return Err(format!(
            "receipt body from {url} is {} bytes (max {MAX_RECEIPT_BODY_BYTES})",
            body.len()
        ));
    }

    Ok(body.to_vec())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn parse_b256(s: &str) -> Result<B256, String> {
    let hex = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(hex).map_err(|e| format!("hex decode error: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!(
            "must be exactly 32 bytes (64 hex chars), got {}",
            bytes.len()
        ));
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
                "rmpc receipt {subcommand}: ${PASSPHRASE_ENV_VAR} is unset; \
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
                &ReceiptFailure {
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
            log::error!("rmpc receipt {subcommand}: signer load failed: {e}");
            EXIT_STARTUP_FAIL
        }
    })
}

fn build_rt(subcommand: &str) -> Result<tokio::runtime::Runtime, i32> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            log::error!("rmpc receipt {subcommand}: tokio runtime build failed: {e}");
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
            log::error!("rmpc receipt {subcommand}: eth_feeHistory failed: {e}");
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
            &ReceiptFailure {
                ok: false,
                error: e.name().to_string(),
                message: Some(format!("{e}")),
            },
            pretty,
        );
        EXIT_REFUSAL
    })
}

fn emit_output(out: &ReceiptOutput, pretty: bool) {
    emit(out, pretty);
}

fn emit_failure(out: &ReceiptFailure, pretty: bool) {
    emit(out, pretty);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolCall;

    /// The calldata the anchoring path builds must ABI-decode back to exactly
    /// the arguments it was handed. The end-to-end version of this — decoding
    /// the digest derived from the pinned golden — lives in
    /// `tests/receipt.rs::submitted_calldata_carries_the_pinned_digest`.
    #[test]
    fn record_receipt_calldata_round_trips() {
        let receipt_id = B256::repeat_byte(0xab);
        let digest = B256::repeat_byte(0xcd);
        let uri = "https://robotmoney.net/api/swarm/receipts/12440000-0000-4000-8000-000000000001";

        let calldata = encode_record_receipt_call(receipt_id, digest, uri);
        let decoded =
            RobotMoneyGateway::consensusRecordReceiptCall::abi_decode(&calldata, true).unwrap();

        assert_eq!(decoded.receiptId, receipt_id);
        assert_eq!(decoded.payloadDigest, digest);
        assert_eq!(decoded.payloadUri, uri);
    }

    #[test]
    fn expected_digest_parsing_accepts_both_spellings_and_refuses_short_input() {
        let with_prefix = format!("0x{}", "ab".repeat(32));
        let without = "ab".repeat(32);
        assert_eq!(
            parse_b256(&with_prefix).unwrap(),
            parse_b256(&without).unwrap()
        );
        assert!(parse_b256("0xabcd").is_err());
        assert!(parse_b256("nothex").is_err());
    }
}
