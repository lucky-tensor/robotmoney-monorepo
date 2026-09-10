//! Canonical: docs/architecture.md §4 — High-Level Flow
//! (See also: docs/technical/rmpc-read-output-contract.md)
//!
//! `rmpc withdraw` — sign and broadcast a gateway redemption (agent-initiated).
//!
//! Per issue #312. The cross-cutting sequence — signer policy, keystore,
//! single-flight lock, replay cache, runtime, RPC, fees, nonce, sign,
//! broadcast, receipt — belongs to [`crate::write_path`] (issue #1285).
//! What is withdraw-specific and lives here:
//!
//! 1. Argument parsing.
//! 2. The withdraw preflight: [`Preflight::run_withdraw_gateway`] (chain
//!    id, code-hash pin, gateway paused, agent policy active + not
//!    expired, `shares <= maxWithdrawPerPayment`, and
//!    `effectiveWithdrawWindowGross + shares <= maxWithdrawPerWindow` —
//!    issue #449's rolling-window cap), then
//!    [`Preflight::run_withdraw_vault`] for the source vault's paused
//!    flag, share allowance, and share balance.
//! 3. `gateway.withdraw(...)` calldata.
//! 4. Decoding the `AgentWithdrawal` event log → stable JSON on stdout.
//!
//! Exit codes mirror `rmpc deposit`:
//! - 0 — success.
//! - 2 — preflight refusal or on-chain failure.
//! - 3 — startup failure.

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
use crate::replay_cache::OP_WITHDRAW;
use crate::write_path::{
    chain_deadline, open_session, Submission, WriteRequest, EXIT_OK, EXIT_STARTUP_FAIL,
    MAX_DEADLINE_SKEW_SECS,
};

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

    // -- Shared write-path prologue ---------------------------------------
    let mut session = match open_session(
        &cfg,
        WriteRequest {
            command: "withdraw",
            gateway: gateway_addr,
            order_id,
            idempotency_key,
            // `shares` is the replay-cache amount field, mirroring the
            // on-chain paymentId formula. OP_WITHDRAW keeps withdraw
            // entries disjoint from deposit entries (PAYMENTID-001).
            amount: shares,
            deadline: 0,
            replay_op: Some(OP_WITHDRAW),
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
            log::error!("rmpc withdraw: failed to fetch block timestamp for deadline: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // -- Preflight --------------------------------------------------------
    // The withdrawal-specific gateway preflight checks maxWithdrawPerPayment,
    // maxWithdrawPerWindow, and effectiveWithdrawWindowGross — not the
    // deposit caps — so valid withdrawals are not refused and out-of-policy
    // withdrawals are caught before signing (issue #371, #449).
    let preflight_result = session.rt.block_on(async {
        Preflight::new(&session.rpc, &cfg)
            .run_withdraw_gateway(PreflightInputs {
                signer_address: agent_address,
                amount: shares,
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

    // -- Withdraw-specific preflight: vault checks -------------------------
    let vault_preflight_result = session.rt.block_on(async {
        Preflight::new(&session.rpc, &cfg)
            .run_withdraw_vault(source_vault, gateway_addr, agent_address, shares)
            .await
    });
    if let Err(err) = vault_preflight_result {
        session.record(AuditDecision::Refused, Some(err.name().to_string()));
        return session
            .refusal(err.name())
            .message(format!("{err}"))
            .checks(ChecksOutput::from_report(&report))
            .emit(args.pretty);
    }

    // -- Fees, nonce, sign, broadcast, receipt ----------------------------
    let calldata = RobotMoneyGateway::withdrawCall {
        orderId: order_id,
        shares,
        sourceVault: source_vault,
        deadline,
        idempotencyKey: idempotency_key,
    }
    .abi_encode();

    // The replay entry stores deadline 0: the withdraw deadline is derived
    // from the block timestamp inside the runtime, and the on-chain
    // paymentId formula excludes the deadline anyway.
    let confirmed = match session.submit(
        &cfg,
        Submission {
            calldata,
            gas_limit: args.gas_limit,
            fee_cap_wei: args.fee_cap_wei,
            receipt_timeout_secs: args.receipt_timeout_secs,
            replay_deadline: 0,
            checks: &|| ChecksOutput::from_report(&report),
        },
    ) {
        Ok(c) => c,
        Err(abort) => return abort.exit(args.pretty),
    };

    // -- Decode AgentWithdrawal log ----------------------------------------
    let topic0 = RobotMoneyGateway::AgentWithdrawal::SIGNATURE_HASH;
    let log = confirmed
        .receipt
        .inner
        .logs()
        .iter()
        .find(|l| l.address() == gateway_addr && l.topics().first() == Some(&topic0));
    let log = match log {
        Some(l) => l,
        None => {
            let err = RmpcError::ErrAgentWithdrawLogMissing {
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
    let decoded = match RobotMoneyGateway::AgentWithdrawal::decode_log_data(&log_data, true) {
        Ok(d) => d,
        Err(e) => {
            log::error!("rmpc withdraw: failed to decode AgentWithdrawal log: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let block_number = confirmed.receipt.block_number.unwrap_or(0);
    session.audit.payment_id = Some(format!("{:#x}", decoded.paymentId));
    session.record(AuditDecision::Signed, None);
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
    use alloy_primitives::keccak256;

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
