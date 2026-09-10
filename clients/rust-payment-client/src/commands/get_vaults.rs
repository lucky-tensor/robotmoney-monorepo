//! Canonical: Plan tracking issue #109 §5.1 — Protocol-scope vault registry reads
//! ADR: docs/technical/rmpc-read-output-contract.md
//! ADR: docs/technical/vault-registry-decisions.md §3.4
//!
//! `rmpc get-vaults` — list all vaults registered in the `VaultRegistry`
//! contract.
//!
//! Sub-reads (all `eth_call`, pinned to a single `eth_blockNumber` snapshot):
//!
//! - `VaultRegistry.listVaults()` → `address[]` of all registered vaults
//!   (active, paused, and retired).
//! - For each vault address: `VaultRegistry.getVault(address)` → **two**
//!   top-level outputs, `(VaultMetadata metadata, VaultStatus status)`, where
//!   `VaultMetadata` is `{ name, asset, registeredAt }`. Two outputs decode as
//!   a positional tuple, not as a single struct — see issue #1362 (the rmpc
//!   half of the drift issue #1348 fixed on the dapp side).
//! - For each vault: `vault.totalAssets()` — live TVL from chain.
//!
//! Output is the §9 envelope from `crate::read_output`. An empty registry
//! (zero vaults) is not an error: the command exits 0 with `vaults: []`.
//!
//! The `registry_address` config field must be set; the command exits non-zero
//! (EXIT_STARTUP_FAIL) when it is absent.
//!
//! Exit codes:
//! - 0 — envelope emitted (including `partial: true` envelopes).
//! - 3 — config / RPC connectivity / address-parse failure.

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::gateway::{MockVault, VaultRegistry};
use crate::network_env::NetworkEnv;
use crate::output::emit;
use crate::read_output::{DecimalU256, Envelope, PartialBuilder};
use crate::rpc::{CallRequest, FailoverRpcClient};

const EXIT_OK: i32 = 0;
const EXIT_STARTUP_FAIL: i32 = 3;

/// One entry in the `vaults` array. Contains registry metadata plus live TVL.
///
/// The registry fields are exactly what `VaultRegistry.getVault` returns
/// today: `VaultMetadata { name, asset, registeredAt }` plus `status`.
/// `risk_label`, `mandate`, `deposit_cap`, `exit_fee_bps` and
/// `receipt_token_address` were an aspirational read shape from
/// `docs/technical/vault-registry-decisions.md` §3.4 that the shipped contract
/// (#329) never implemented, so rmpc could never have populated them from a
/// real chain — it could not even decode the response. Dropped here for the
/// same reasons issue #1348 dropped them from the dapp's `VaultRecord`:
/// `receipt_token_address` always equals the vault address (every vault is its
/// own ERC-4626 share token), and `deposit_cap`/`exit_fee_bps` are live reads
/// on the vault contract, not registry state.
#[derive(Debug, Default, Serialize)]
pub struct VaultEntry {
    /// Vault contract address (lowercase 0x-hex).
    pub address: String,
    /// Human-readable label from the registry.
    pub name: String,
    /// `VaultMetadata.asset` — the ERC-20 the vault denominates in.
    pub asset: String,
    /// Operational status: `"active"`, `"paused"`, or `"retired"`.
    pub status: String,
    /// `VaultMetadata.registeredAt` — block timestamp of `registerVault`.
    pub registered_at: u64,
    /// Live `vault.totalAssets()` — decimal string, or `null` when the
    /// sub-read failed.
    pub total_assets: Option<DecimalU256>,
}

/// `data` payload for `rmpc get-vaults`. Contains one `VaultEntry` per
/// registered vault. Empty when the registry has zero vaults.
#[derive(Debug, Default, Serialize)]
pub struct GetVaultsData {
    /// Registry contract address (from operator config).
    pub registry: String,
    /// All registered vaults (all statuses). Callers filter by `status`.
    pub vaults: Vec<VaultEntry>,
}

/// Entry point invoked from `main.rs`. Returns the process exit code.
pub fn run(config_path: &Path, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-vaults: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let registry_addr = match cfg.registry_address.as_deref() {
        Some(s) => match Address::from_str(s) {
            Ok(a) => a,
            Err(e) => {
                log::error!("rmpc get-vaults: registry_address parse error: {e}");
                return EXIT_STARTUP_FAIL;
            }
        },
        None => {
            log::error!(
                "rmpc get-vaults: registry_address not set in config; \
                 add `registry_address = \"0x...\"` to the operator TOML"
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
            log::error!("rmpc get-vaults: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-vaults: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc get-vaults: network environment: {} (chain_id={})",
        network_env.human_label(),
        cfg.chain_id
    );

    let env = match rt.block_on(read_vaults(&rpc, registry_addr)) {
        Ok(e) => e,
        Err(e) => {
            log::error!("rmpc get-vaults: pre-read setup failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    emit(&env, pretty);
    EXIT_OK
}

/// Drive the vault list read against a pinned block. Pre-read setup failures
/// (chain id, block number, `listVaults()`) propagate as `Err`; per-vault
/// sub-read failures are captured via `record_err` on the builder.
async fn read_vaults(
    rpc: &FailoverRpcClient,
    registry: Address,
) -> crate::errors::Result<Envelope<GetVaultsData>> {
    let chain_id = rpc.chain_id().await?;
    let block_number = rpc.block_number().await?;
    let block_tag = format!("0x{block_number:x}");

    // listVaults() — this is the load-bearing call; if it fails we propagate
    // the error so the caller gets EXIT_STARTUP_FAIL (not a partial envelope).
    let vault_addrs = call_list_vaults(rpc, registry, &block_tag).await?;

    let data = GetVaultsData {
        registry: format!("{registry:#x}"),
        vaults: Vec::with_capacity(vault_addrs.len()),
    };
    let mut b = PartialBuilder::new(chain_id, block_number, data);

    for (i, vault_addr) in vault_addrs.iter().enumerate() {
        let prefix = format!("vaults[{i}]");

        // Fetch the registry record (metadata + status) for this vault.
        let record = match call_get_vault(rpc, registry, *vault_addr, &block_tag).await {
            Ok(r) => Some(r),
            Err(e) => {
                b.record_err(format!("{prefix}.registry_record"), e.to_string());
                None
            }
        };

        // Fetch live totalAssets from the vault contract.
        let total_assets = match call_total_assets(rpc, *vault_addr, &block_tag).await {
            Ok(v) => Some(DecimalU256(v)),
            Err(e) => {
                b.record_err(format!("{prefix}.total_assets"), e);
                None
            }
        };

        let entry = if let Some(rec) = record {
            VaultEntry {
                address: format!("{vault_addr:#x}"),
                name: rec.metadata.name,
                asset: format!("{:#x}", rec.metadata.asset),
                status: vault_status_to_str(rec.status).to_string(),
                registered_at: rec.metadata.registeredAt.saturating_to::<u64>(),
                total_assets,
            }
        } else {
            VaultEntry {
                address: format!("{vault_addr:#x}"),
                total_assets,
                ..Default::default()
            }
        };
        b.data_mut().vaults.push(entry);
    }

    Ok(b.finish())
}

/// Decode a `VaultStatus` enum (uint8) to a stable string.
fn vault_status_to_str(s: u8) -> &'static str {
    match s {
        0 => "active",
        1 => "paused",
        2 => "retired",
        _ => "unknown",
    }
}

// ---- typed view helpers --------------------------------------------------

/// Call `VaultRegistry.listVaults()` and return the decoded `address[]`.
async fn call_list_vaults(
    rpc: &FailoverRpcClient,
    registry: Address,
    block_tag: &str,
) -> crate::errors::Result<Vec<Address>> {
    let data = VaultRegistry::listVaultsCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: registry,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await?;
    let r = VaultRegistry::listVaultsCall::abi_decode_returns(&out, true).map_err(|e| {
        crate::errors::RmpcError::ErrRpcDecode(format!("listVaults abi decode: {e}"))
    })?;
    Ok(r.addresses)
}

/// Call `VaultRegistry.getVault(address)` and return the decoded return value.
///
/// `getVault` has two top-level outputs — `(VaultMetadata metadata, uint8
/// status)` — so the decoded value is a positional tuple, surfaced by
/// `alloy` as a `getVaultReturn { metadata, status }`. It is NOT one merged
/// struct; decoding it as one is the drift issue #1362 fixed.
async fn call_get_vault(
    rpc: &FailoverRpcClient,
    registry: Address,
    vault: Address,
    block_tag: &str,
) -> crate::errors::Result<VaultRegistry::getVaultReturn> {
    let data = VaultRegistry::getVaultCall { vault }.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: registry,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await?;
    let r = VaultRegistry::getVaultCall::abi_decode_returns(&out, true)
        .map_err(|e| crate::errors::RmpcError::ErrRpcDecode(format!("getVault abi decode: {e}")))?;
    Ok(r)
}

/// Call `vault.totalAssets()` and return the decoded `U256`.
async fn call_total_assets(
    rpc: &FailoverRpcClient,
    vault: Address,
    block_tag: &str,
) -> std::result::Result<U256, String> {
    let data = MockVault::totalAssetsCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: vault,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await
        .map_err(|e| format!("eth_call failed: {e}"))?;
    let r = MockVault::totalAssetsCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(r._0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn vault_status_to_str_coverage() {
        assert_eq!(vault_status_to_str(0), "active");
        assert_eq!(vault_status_to_str(1), "paused");
        assert_eq!(vault_status_to_str(2), "retired");
        assert_eq!(vault_status_to_str(99), "unknown");
    }

    #[test]
    fn get_vaults_data_empty_vaults_serialises_as_array() {
        let data = GetVaultsData {
            registry: "0x0000000000000000000000000000000000000001".to_string(),
            vaults: vec![],
        };
        let v: Value = serde_json::to_value(&data).unwrap();
        assert!(v["vaults"].as_array().unwrap().is_empty());
    }

    #[test]
    fn decimal_u256_in_vault_entry_is_string() {
        let entry = VaultEntry {
            address: "0x0000000000000000000000000000000000000001".to_string(),
            name: "Test Vault".to_string(),
            total_assets: Some(DecimalU256(U256::from(1_000_000u64))),
            ..Default::default()
        };
        let v: Value = serde_json::to_value(&entry).unwrap();
        assert!(
            v["total_assets"].is_string(),
            "total_assets must be a JSON string"
        );
        assert_eq!(v["total_assets"].as_str().unwrap(), "1000000");
    }

    #[test]
    fn failed_total_assets_is_null_but_successful_zero_is_string() {
        let failed = VaultEntry::default();
        let failed_json = serde_json::to_value(&failed).unwrap();
        assert!(failed_json["total_assets"].is_null());

        let empty = VaultEntry {
            total_assets: Some(DecimalU256(U256::ZERO)),
            ..Default::default()
        };
        let empty_json = serde_json::to_value(&empty).unwrap();
        assert_eq!(empty_json["total_assets"], "0");
    }

    /// When registry_address is absent from config, run() must return
    /// EXIT_STARTUP_FAIL without touching the network.
    #[test]
    fn run_fails_fast_without_registry_address() {
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
}
