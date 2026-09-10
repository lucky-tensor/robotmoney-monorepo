//! Canonical: docs/architecture.md §5 — On-Chain Gateway
//!
//! `rmpc withdraw-router` — sign and broadcast a gateway multi-vault proportional
//! redemption through the Portfolio Router (agent-initiated).
//!
//! The cross-cutting sequence — signer policy, keystore, single-flight
//! lock, replay cache, runtime, RPC, fees, nonce, sign, broadcast,
//! receipt — belongs to [`crate::write_path`] (issue #1285). What is
//! router-specific and lives here:
//!
//! 1. Argument parsing: `vaults[i]` is identity-bound to
//!    `sharesPerLeg[i]` (issue #967), and `minAssetsPerLeg` is the
//!    per-leg slippage floor (GW-5 / F-11).
//! 2. [`Preflight::run_withdraw_gateway`] against the summed shares, then
//!    [`Preflight::run_withdraw_vault`] per identity-bound leg (RPC-7).
//! 3. The explicit `--confirm` gate: without it the command prints a
//!    preview refusal and exits 2 without signing.
//! 4. `gateway.withdrawFromRouter(...)` calldata.
//! 5. Decoding the `AgentWithdrawalRouted` event log → stable JSON.
//!
//! Exit codes mirror `rmpc withdraw`:
//! - 0 — success.
//! - 2 — preflight refusal, missing --confirm, or on-chain failure.
//! - 3 — startup failure.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, LogData, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use serde::Serialize;

use crate::config::Config;
use crate::gateway::RobotMoneyGateway;
use crate::logging::AuditDecision;
use crate::output::emit;
use crate::policy::{ChecksOutput, Preflight, PreflightInputs};
use crate::replay_cache::OP_WITHDRAW;
use crate::write_path::{
    open_session, Submission, WriteRequest, EXIT_OK, EXIT_REFUSAL, EXIT_STARTUP_FAIL,
    MAX_DEADLINE_SKEW_SECS,
};

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

    // -- Shared write-path prologue ---------------------------------------
    // total_shares is the replay-cache amount field, and OP_WITHDRAW keeps
    // router-withdraw entries disjoint from deposit entries (PAYMENTID-001).
    let mut session = match open_session(
        &cfg,
        WriteRequest {
            command: "withdraw-router",
            gateway: gateway_addr,
            order_id,
            idempotency_key,
            amount: total_shares,
            deadline,
            replay_op: Some(OP_WITHDRAW),
        },
    ) {
        Ok(s) => s,
        Err(abort) => return abort.exit(args.pretty),
    };
    let agent_address = session.agent_address;

    // -- Preflight --------------------------------------------------------
    // The withdrawal-specific gateway preflight (chain id, code hash,
    // gateway paused, agent active+expiry, withdrawal window cap) with
    // totalShares as the amount.
    let preflight_result = session.rt.block_on(async {
        Preflight::new(&session.rpc, &cfg)
            .run_withdraw_gateway(PreflightInputs {
                signer_address: agent_address,
                amount: total_shares,
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

    // -- Per-leg vault share preflight (RPC-7) ----------------------------
    // The gateway preflight above intentionally leaves USDC allowance/balance
    // at zero (N/A for withdrawals). The redeem itself burns the agent's
    // *vault shares*, which the gateway pulls from each source vault, so the
    // agent must (a) hold enough shares in each vault and (b) have approved
    // the gateway to spend them. Run the same per-vault share
    // allowance/balance/paused check the single-vault `withdraw` path uses,
    // once per identity-bound (vault, shares) leg.
    for (vault, leg_shares) in vaults.iter().zip(shares_per_leg.iter()) {
        let leg_result = session.rt.block_on(async {
            Preflight::new(&session.rpc, &cfg)
                .run_withdraw_vault(*vault, gateway_addr, agent_address, *leg_shares)
                .await
        });
        if let Err(err) = leg_result {
            session.record(AuditDecision::Refused, Some(err.name().to_string()));
            return session
                .refusal(err.name())
                .message(format!("vault {vault:#x}: {err}"))
                .checks(ChecksOutput::from_report(&report))
                .emit(args.pretty);
        }
    }

    // -- Explicit confirmation required -----------------------------------
    if !args.confirm {
        eprintln!(
            "rmpc withdraw-router: preview — {} legs, total_shares={total_shares}. Re-run with --confirm to proceed.",
            shares_per_leg.len()
        );
        session.record(
            AuditDecision::Refused,
            Some("ErrConfirmNotProvided".to_string()),
        );
        return session
            .refusal("ErrConfirmNotProvided")
            .message("Pass --confirm to execute. Inspect --get-router first to verify leg amounts.")
            .checks(ChecksOutput::from_report(&report))
            .emit(args.pretty);
    }

    // -- Fees, nonce, sign, broadcast, receipt ----------------------------
    let calldata = RobotMoneyGateway::withdrawFromRouterCall {
        orderId: order_id,
        vaults: vaults.clone(),
        sharesPerLeg: shares_per_leg.clone(),
        minAssetsPerLeg: min_assets_per_leg.clone(),
        deadline,
        idempotencyKey: idempotency_key,
    }
    .abi_encode();

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

    // -- Decode AgentWithdrawalRouted log ---------------------------------
    let topic0 = RobotMoneyGateway::AgentWithdrawalRouted::SIGNATURE_HASH;
    let log = confirmed
        .receipt
        .inner
        .logs()
        .iter()
        .find(|l| l.address() == gateway_addr && l.topics().first() == Some(&topic0));
    let log = match log {
        Some(l) => l,
        None => {
            log::error!("rmpc withdraw-router: AgentWithdrawalRouted event not found in receipt");
            session.record(
                AuditDecision::Refused,
                Some("ErrRouterWithdrawLogMissing".to_string()),
            );
            return session
                .refusal("ErrRouterWithdrawLogMissing")
                .message("AgentWithdrawalRouted event not found in receipt")
                .tx_hash(confirmed.tx_hash_hex.clone())
                .emit(args.pretty);
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

    let block_number = confirmed.receipt.block_number.unwrap_or(0);
    session.audit.payment_id = Some(format!("{:#x}", decoded.paymentId));
    session.record(AuditDecision::Signed, None);
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
        let vaults = vec![Address::repeat_byte(1), Address::repeat_byte(2)];
        let floors = vec![U256::from(59_700_000u64), U256::from(39_800_000u64)];
        let call = RobotMoneyGateway::withdrawFromRouterCall {
            orderId: B256::ZERO,
            vaults,
            sharesPerLeg: legs,
            minAssetsPerLeg: floors,
            deadline: 0u64,
            idempotencyKey: B256::ZERO,
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
