//! Canonical: docs/architecture.md §4 — High-Level Flow
//! (See also: Plan tracking issue #109 §4.8 — CLI surface)
//!
//! `rmpc deposit` — sign and broadcast a USDC deposit through the gateway.
//!
//! Per `Plan tracking issue #109` §3.8 and issue #16. The cross-cutting
//! sequence — signer policy, keystore, single-flight lock, replay cache,
//! runtime, RPC, fees, nonce, sign, broadcast, receipt — belongs to
//! [`crate::write_path`] (issue #1285). What is deposit-specific and
//! lives here:
//!
//! 1. Argument parsing, including the router (`--destination`) branch.
//! 2. [`Preflight::run`] with the actual deposit amount. Any refusal
//!    exits non-zero with a named-error JSON body — symmetric with
//!    `self-check` so operators can correlate.
//! 3. `gateway.deposit(...)` / `gateway.depositTo(...)` calldata.
//! 4. Decoding the `AgentDeposit` event log → a stable JSON document on
//!    stdout. The shape mirrors `rmpc status` so users can correlate a
//!    deposit response with a later lookup.
//!
//! Exit codes:
//! - 0 — receipt mined with `status == 1` and an `AgentDeposit` log.
//! - 2 — preflight refusal, fee-cap refusal, lock contention, or any
//!   refusal that maps to an [`RmpcError`] variant.
//! - 3 — startup failure: config / keystore / RPC client / runtime build.

use std::path::PathBuf;
use std::str::FromStr;

use alloy_primitives::{Address, LogData, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use serde::Serialize;

use crate::config::Config;
use crate::errors::RmpcError;
use crate::gateway::RobotMoneyGateway;
use crate::logging::AuditDecision;
use crate::output::emit;
use crate::policy::{ChecksOutput, Preflight, PreflightInputs};
use crate::write_path::{
    chain_deadline, open_session, Submission, WriteRequest, EXIT_OK, EXIT_STARTUP_FAIL,
    MAX_DEADLINE_SKEW_SECS,
};

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

/// Entry point invoked from `main.rs`. Returns the desired process exit code.
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

    // -- Shared write-path prologue ---------------------------------------
    // `replay_op: None` keeps deposit on the pre-op-prefix replay-cache key
    // shape, so caches written by older builds still match.
    let mut session = match open_session(
        &cfg,
        WriteRequest {
            command: "deposit",
            gateway: gateway_addr,
            order_id,
            idempotency_key,
            amount,
            deadline: 0,
            replay_op: None,
        },
    ) {
        Ok(s) => s,
        Err(abort) => return abort.exit(args.pretty),
    };
    let agent_address = session.agent_address;

    // -- Deadline from block timestamp ------------------------------------
    let deadline = match session
        .rt
        .block_on(chain_deadline(&session.rpc, deadline_secs))
    {
        Ok(d) => {
            session.audit.deadline = d;
            d
        }
        Err(e) => {
            log::error!("rmpc deposit: failed to fetch block timestamp for deadline: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Preflight --------------------------------------------------------
    let preflight_result = session.rt.block_on(async {
        Preflight::new(&session.rpc, &cfg)
            .run(PreflightInputs {
                signer_address: agent_address,
                amount,
            })
            .await
    });
    let report = match preflight_result {
        Ok(r) => r,
        Err(err) => {
            session.record(AuditDecision::Refused, Some(err.name().to_string()));
            return session
                .refusal(err.name())
                .message(format!("{err}"))
                .checks(ChecksOutput::from_err_partial(&err))
                .emit(args.pretty);
        }
    };

    // -- Calldata: router-deposit (depositTo) vs. single-vault deposit ----
    let calldata = if let Some(ref dest_str) = args.destination {
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

    // -- Fees, nonce, sign, broadcast, receipt ----------------------------
    let confirmed = match session.submit(
        &cfg,
        Submission {
            calldata,
            gas_limit: args.gas_limit,
            fee_cap_wei: args.fee_cap_wei,
            receipt_timeout_secs: args.receipt_timeout_secs,
            replay_deadline: deadline,
            checks: &|| ChecksOutput::from_report(&report),
        },
    ) {
        Ok(c) => c,
        Err(abort) => return abort.exit(args.pretty),
    };

    // -- Decode AgentDeposit log ------------------------------------------
    let topic0 = RobotMoneyGateway::AgentDeposit::SIGNATURE_HASH;
    let log = confirmed
        .receipt
        .inner
        .logs()
        .iter()
        .find(|l| l.address() == gateway_addr && l.topics().first() == Some(&topic0));
    let log = match log {
        Some(l) => l,
        None => {
            let err = RmpcError::ErrAgentDepositLogMissing {
                tx_hash: confirmed.tx_hash_hex.clone(),
            };
            session.record(AuditDecision::Refused, Some(err.name().to_string()));
            return session
                .refusal(err.name())
                .message(format!("{err}"))
                .tx_hash(confirmed.tx_hash_hex.clone())
                .emit(args.pretty);
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

    let block_number = confirmed.receipt.block_number.unwrap_or(0);
    session.audit.payment_id = Some(format!("{:#x}", decoded.paymentId));
    session.record(AuditDecision::Signed, None);
    let out = DepositOutput {
        status: "success",
        payment_id: format!("{:#x}", decoded.paymentId),
        order_id: format!("{:#x}", decoded.orderId),
        agent: format!("{:#x}", decoded.agent),
        share_receiver: format!("{:#x}", decoded.shareReceiver),
        amount: decoded.amount.to_string(),
        shares_minted: decoded.sharesMinted.to_string(),
        block_number,
        tx_hash: confirmed.tx_hash_hex,
        gas_used: confirmed.receipt.gas_used.to_string(),
        effective_gas_price: confirmed.receipt.effective_gas_price.to_string(),
    };
    emit(&out, args.pretty);
    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Matcher;
    use serde_json::json;

    #[tokio::test]
    async fn deadline_is_capped_at_max_skew() {
        // Sanity: the deadline uses the block timestamp, not the wall
        // clock. Mock a block with a known timestamp and verify the
        // deadline is that timestamp plus the capped deadline_secs.
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
        let deadline = chain_deadline(&rpc, deadline_secs.min(MAX_DEADLINE_SKEW_SECS))
            .await
            .unwrap();
        // 0x64a9f4c0 = 1_688_859_840 → + 300 deadline_secs → 1_688_860_140
        assert_eq!(deadline, 1_688_860_140);
    }
}
