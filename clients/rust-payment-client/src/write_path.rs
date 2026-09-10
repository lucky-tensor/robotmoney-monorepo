//! Canonical: docs/architecture.md §4 — High-Level Flow
//! (See also: docs/technical/rmpc-read-output-contract.md §3.7 — named errors)
//!
//! `write_path` — the orchestration owner for every `rmpc` write command.
//!
//! # Why this module exists
//!
//! The leaf steps of a write have always been shared: [`crate::policy`]
//! owns preflight, [`crate::fees`] owns the fee bid, [`crate::nonce`]
//! owns the single-flight lock, [`crate::tx`] owns envelope build /
//! broadcast / receipt polling. What had **no owner** was the code that
//! *sequences* them, so `deposit`, `withdraw` and `withdraw-router` each
//! carried their own verbatim copy of it — 465 of `withdraw`'s 577 lines
//! appeared in order in `withdraw_router`, and drift had already started
//! (issue #1285).
//!
//! This module owns that sequence, in two halves:
//!
//! - [`open_session`] — the prologue. Signer policy, keystore decrypt,
//!   state dir, single-flight lock, replay-cache open and lookup, tokio
//!   runtime, RPC client, and the audit-record skeleton.
//! - [`WriteSession::submit`] — the epilogue. Fee bid, nonce, envelope
//!   build + sign, broadcast, optimistic replay insert, receipt wait, and
//!   the confirmed-revert refusal.
//!
//! Between the two sits the only genuinely per-command work: which
//! preflight to run, which calldata to encode, and which event log to
//! decode out of the receipt.
//!
//! # One refusal shape
//!
//! Every refusal these commands emit is a [`WriteFailure`], built here.
//! That is what makes the refusal JSON field set identical across
//! `deposit`, `withdraw` and `withdraw-router` by construction rather
//! than by three hand-maintained structs happening to agree.
//!
//! # Injected RPC
//!
//! The chain-touching steps ([`chain_deadline`], [`signed_envelope`],
//! [`broadcast_and_confirm`]) take `&FailoverRpcClient` rather than
//! building one from config, so the orchestration is unit-testable
//! against a `mockito` server without spawning the `rmpc` binary.

use std::path::PathBuf;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types::TransactionReceipt;
use serde::Serialize;

use crate::config::Config;
use crate::errors::{Result as RmpcResult, RmpcError};
use crate::fees::{compute_fees, FeeBid};
use crate::logging::{record_audit, AuditDecision, AuditRecordBuilder};
use crate::network_env::NetworkEnv;
use crate::nonce::AgentLock;
use crate::output::emit;
use crate::policy::ChecksOutput;
use crate::replay_cache::ReplayCache;
use crate::rpc::FailoverRpcClient;
use crate::signer::software::{SoftwareSigner, PASSPHRASE_ENV_VAR};
use crate::signer::{require_production_grade_for_write, AgentSigner, SignerBackendKind};
use crate::tx::{
    broadcast, build_eip1559, encode_signed, signing_hash, wait_for_receipt_with, Eip1559Inputs,
};

/// Process exit code for a completed write.
pub const EXIT_OK: i32 = 0;
/// Process exit code for any refusal — preflight, policy, fee cap, lock
/// contention, replay hit, receipt timeout, or on-chain revert.
pub const EXIT_REFUSAL: i32 = 2;
/// Process exit code for a startup failure: config, keystore, state dir,
/// RPC client, or tokio runtime.
pub const EXIT_STARTUP_FAIL: i32 = 3;

/// Gateway-side maximum deadline skew, mirrored client-side so the daemon
/// never builds a transaction the contract is guaranteed to reject. Keep
/// in sync with `RobotMoneyGateway.MAX_DEADLINE_SKEW`.
///
/// Previously declared in `commands::deposit`, which made `withdraw` and
/// `withdraw_router` import a constant from a sibling command module.
pub const MAX_DEADLINE_SKEW_SECS: u64 = 600;

/// The stable JSON document every write command prints on a refusal.
///
/// `error` is the [`RmpcError::name`] of the underlying failure (or a
/// command-specific name for the handful of refusals that have no
/// `RmpcError` variant, such as `ErrConfirmNotProvided`). `checks` is
/// populated when the refusal came from preflight, so operators get the
/// same snapshot `rmpc self-check` would give them.
#[derive(Debug, Serialize)]
pub struct WriteFailure {
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

impl WriteFailure {
    /// A refusal carrying only the error name. Chain the builders below
    /// for the optional fields.
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            status: "refused",
            error: error.into(),
            message: None,
            agent: None,
            order_id: None,
            tx_hash: None,
            checks: None,
        }
    }

    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn agent(mut self, agent: Address) -> Self {
        self.agent = Some(format!("{agent:#x}"));
        self
    }

    pub fn order_id(mut self, order_id: B256) -> Self {
        self.order_id = Some(format!("{order_id:#x}"));
        self
    }

    pub fn tx_hash(mut self, tx_hash: impl Into<String>) -> Self {
        self.tx_hash = Some(tx_hash.into());
        self
    }

    pub fn checks(mut self, checks: ChecksOutput) -> Self {
        self.checks = Some(checks);
        self
    }

    /// Print the document and return [`EXIT_REFUSAL`].
    pub fn emit(&self, pretty: bool) -> i32 {
        emit(self, pretty);
        EXIT_REFUSAL
    }
}

/// Why the shared write path stopped.
///
/// `Startup` has already logged its cause on stderr and prints nothing on
/// stdout (exit 3); `Refused` carries the operator-visible document
/// (exit 2). Callers turn either into an exit code with [`Self::exit`].
#[derive(Debug)]
pub enum WriteAbort {
    Startup,
    Refused(Box<WriteFailure>),
}

impl WriteAbort {
    /// Emit the refusal document, if any, and return the process exit code.
    pub fn exit(self, pretty: bool) -> i32 {
        match self {
            WriteAbort::Startup => EXIT_STARTUP_FAIL,
            WriteAbort::Refused(f) => f.emit(pretty),
        }
    }

    fn refused(f: WriteFailure) -> Self {
        WriteAbort::Refused(Box::new(f))
    }
}

/// The identity of one write, as supplied by the command's argument
/// parsing. Everything here is known before any I/O happens.
#[derive(Debug, Clone, Copy)]
pub struct WriteRequest {
    /// `"deposit"`, `"withdraw"`, `"withdraw-router"`. Doubles as the
    /// audit `request_type` and the `rmpc <command>:` log prefix.
    pub command: &'static str,
    pub gateway: Address,
    pub order_id: B256,
    pub idempotency_key: B256,
    /// The replay-cache and audit amount field: USDC for `deposit`,
    /// shares for `withdraw`, summed shares for `withdraw-router`.
    pub amount: U256,
    /// Deadline stamped into the audit record at open time. `deposit` and
    /// `withdraw` derive theirs from the block timestamp later and stamp
    /// it then, so they pass 0 here.
    pub deadline: u64,
    /// Replay-cache op-kind prefix. `None` keeps `deposit` on its
    /// pre-op-prefix key shape so existing operator caches keep matching;
    /// the withdraw paths pass [`crate::replay_cache::OP_WITHDRAW`].
    pub replay_op: Option<u8>,
}

/// Everything a write command needs once the prologue has run.
pub struct WriteSession {
    pub request: WriteRequest,
    pub chain_id: u64,
    pub signer: SoftwareSigner,
    pub agent_address: Address,
    pub state_dir: PathBuf,
    pub replay: ReplayCache,
    pub rt: tokio::runtime::Runtime,
    pub rpc: FailoverRpcClient,
    pub audit: AuditRecordBuilder,
    /// Held for the lifetime of the command — dropping it releases the
    /// single-flight agent lock. Never read; the RAII guard is the point.
    _lock: AgentLock,
}

/// Run the prologue every write command shares.
///
/// In order: production-signer policy, keystore decrypt, audit skeleton,
/// state dir, single-flight lock, replay-cache open, replay lookup, tokio
/// runtime, RPC client. Refusals are rendered here so the field set is
/// identical for all three commands; startup failures log and exit 3.
pub fn open_session(cfg: &Config, request: WriteRequest) -> Result<WriteSession, WriteAbort> {
    let cmd = request.command;
    let order_id = request.order_id;

    if let Err(err) = require_production_grade_for_write(cfg.chain_id, SignerBackendKind::Software)
    {
        log::error!("rmpc {cmd}: {err}");
        return Err(WriteAbort::refused(
            WriteFailure::new(err.name())
                .message(format!("{err}"))
                .order_id(order_id),
        ));
    }

    let signer = load_signer(cfg, cmd, order_id)?;
    let agent_address = signer.public_address();
    let backend_label = match signer.backend_kind() {
        SignerBackendKind::Software => "software",
        SignerBackendKind::Hsm => "hsm",
        SignerBackendKind::Kms => "kms",
    };

    // Audit-record skeleton. Filled in incrementally; every exit path
    // below and in `submit` calls `audit.build(...)` + `record_audit`, so
    // every signing decision (success OR refusal) leaves a trail.
    let audit = AuditRecordBuilder {
        agent: format!("{agent_address:#x}"),
        backend: backend_label.to_string(),
        request_type: cmd.to_string(),
        order_id: format!("{order_id:#x}"),
        idempotency_key: format!("{:#x}", request.idempotency_key),
        amount: request.amount.to_string(),
        deadline: request.deadline,
        gateway: format!("{:#x}", request.gateway),
        chain_id: cfg.chain_id,
        tx_hash: None,
        payment_id: None,
    };

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "{cmd}: starting agent={} order_id={} amount={} chain_id={} network_env={}",
        audit.agent,
        audit.order_id,
        audit.amount,
        audit.chain_id,
        network_env.as_str()
    );
    log::info!("{cmd}: network environment: {}", network_env.human_label());
    if let Some(warn) = network_env.production_warning() {
        log::warn!("{cmd}: {warn}");
    }

    // State dir for the per-agent lock + replay cache. Resolved via
    // `Config::resolve_state_dir`: env (`RMPC_STATE_DIR`) → TOML
    // `state_dir` → fail-fast. No silent `/tmp` fallback (audit M1).
    let state_dir = match cfg.resolve_state_dir() {
        Ok(p) => p,
        Err(e) => {
            log::error!("rmpc {cmd}: {e}");
            return Err(WriteAbort::Startup);
        }
    };

    let lock = match AgentLock::acquire(&state_dir, &agent_address) {
        Ok(l) => l,
        Err(RmpcError::ErrConcurrentInvocation) => {
            record_audit(&audit.build(
                AuditDecision::Refused,
                Some("ErrConcurrentInvocation".to_string()),
            ));
            return Err(WriteAbort::refused(
                WriteFailure::new("ErrConcurrentInvocation")
                    .message(format!(
                        "another rmpc invocation already holds the lock for agent {agent_address:#x}"
                    ))
                    .agent(agent_address)
                    .order_id(order_id),
            ));
        }
        Err(e) => {
            log::error!("rmpc {cmd}: lock acquire failed: {e}");
            return Err(WriteAbort::Startup);
        }
    };

    // -- Replay cache (audit M3 / AZ-RPC-2) -------------------------------
    // The cache key is the gateway-equivalent paymentId — deadline is
    // intentionally excluded, mirroring the on-chain formula. On a hit,
    // surface the prior tx_hash and refuse instead of paying gas to
    // discover the same dedupe on chain.
    let replay = match ReplayCache::open(&state_dir) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc {cmd}: replay cache open failed: {e}");
            return Err(WriteAbort::Startup);
        }
    };
    match replay_lookup(&replay, &request, cfg.chain_id, agent_address) {
        Ok(Some(prior_tx)) => {
            let err = RmpcError::ErrOrderIdAlreadySubmitted {
                tx_hash: prior_tx.clone(),
            };
            record_audit(&audit.build(AuditDecision::Refused, Some(err.name().to_string())));
            return Err(WriteAbort::refused(
                WriteFailure::new(err.name())
                    .message(format!("{err}"))
                    .agent(agent_address)
                    .order_id(order_id)
                    .tx_hash(prior_tx),
            ));
        }
        Ok(None) => {}
        Err(e) => {
            log::error!("rmpc {cmd}: replay cache lookup failed: {e}");
            return Err(WriteAbort::Startup);
        }
    }

    // Build the runtime; the rest of the daemon stays sync.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc {cmd}: tokio runtime build failed: {e}");
            return Err(WriteAbort::Startup);
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc {cmd}: rpc client init failed: {e}");
            return Err(WriteAbort::Startup);
        }
    };

    Ok(WriteSession {
        request,
        chain_id: cfg.chain_id,
        signer,
        agent_address,
        state_dir,
        replay,
        rt,
        rpc,
        audit,
        _lock: lock,
    })
}

/// Decrypt the software keystore. `$RMPC_KEYSTORE_PASSPHRASE` being unset
/// is a startup failure — the command refuses to prompt on stdin. An
/// operator policy that disallows the software keystore is a refusal, so
/// harnesses and audit scrapers see `ErrSoftwareSignerDisallowed` on
/// stdout without tailing the diagnostic file.
fn load_signer(cfg: &Config, cmd: &str, order_id: B256) -> Result<SoftwareSigner, WriteAbort> {
    let passphrase = match std::env::var(PASSPHRASE_ENV_VAR) {
        Ok(s) => s,
        Err(_) => {
            log::error!(
                "rmpc {cmd}: ${PASSPHRASE_ENV_VAR} is unset; refusing to prompt on stdin from a non-interactive command"
            );
            return Err(WriteAbort::Startup);
        }
    };
    match SoftwareSigner::load_with_passphrase(
        &cfg.signer.keystore_path,
        passphrase.as_bytes(),
        cfg.signer.allow_software_fallback,
    ) {
        Ok(s) => Ok(s),
        Err(crate::signer::SignerError::ErrSoftwareSignerDisallowed) => {
            log::error!(
                "rmpc {cmd}: ErrSoftwareSignerDisallowed: [signer].allow_software_fallback must be true"
            );
            Err(WriteAbort::refused(
                WriteFailure::new("ErrSoftwareSignerDisallowed")
                    .message(
                        "[signer].allow_software_fallback must be true to use the software keystore",
                    )
                    .order_id(order_id),
            ))
        }
        Err(e) => {
            log::error!("rmpc {cmd}: signer load failed: {e}");
            Err(WriteAbort::Startup)
        }
    }
}

fn replay_lookup(
    replay: &ReplayCache,
    req: &WriteRequest,
    chain_id: u64,
    agent: Address,
) -> RmpcResult<Option<String>> {
    match req.replay_op {
        None => replay.lookup(
            chain_id,
            req.gateway,
            agent,
            req.order_id,
            req.amount,
            req.idempotency_key,
        ),
        Some(op) => replay.lookup_op(
            op,
            chain_id,
            req.gateway,
            agent,
            req.order_id,
            req.amount,
            req.idempotency_key,
        ),
    }
}

/// Why [`signed_envelope`] could not produce a signed transaction.
#[derive(Debug)]
pub enum EnvelopeError {
    /// The computed `maxFeePerGas` breached the operator's cap. An
    /// operator-actionable refusal, not a startup failure.
    FeeCap(RmpcError),
    /// Transport or signing failure; already logged.
    Startup,
}

/// Latest block timestamp + `deadline_secs`.
///
/// Block time, never wall clock: the gateway compares the deadline
/// against `block.timestamp`, so a client clock skewed against the chain
/// would otherwise build transactions the contract rejects.
pub async fn chain_deadline(rpc: &FailoverRpcClient, deadline_secs: u64) -> RmpcResult<u64> {
    let block_number = rpc.block_number().await?;
    let ts = rpc.block_timestamp(block_number).await?;
    Ok(ts.saturating_add(deadline_secs))
}

/// Fee bid → nonce → EIP-1559 envelope → signature → RLP bytes.
///
/// The RPC client is a parameter rather than something built from `cfg`
/// so this whole sequence can be exercised against a `mockito` server.
pub async fn signed_envelope(
    rpc: &FailoverRpcClient,
    cfg: &Config,
    signer: &SoftwareSigner,
    gateway: Address,
    gas_limit: u64,
    fee_cap_wei: Option<u64>,
    calldata: Vec<u8>,
) -> Result<Bytes, EnvelopeError> {
    let fees = fee_bid(rpc, cfg, fee_cap_wei).await?;

    let agent = signer.public_address();
    let nonce = rpc
        .get_transaction_count(agent, Some("pending"))
        .await
        .map_err(|e| {
            log::error!("rmpc: eth_getTransactionCount failed: {e}");
            EnvelopeError::Startup
        })?;

    let tx = build_eip1559(Eip1559Inputs {
        chain_id: cfg.chain_id,
        nonce,
        to: gateway,
        gas_limit,
        fees,
        value: U256::ZERO,
        input: Bytes::from(calldata),
    });
    let hash = signing_hash(&tx);
    let mut hash_bytes = [0u8; 32];
    hash_bytes.copy_from_slice(hash.as_slice());
    let signature = signer.sign_eip1559_hash(&hash_bytes).map_err(|e| {
        log::error!("rmpc: envelope signing failed: {e}");
        EnvelopeError::Startup
    })?;
    Ok(encode_signed(tx, signature))
}

/// `eth_feeHistory` + [`compute_fees`] against the operator's caps.
pub async fn fee_bid(
    rpc: &FailoverRpcClient,
    cfg: &Config,
    fee_cap_wei: Option<u64>,
) -> Result<FeeBid, EnvelopeError> {
    let fh = rpc.fee_history(5, "latest", &[50.0]).await.map_err(|e| {
        log::error!("rmpc: eth_feeHistory failed: {e}");
        EnvelopeError::Startup
    })?;
    compute_fees(
        &fh,
        cfg.effective_max_fee_per_gas_cap(fee_cap_wei) as u128,
        cfg.max_priority_fee_per_gas_cap
            .map_or(u128::MAX, |v| v as u128),
    )
    .map_err(EnvelopeError::FeeCap)
}

/// Broadcast a signed envelope and poll for its receipt.
///
/// The receipt is returned regardless of its `status` field; deciding
/// what a `status == 0` receipt means for the replay cache belongs to
/// [`WriteSession::submit`].
pub async fn broadcast_and_confirm(
    rpc: &FailoverRpcClient,
    raw: &Bytes,
    receipt_timeout_secs: u64,
) -> RmpcResult<(B256, TransactionReceipt)> {
    let tx_hash = broadcast(rpc, raw).await?;
    let max_attempts = receipt_timeout_secs.min(u32::MAX as u64) as u32;
    let receipt =
        wait_for_receipt_with(rpc, tx_hash, Duration::from_secs(1), max_attempts.max(1)).await?;
    Ok((tx_hash, receipt))
}

/// The per-command inputs to [`WriteSession::submit`].
pub struct Submission<'a> {
    /// ABI-encoded gateway call for this command.
    pub calldata: Vec<u8>,
    pub gas_limit: u64,
    /// CLI override for `max_fee_per_gas_cap`, in wei.
    pub fee_cap_wei: Option<u64>,
    pub receipt_timeout_secs: u64,
    /// Deadline stored alongside the optimistic replay-cache entry. Audit
    /// metadata only — the on-chain paymentId formula excludes it.
    pub replay_deadline: u64,
    /// Preflight snapshot attached to the refusals raised before the
    /// transaction reaches the chain. A closure because those refusals are
    /// the unhappy path: the snapshot is not built on a successful write.
    pub checks: &'a dyn Fn() -> ChecksOutput,
}

/// A signed, mined, non-reverted transaction and its receipt.
pub struct Confirmed {
    pub tx_hash_hex: String,
    pub receipt: TransactionReceipt,
}

impl WriteSession {
    /// The refusal document skeleton, pre-filled with the agent and order
    /// id every post-signer refusal carries.
    pub fn refusal(&self, error: &str) -> WriteFailure {
        WriteFailure::new(error)
            .agent(self.agent_address)
            .order_id(self.request.order_id)
    }

    /// Record an audit decision for this session.
    pub fn record(&self, decision: AuditDecision, error: Option<String>) {
        record_audit(&self.audit.build(decision, error));
    }

    /// The epilogue every write command shares: fee bid, nonce, envelope
    /// build + sign, broadcast, optimistic replay insert, receipt wait,
    /// and the confirmed-revert refusal.
    ///
    /// `checks` is the preflight snapshot attached to the refusals raised
    /// before the transaction reaches the chain (fee cap, broadcast
    /// failure). Refusals raised after broadcast carry `tx_hash` and no
    /// `checks`, matching what the operator can actually act on.
    ///
    /// On `Ok` the receipt is mined with `status == 1`; the caller only
    /// has to decode its own event log out of it.
    pub fn submit(
        &mut self,
        cfg: &Config,
        submission: Submission<'_>,
    ) -> Result<Confirmed, WriteAbort> {
        let cmd = self.request.command;
        let Submission {
            calldata,
            gas_limit,
            fee_cap_wei,
            receipt_timeout_secs,
            replay_deadline,
            checks,
        } = submission;

        let raw = match self.rt.block_on(signed_envelope(
            &self.rpc,
            cfg,
            &self.signer,
            self.request.gateway,
            gas_limit,
            fee_cap_wei,
            calldata,
        )) {
            Ok(raw) => raw,
            Err(EnvelopeError::Startup) => return Err(WriteAbort::Startup),
            Err(EnvelopeError::FeeCap(e)) => {
                self.record(AuditDecision::Refused, Some(e.name().to_string()));
                return Err(WriteAbort::refused(
                    self.refusal(e.name())
                        .message(format!("{e}"))
                        .checks(checks()),
                ));
            }
        };

        // -- Broadcast ----------------------------------------------------
        // A broadcast failure is a refusal with a stable name, not a
        // startup failure: the most likely cause is a revert the node
        // simulated ahead of inclusion.
        let tx_hash = match self.rt.block_on(broadcast(&self.rpc, &raw)) {
            Ok(h) => h,
            Err(e) => {
                log::error!("rmpc {cmd}: eth_sendRawTransaction failed: {e}");
                self.record(AuditDecision::BroadcastFailed, Some(e.name().to_string()));
                return Err(WriteAbort::refused(
                    self.refusal(e.name())
                        .message(format!("{e}"))
                        .checks(checks()),
                ));
            }
        };

        let tx_hash_hex = format!("{tx_hash:#x}");
        self.audit.tx_hash = Some(tx_hash_hex.clone());

        // Optimistic insert: recorded immediately after broadcast so a
        // retry hits the local check before paying gas. Non-fatal on
        // failure — the on-chain paymentId remains the source of truth.
        if let Err(e) = self.replay_insert(replay_deadline, &tx_hash_hex) {
            log::warn!("rmpc {cmd}: replay cache insert failed (non-fatal): {e}");
        }

        // -- Receipt ------------------------------------------------------
        let max_attempts = receipt_timeout_secs.min(u32::MAX as u64) as u32;
        let receipt = match self.rt.block_on(wait_for_receipt_with(
            &self.rpc,
            tx_hash,
            Duration::from_secs(1),
            max_attempts.max(1),
        )) {
            Ok(r) => r,
            Err(e) => {
                // AZ-RPC-1 (timeout ≠ failure): the budget ran out but the
                // transaction may still land. Do NOT remove the
                // replay-cache entry — that would allow a second broadcast
                // for the same paymentId while the first is in flight. The
                // operator must inspect this tx_hash before re-submitting.
                self.record(AuditDecision::Refused, Some(e.name().to_string()));
                return Err(WriteAbort::refused(
                    self.refusal(e.name())
                        .message(format!("{e}"))
                        .tx_hash(tx_hash_hex.clone()),
                ));
            }
        };

        if !receipt.inner.status() {
            // RPC-2 (finalize-on-confirmed-failure): the tx reverted, so
            // nothing was recorded on chain. Remove the optimistic entry
            // so a legitimate retry is not permanently refused.
            if let Err(e) = self.replay_remove() {
                log::warn!(
                    "rmpc {cmd}: replay cache finalize-on-failure remove failed (non-fatal): {e}"
                );
            }
            let err = RmpcError::ErrTxReverted {
                tx_hash: tx_hash_hex.clone(),
            };
            self.record(AuditDecision::Reverted, Some(err.name().to_string()));
            return Err(WriteAbort::refused(
                self.refusal(err.name())
                    .message(format!("{err}"))
                    .tx_hash(tx_hash_hex.clone()),
            ));
        }

        Ok(Confirmed {
            tx_hash_hex,
            receipt,
        })
    }

    fn replay_insert(&self, deadline: u64, tx_hash: &str) -> RmpcResult<()> {
        let r = &self.request;
        match r.replay_op {
            None => self.replay.insert(
                self.chain_id,
                r.gateway,
                self.agent_address,
                r.order_id,
                r.amount,
                r.idempotency_key,
                deadline,
                tx_hash,
            ),
            Some(op) => self.replay.insert_op(
                op,
                self.chain_id,
                r.gateway,
                self.agent_address,
                r.order_id,
                r.amount,
                r.idempotency_key,
                deadline,
                tx_hash,
            ),
        }
    }

    fn replay_remove(&self) -> RmpcResult<()> {
        let r = &self.request;
        match r.replay_op {
            None => self.replay.remove(
                self.chain_id,
                r.gateway,
                self.agent_address,
                r.order_id,
                r.amount,
                r.idempotency_key,
            ),
            Some(op) => self.replay.remove_op(
                op,
                self.chain_id,
                r.gateway,
                self.agent_address,
                r.order_id,
                r.amount,
                r.idempotency_key,
            ),
        }
    }
}

#[cfg(test)]
mod tests;
