//! Canonical: Plan tracking issue #109 — "Router-weight governance" phase
//! Implements: issue #309 (rmpc get-router subprocess assertion)
//!
//! `rmpc get-router` — direct on-chain read of Portfolio Router state.
//!
//! Sub-reads (all `eth_call`, pinned to a single `eth_blockNumber` snapshot):
//!
//! - `PortfolioRouter.getWeights()` → current vault address list and bps.
//! - `PortfolioRouter.routerCap()` → global deposit cap (0 = uncapped).
//!
//! Config field `router_address` must be set; exits `EXIT_STARTUP_FAIL`
//! when absent.
//!
//! Exit codes:
//! - 0 — envelope emitted.
//! - 3 — config / RPC connectivity / address-parse failure.

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::gateway::PortfolioRouter;
use crate::network_env::NetworkEnv;
use crate::output::emit;
use crate::read_output::{DecimalU256, Envelope, PartialBuilder};
use crate::rpc::{CallRequest, FailoverRpcClient};

const EXIT_OK: i32 = 0;
const EXIT_STARTUP_FAIL: i32 = 3;

/// One entry in the `weights` array.
#[derive(Debug, Default, Serialize)]
pub struct WeightEntry {
    /// Vault contract address (lowercase 0x-hex).
    pub vault: String,
    /// Weight in basis points (0–10 000).
    pub weight_bps: u64,
}

/// `data` payload for `rmpc get-router`.
#[derive(Debug, Default, Serialize)]
pub struct RouterData {
    /// Portfolio Router address (from operator config).
    pub address: String,
    /// Current vault weight vector. Empty when no weights are set.
    pub weights: Vec<WeightEntry>,
    /// Global router cap in USDC's smallest unit (decimal string).
    /// `"0"` means uncapped.
    pub router_cap: DecimalU256,
}

/// Entry point invoked from `main.rs`. Returns the process exit code.
pub fn run(config_path: &Path, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-router: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let router_addr = match cfg.router_address.as_deref() {
        Some(s) => match Address::from_str(s) {
            Ok(a) => a,
            Err(e) => {
                log::error!("rmpc get-router: router_address parse error: {e}");
                return EXIT_STARTUP_FAIL;
            }
        },
        None => {
            log::error!(
                "rmpc get-router: router_address not set in config; \
                 add `router_address = \"0x...\"` to the operator TOML"
            );
            return EXIT_STARTUP_FAIL;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc get-router: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-router: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc get-router: network environment: {} (chain_id={})",
        network_env.human_label(),
        cfg.chain_id
    );

    let env = match rt.block_on(read_router(&rpc, router_addr)) {
        Ok(e) => e,
        Err(e) => {
            log::error!("rmpc get-router: pre-read setup failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    emit(&env, pretty);
    EXIT_OK
}

async fn read_router(
    rpc: &FailoverRpcClient,
    router: Address,
) -> crate::errors::Result<Envelope<RouterData>> {
    let chain_id = rpc.chain_id().await?;
    let block_number = rpc.block_number().await?;
    let block_tag = format!("0x{block_number:x}");

    let data = RouterData {
        address: format!("{router:#x}"),
        weights: Vec::new(),
        router_cap: DecimalU256(U256::ZERO),
    };
    let mut b = PartialBuilder::new(chain_id, block_number, data);

    // Read getWeights().
    match call_get_weights(rpc, router, &block_tag).await {
        Ok((vaults, bps)) => {
            let entries: Vec<WeightEntry> = vaults
                .into_iter()
                .zip(bps)
                .map(|(v, bps_val)| WeightEntry {
                    vault: format!("{v:#x}"),
                    weight_bps: bps_val.saturating_to::<u64>(),
                })
                .collect();
            b.data_mut().weights = entries;
        }
        Err(e) => {
            b.record_err("weights".to_string(), e.to_string());
        }
    }

    // Read routerCap().
    match call_router_cap(rpc, router, &block_tag).await {
        Ok(cap) => {
            b.data_mut().router_cap = DecimalU256(cap);
        }
        Err(e) => {
            b.record_err("router_cap".to_string(), e);
        }
    }

    Ok(b.finish())
}

async fn call_get_weights(
    rpc: &FailoverRpcClient,
    router: Address,
    block_tag: &str,
) -> crate::errors::Result<(Vec<Address>, Vec<U256>)> {
    let data = PortfolioRouter::getWeightsCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: router,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await?;
    let r = PortfolioRouter::getWeightsCall::abi_decode_returns(&out, true).map_err(|e| {
        crate::errors::RmpcError::ErrRpcDecode(format!("getWeights abi decode: {e}"))
    })?;
    Ok((r.vaults, r.bps))
}

async fn call_router_cap(
    rpc: &FailoverRpcClient,
    router: Address,
    block_tag: &str,
) -> std::result::Result<U256, String> {
    let data = PortfolioRouter::routerCapCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: router,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await
        .map_err(|e| format!("eth_call failed: {e}"))?;
    let r = PortfolioRouter::routerCapCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(r._0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_fails_fast_without_router_address() {
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
        let code = run(&cfg_path, false);
        assert_eq!(code, EXIT_STARTUP_FAIL);
    }

    #[test]
    fn weight_entry_serialises_correctly() {
        let entry = WeightEntry {
            vault: "0x0000000000000000000000000000000000000001".to_string(),
            weight_bps: 6000,
        };
        let v: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["weight_bps"].as_u64().unwrap(), 6000);
        assert_eq!(
            v["vault"].as_str().unwrap(),
            "0x0000000000000000000000000000000000000001"
        );
    }
}
