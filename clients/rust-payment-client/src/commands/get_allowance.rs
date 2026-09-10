//! Canonical: Plan tracking issue #109 §9 — Phase 3 Direct Chain-Read Query Tooling
//! ADR: docs/technical/rmpc-read-output-contract.md
//!
//! `rmpc get-allowance --owner 0x… --spender 0x…` — ERC-20 allowance.
//!
//! Single-read read command per §9. One `eth_call` to
//! `allowance(owner, spender)` on the configured USDC token, plus the
//! envelope header reads (`eth_chainId`, `eth_blockNumber`).
//!
//! Exit codes:
//! - 0 — allowance read succeeded; JSON envelope emitted on stdout.
//! - 2 — input parse failure (bad `--owner` or `--spender`).
//! - 3 — config / RPC / decode failure.

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::gateway::Erc20;
use crate::network_env::NetworkEnv;
use crate::output::emit;
use crate::read_output::{DecimalU256, Envelope, PartialBuilder};
use crate::rpc::{CallRequest, FailoverRpcClient};

const EXIT_OK: i32 = 0;
const EXIT_INPUT_FAIL: i32 = 2;
const EXIT_STARTUP_FAIL: i32 = 3;

/// `data` payload for `get-allowance`.
#[derive(Debug, Serialize)]
pub struct AllowanceData {
    /// 0x-hex token contract address (the USDC configured in operator TOML).
    pub token: String,
    /// 0x-hex owner address (the holder who granted approval).
    pub owner: String,
    /// 0x-hex spender address (typically the gateway).
    pub spender: String,
    /// Decimal-string allowance in the token's smallest unit.
    pub allowance: DecimalU256,
}

/// Entry point invoked from `main.rs`. Returns the desired process exit code.
pub fn run(config_path: &Path, owner_hex: &str, spender_hex: &str, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-allowance: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let owner = match Address::from_str(owner_hex) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc get-allowance: --owner not a 20-byte hex address: {e}");
            return EXIT_INPUT_FAIL;
        }
    };
    let spender = match Address::from_str(spender_hex) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc get-allowance: --spender not a 20-byte hex address: {e}");
            return EXIT_INPUT_FAIL;
        }
    };

    let token = match Address::from_str(&cfg.usdc_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc get-allowance: usdc_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc get-allowance: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-allowance: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let result: Result<Envelope<AllowanceData>, String> = rt.block_on(async {
        let chain_id = rpc
            .chain_id()
            .await
            .map_err(|e| format!("eth_chainId: {e}"))?;
        let block_number = rpc
            .block_number()
            .await
            .map_err(|e| format!("eth_blockNumber: {e}"))?;
        // Pin the allowance read to the envelope block (RPC-16) so the value
        // matches the advertised `block_number` rather than drifting to
        // "latest".
        let block_tag = format!("0x{block_number:x}");
        let allowance = call_allowance(&rpc, token, owner, spender, &block_tag)
            .await
            .map_err(|e| format!("allowance: {e}"))?;
        Ok(PartialBuilder::new(
            chain_id,
            block_number,
            AllowanceData {
                token: format!("{token:#x}"),
                owner: format!("{owner:#x}"),
                spender: format!("{spender:#x}"),
                allowance: DecimalU256(allowance),
            },
        )
        .finish())
    });

    match result {
        Ok(env) => {
            let network_env = NetworkEnv::from_chain_id(env.chain_id);
            log::info!(
                "rmpc get-allowance: network environment: {} (chain_id={})",
                network_env.human_label(),
                env.chain_id
            );
            emit(&env, pretty);
            EXIT_OK
        }
        Err(msg) => {
            log::error!("rmpc get-allowance: {msg}");
            EXIT_STARTUP_FAIL
        }
    }
}

async fn call_allowance(
    rpc: &FailoverRpcClient,
    token: Address,
    owner: Address,
    spender: Address,
    block_tag: &str,
) -> Result<U256, crate::errors::RmpcError> {
    let data = Erc20::allowanceCall { owner, spender }.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: token,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await?;
    let decoded = Erc20::allowanceCall::abi_decode_returns(&out, true)
        .map_err(|e| crate::errors::RmpcError::ErrRpcDecode(format!("allowance decode: {e}")))?;
    Ok(decoded._0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RPC-16: `call_allowance` must issue its `eth_call` against the pinned
    /// envelope block tag, not `"latest"`.
    #[tokio::test]
    async fn allowance_pins_eth_call_to_envelope_block_tag() {
        use alloy_primitives::{address, hex as ahex};
        let mut server = mockito::Server::new_async().await;
        let mut word = [0u8; 32];
        word[31] = 9;
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"0x{}"}}"#,
            ahex::encode(word)
        );
        let m = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "method": "eth_call",
                "params": [{}, "0x2a"]
            })))
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create_async()
            .await;
        let rpc = crate::rpc::FailoverRpcClient::new(vec![server.url()]).unwrap();

        let got = call_allowance(
            &rpc,
            address!("0000000000000000000000000000000000000bbb"),
            address!("00000000000000000000000000000000000000aa"),
            address!("0000000000000000000000000000000000000ccc"),
            "0x2a",
        )
        .await
        .unwrap();
        assert_eq!(got, U256::from(9u64));
        m.assert_async().await;
    }

    #[test]
    fn allowance_data_serializes_with_decimal_string() {
        let d = AllowanceData {
            token: "0xbb".into(),
            owner: "0xaa".into(),
            spender: "0xcc".into(),
            allowance: DecimalU256(U256::MAX),
        };
        let v = serde_json::to_value(d).unwrap();
        assert!(
            v["allowance"].is_string(),
            "allowance must be string per §9 contract"
        );
        assert_eq!(v["allowance"], U256::MAX.to_string());
    }
}
