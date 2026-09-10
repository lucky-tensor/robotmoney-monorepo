//! Canonical: Plan tracking issue #109 §4.8 / §9 — CLI surface (status subcommand)
//! ADR: docs/technical/rmpc-read-output-contract.md
//!
//! `rmpc status --payment-id 0x…` — read-only payment lookup.
//!
//! Issues three JSON-RPC calls:
//! 1. `eth_chainId` — envelope header.
//! 2. `eth_blockNumber` — envelope header.
//! 3. `eth_getLogs` filtered on the gateway address + `AgentDeposit` topic0
//!    + `paymentId` topic1 — the deposit record.
//!
//! Output follows the Phase 3 shared envelope (`read_output::Envelope<T>`)
//! with `chain_id`, `block_number`, `source: "json_rpc"`, `partial`, `errors`,
//! and `data`. This is the same envelope every `rmpc get-*` command uses
//! (issue #149 migration from the old flat shape).
//!
//! Exit codes:
//! - 0 — exactly one matching log found, decoded successfully.
//! - 0 — no matching log found; output JSON carries `data.status: "not_found"`.
//!   Absence is a valid query result, not an error.
//! - 2 — input parse failure (bad `--payment-id`).
//! - 3 — config / RPC / decode failure (operator-actionable).

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::{Address, LogData, B256};
use alloy_sol_types::SolEvent;
use serde::Serialize;
use serde_json::json;

use crate::config::Config;
use crate::gateway::RobotMoneyGateway;
use crate::network_env::NetworkEnv;
use crate::output::emit;
use crate::read_output::{DecimalU256, PartialBuilder};
use crate::rpc::RawLog;

const EXIT_OK: i32 = 0;
const EXIT_INPUT_FAIL: i32 = 2;
const EXIT_STARTUP_FAIL: i32 = 3;

/// `data` payload for a successful (found) `rmpc status` response.
///
/// Field names are part of the operator-visible contract — downstream e2e
/// tests match on them. Large integers (`amount`, `shares_minted`) serialize
/// as decimal strings via [`DecimalU256`] per the §9 read-output contract.
#[derive(Debug, Serialize)]
pub struct StatusFound {
    pub payment_id: String,
    pub order_id: String,
    pub agent: String,
    pub share_receiver: String,
    /// Deposit amount as a decimal string. Never a lossy JSON number.
    pub amount: DecimalU256,
    /// Shares minted as a decimal string. Never a lossy JSON number.
    pub shares_minted: DecimalU256,
    /// Log block number for the `AgentDeposit` event.
    pub block_number: u64,
    pub tx_hash: String,
}

/// `data` payload when no `AgentDeposit` log exists for the given payment id.
///
/// Consistent with `get-deposit` / `get-tx` not-found representations: the
/// payment id is echoed and a typed `status` field identifies the outcome.
/// The wrapper `Envelope` still carries `chain_id`, `block_number`, `source`,
/// `partial: false`, and `errors: []` so consumers need no special-casing.
#[derive(Debug, Serialize)]
pub struct StatusNotFound {
    pub payment_id: String,
    pub status: &'static str,
}

/// Entry point invoked from `main.rs`. Returns the desired process exit code.
pub fn run(config_path: &Path, payment_id_hex: &str, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc status: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let payment_id = match B256::from_str(payment_id_hex) {
        Ok(b) => b,
        Err(e) => {
            log::error!("rmpc status: --payment-id is not a 32-byte hex string: {e}");
            return EXIT_INPUT_FAIL;
        }
    };

    let gateway_addr = match Address::from_str(&cfg.gateway_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc status: gateway_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc status: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc status: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let topic0 = RobotMoneyGateway::AgentDeposit::SIGNATURE_HASH;

    let pid_hex = format!("{payment_id:#x}");

    // Three-call outcome: chain_id + block_number + getLogs.
    // `Ok((headers, None))` = not_found (valid, exit 0).
    // `Ok((headers, Some(...)))` = found, emit envelope.
    // `Err(msg)` = RPC/decode failure, exit 3.
    type HeaderPair = (u64, u64);
    type Outcome = Result<(HeaderPair, Option<StatusFound>), String>;
    let outcome: Outcome = rt.block_on(async {
        let chain_id = rpc
            .chain_id()
            .await
            .map_err(|e| format!("eth_chainId: {e}"))?;
        let block_number = rpc
            .block_number()
            .await
            .map_err(|e| format!("eth_blockNumber: {e}"))?;
        // Pin the log scan to the envelope block (RPC-9) so the deposit record
        // is resolved against the same block `block_number` advertises, not a
        // later "latest" tip.
        let block_tag = format!("0x{block_number:x}");
        let filter = json!({
            "address": gateway_addr,
            "fromBlock": "earliest",
            "toBlock": block_tag,
            "topics": [topic0, payment_id],
        });
        let logs: Vec<RawLog> = rpc
            .get_logs(filter)
            .await
            .map_err(|e| format!("eth_getLogs: {e}"))?;
        if logs.is_empty() {
            return Ok(((chain_id, block_number), None));
        }
        // The first matching log wins. paymentId is indexed and the gateway
        // rejects replays, so at most one log can ever match.
        let raw = &logs[0];
        let log_data = LogData::new_unchecked(raw.topics.clone(), raw.data.clone());
        let decoded = RobotMoneyGateway::AgentDeposit::decode_log_data(&log_data, true)
            .map_err(|e| format!("AgentDeposit decode: {e}"))?;
        let found = StatusFound {
            payment_id: format!("{:#x}", decoded.paymentId),
            order_id: format!("{:#x}", decoded.orderId),
            agent: format!("{:#x}", decoded.agent),
            share_receiver: format!("{:#x}", decoded.shareReceiver),
            amount: DecimalU256(decoded.amount),
            shares_minted: DecimalU256(decoded.sharesMinted),
            block_number: raw.block_number.to::<u64>(),
            tx_hash: format!("{:#x}", raw.transaction_hash),
        };
        Ok(((chain_id, block_number), Some(found)))
    });

    match outcome {
        Ok(((chain_id, block_number), Some(found))) => {
            let network_env = NetworkEnv::from_chain_id(chain_id);
            log::info!(
                "rmpc status: network environment: {} (chain_id={})",
                network_env.human_label(),
                chain_id
            );
            let env = PartialBuilder::new(chain_id, block_number, found).finish();
            emit(&env, pretty);
            EXIT_OK
        }
        Ok(((chain_id, block_number), None)) => {
            let network_env = NetworkEnv::from_chain_id(chain_id);
            log::info!(
                "rmpc status: network environment: {} (chain_id={})",
                network_env.human_label(),
                chain_id
            );
            // Not-found is a valid query result. Wrap in the shared envelope
            // so callers get chain_id/block_number/source on every response,
            // consistent with get-deposit/get-tx not-found behavior.
            let not_found = StatusNotFound {
                payment_id: pid_hex,
                status: "not_found",
            };
            let env = PartialBuilder::new(chain_id, block_number, not_found).finish();
            emit(&env, pretty);
            EXIT_OK
        }
        Err(msg) => {
            log::error!("rmpc status: {msg}");
            EXIT_STARTUP_FAIL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_output::Envelope;
    use alloy_primitives::U256;

    #[test]
    fn status_found_amounts_serialize_as_decimal_strings() {
        let d = StatusFound {
            payment_id: "0x11".into(),
            order_id: "0x22".into(),
            agent: "0xaa".into(),
            share_receiver: "0xbb".into(),
            amount: DecimalU256(U256::from(2_000_000u64)),
            shares_minted: DecimalU256(U256::from(1_111_111u64)),
            block_number: 99,
            tx_hash: "0x33".into(),
        };
        let v = serde_json::to_value(d).unwrap();
        // Large integers must be strings, never lossy JSON numbers.
        assert!(v["amount"].is_string(), "amount must be a JSON string");
        assert!(
            v["shares_minted"].is_string(),
            "shares_minted must be a JSON string"
        );
        assert_eq!(v["amount"], "2000000");
        assert_eq!(v["shares_minted"], "1111111");
        assert_eq!(v["block_number"], 99u64);
    }

    #[test]
    fn status_found_large_u256_no_precision_loss() {
        // 2^200 — outside any JSON number's safe range.
        let big = U256::from(1u8) << 200;
        let d = StatusFound {
            payment_id: "0x01".into(),
            order_id: "0x02".into(),
            agent: "0x03".into(),
            share_receiver: "0x04".into(),
            amount: DecimalU256(big),
            shares_minted: DecimalU256(U256::ZERO),
            block_number: 1,
            tx_hash: "0x05".into(),
        };
        let v = serde_json::to_value(d).unwrap();
        assert_eq!(v["amount"].as_str().unwrap(), big.to_string());
    }

    #[test]
    fn status_not_found_has_status_field() {
        let d = StatusNotFound {
            payment_id: "0xdeadbeef".into(),
            status: "not_found",
        };
        let v = serde_json::to_value(d).unwrap();
        assert_eq!(v["status"], "not_found");
        assert_eq!(v["payment_id"], "0xdeadbeef");
    }

    #[test]
    fn status_found_in_envelope_has_shared_envelope_fields() {
        let found = StatusFound {
            payment_id: "0x01".into(),
            order_id: "0x02".into(),
            agent: "0x03".into(),
            share_receiver: "0x04".into(),
            amount: DecimalU256(U256::from(500u64)),
            shares_minted: DecimalU256(U256::from(499u64)),
            block_number: 42,
            tx_hash: "0x05".into(),
        };
        let env: Envelope<StatusFound> = PartialBuilder::new(8453, 12_000_000, found).finish();
        let v = serde_json::to_value(&env).unwrap();
        // All Phase 3 envelope top-level fields must be present.
        assert_eq!(v["chain_id"], 8453u64);
        assert_eq!(v["block_number"], 12_000_000u64);
        assert_eq!(v["source"], "json_rpc");
        assert_eq!(v["partial"], false);
        assert!(v["errors"].as_array().unwrap().is_empty());
        // data fields still reachable.
        assert!(v["data"]["amount"].is_string());
        assert_eq!(v["data"]["block_number"], 42u64);
    }

    #[test]
    fn status_not_found_in_envelope_has_shared_envelope_fields() {
        let not_found = StatusNotFound {
            payment_id: "0xdeadbeef".into(),
            status: "not_found",
        };
        let env: Envelope<StatusNotFound> = PartialBuilder::new(8453, 99_999, not_found).finish();
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["chain_id"], 8453u64);
        assert_eq!(v["block_number"], 99_999u64);
        assert_eq!(v["source"], "json_rpc");
        assert_eq!(v["partial"], false);
        assert!(v["errors"].as_array().unwrap().is_empty());
        assert_eq!(v["data"]["status"], "not_found");
        assert_eq!(v["data"]["payment_id"], "0xdeadbeef");
    }
}
