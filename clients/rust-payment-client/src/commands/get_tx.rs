//! Canonical: Plan tracking issue #109 §9 — Phase 3 Direct Chain-Read Query Tooling
//! ADR: docs/technical/rmpc-read-output-contract.md
//!
//! `rmpc get-tx --tx-hash 0x…` — transaction status by hash.
//!
//! Single-read read command. One `eth_getTransactionReceipt`, plus the
//! envelope-header reads. The receipt carries `status`, `block_number`,
//! `gas_used`, and `effective_gas_price` — exactly the §9 fields ("status,
//! block, gas used for a tx hash").
//!
//! Pending transactions (receipt is `null`) are reported with `status:
//! "pending"` and exit code 0. Unknown tx hashes are indistinguishable
//! from pending at the JSON-RPC level; both appear as `null`. The
//! operator distinguishes the two via wall-clock time. Per §9 acceptance
//! criteria, "unknown tx hash" must be a typed error — but only after a
//! confirmation horizon has elapsed, which `rmpc` cannot judge. We
//! therefore emit `status: "not_found_or_pending"` on `null`, exit 4,
//! and document the ambiguity. (`get-tx` would need an extra
//! `eth_getTransactionByHash` to disambiguate; that's a follow-up if
//! consumers ask for it.)
//!
//! Exit codes:
//! - 0 — receipt mined and decoded.
//! - 2 — input parse failure (bad `--tx-hash`).
//! - 3 — config / RPC / decode failure.
//! - 4 — `ErrTxNotFound`: receipt is null (pending or unknown).

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::B256;
use serde::Serialize;

use crate::config::Config;
use crate::network_env::NetworkEnv;
use crate::output::emit;
use crate::read_output::{DecimalU128, DecimalU256, Envelope, PartialBuilder};

const EXIT_OK: i32 = 0;
const EXIT_INPUT_FAIL: i32 = 2;
const EXIT_STARTUP_FAIL: i32 = 3;
const EXIT_NOT_FOUND: i32 = 4;

/// `data` payload for `get-tx`. Captures only the §9-named fields plus
/// the `from`/`to`/`tx_hash` triple that lets a consumer correlate the
/// receipt with its originating tx without a second RPC.
#[derive(Debug, Serialize)]
pub struct TxData {
    /// 0x-hex transaction hash (echoed from `--tx-hash`).
    pub tx_hash: String,
    /// `"success"` if `receipt.status == 1`, `"reverted"` otherwise.
    pub status: &'static str,
    /// Block number the receipt was included in.
    pub block_number: u64,
    /// 0x-hex sender address from the receipt.
    pub from: String,
    /// 0x-hex recipient address from the receipt. `None` for contract
    /// creation; serialized as JSON `null` in that case.
    pub to: Option<String>,
    /// Decimal-string gas consumed by the tx.
    pub gas_used: DecimalU128,
    /// Decimal-string `effective_gas_price` from the receipt (wei per
    /// gas after EIP-1559 priority + base-fee selection).
    pub effective_gas_price: DecimalU256,
}

/// Entry point invoked from `main.rs`. Returns the desired process exit code.
pub fn run(config_path: &Path, tx_hash_hex: &str, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-tx: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let tx_hash = match B256::from_str(tx_hash_hex) {
        Ok(b) => b,
        Err(e) => {
            log::error!("rmpc get-tx: --tx-hash not 32-byte hex: {e}");
            return EXIT_INPUT_FAIL;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc get-tx: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-tx: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    type Outcome = Result<Option<Envelope<TxData>>, String>;
    let outcome: Outcome = rt.block_on(async {
        let chain_id = rpc
            .chain_id()
            .await
            .map_err(|e| format!("eth_chainId: {e}"))?;
        let block_number = rpc
            .block_number()
            .await
            .map_err(|e| format!("eth_blockNumber: {e}"))?;
        let receipt = rpc
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(|e| format!("eth_getTransactionReceipt: {e}"))?;
        let Some(r) = receipt else {
            return Ok(None);
        };
        let status_str = if r.inner.status() {
            "success"
        } else {
            "reverted"
        };
        let env = PartialBuilder::new(
            chain_id,
            block_number,
            TxData {
                tx_hash: format!("{tx_hash:#x}"),
                status: status_str,
                block_number: r.block_number.unwrap_or(0),
                from: format!("{:#x}", r.from),
                to: r.to.map(|a| format!("{a:#x}")),
                gas_used: DecimalU128(r.gas_used),
                effective_gas_price: DecimalU256(alloy_primitives::U256::from(
                    r.effective_gas_price,
                )),
            },
        )
        .finish();
        Ok(Some(env))
    });

    match outcome {
        Ok(Some(env)) => {
            let network_env = NetworkEnv::from_chain_id(env.chain_id);
            log::info!(
                "rmpc get-tx: network environment: {} (chain_id={})",
                network_env.human_label(),
                env.chain_id
            );
            emit(&env, pretty);
            EXIT_OK
        }
        Ok(None) => {
            log::error!(
                "rmpc get-tx: ErrTxNotFound: receipt is null (pending or unknown) for tx_hash={tx_hash_hex}"
            );
            EXIT_NOT_FOUND
        }
        Err(msg) => {
            log::error!("rmpc get-tx: {msg}");
            EXIT_STARTUP_FAIL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    #[test]
    fn tx_data_serializes_with_decimal_strings_and_optional_to() {
        let d = TxData {
            tx_hash: "0xab".into(),
            status: "success",
            block_number: 17,
            from: "0xaa".into(),
            to: None,
            gas_used: DecimalU128(21_000),
            effective_gas_price: DecimalU256(U256::from(1_500_000_000u64)),
        };
        let v = serde_json::to_value(d).unwrap();
        assert_eq!(v["status"], "success");
        assert!(v["gas_used"].is_string());
        assert!(v["effective_gas_price"].is_string());
        assert_eq!(v["gas_used"], "21000");
        assert_eq!(v["to"], serde_json::Value::Null);
    }
}
