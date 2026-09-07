//! Canonical: Plan tracking issue #109 §9 — Phase 3 Direct Chain-Read Query Tooling
//! ADR: docs/technical/rmpc-read-output-contract.md
//!
//! `rmpc get-vault` — direct on-chain read of a vault.
//!
//! Two modes:
//!
//! 1. **Config-vault mode** (no `--address` flag): reads the single vault
//!    pinned in the operator config via the gateway's `vault()` view.
//!    Sub-reads: `gateway.vault()`, `vault.asset()`, `vault.name()`,
//!    `vault.symbol()`, `vault.decimals()`, `vault.totalAssets()`,
//!    `vault.totalSupply()`. Legacy mode — kept for backwards compatibility.
//!
//! 2. **Registry mode** (`--address <addr>`): looks up the vault in the
//!    `VaultRegistry` contract at `config.registry_address`, then augments
//!    with live ERC-4626 state (`totalAssets`, `totalSupply`, `share_price`).
//!    Returns registry metadata (name, status, registered_at) plus live
//!    accounting state. `getVault` returns **two** top-level outputs —
//!    `(VaultMetadata metadata, VaultStatus status)` — so it decodes as a
//!    positional tuple, not one merged struct (issue #1362).
//!    Exits non-zero when the address is not registered.
//!
//! `share_price` is computed as `totalAssets * 10^decimals / totalSupply`,
//! in the underlying-asset's smallest unit. When `totalSupply == 0` the
//! price is reported as `null`.
//!
//! Per §9 acceptance criteria, fields not exposed by the deployed contract
//! surface are reported as `"not_onchain"` in the `notes` map (config-vault
//! mode only). Registry mode omits them: the registry does not carry
//! `deposit_cap`, `exit_fee_bps`, a risk label or a mandate either — see
//! `RegistryVaultData` and issue #1362.
//!
//! Exit codes: 0 (envelope, possibly partial), 3 (pre-read setup fail).

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::gateway::{MockVault, RobotMoneyGateway, VaultRegistry};
use crate::network_env::NetworkEnv;
use crate::read_output::{DecimalU256, Envelope, PartialBuilder};
use crate::rpc::{CallRequest, FailoverRpcClient};

const EXIT_OK: i32 = 0;
const EXIT_STARTUP_FAIL: i32 = 3;

/// `data` payload for `rmpc get-vault`. Field order is the wire order;
/// snapshot tests assert on it.
#[derive(Debug, Default, Serialize)]
pub struct VaultData {
    /// Vault address as resolved from the operator config.
    pub address: String,
    /// `gateway.vault()` — what the gateway *actually* routes to. Empty
    /// when the cross-check sub-read failed; consumers can compare
    /// `gateway_vault` and `address` to detect drift between operator
    /// config and the deployed gateway.
    pub gateway_vault: String,
    /// `vault.asset()` lowercase 0x-hex.
    pub asset: String,
    /// `vault.name()`. Empty string when the read failed.
    pub name: String,
    /// `vault.symbol()`.
    pub symbol: String,
    /// `vault.decimals()` — ERC-20 share decimals.
    pub decimals: u8,
    /// `vault.totalAssets()` — `uint256` decimal string.
    pub total_assets: DecimalU256,
    /// `vault.totalSupply()` — `uint256` decimal string.
    pub total_supply: DecimalU256,
    /// Computed share price as `totalAssets * 10^decimals / totalSupply`,
    /// rendered as a decimal string. `null` when `totalSupply == 0`
    /// (price is undefined).
    pub share_price: Option<String>,
    /// Per §9: explicit markers for fields the deployed contract
    /// surface does not expose. The map values are the literal string
    /// `"not_onchain"`; future ABI extensions promote them to first-
    /// class fields in a follow-up batch.
    pub notes: VaultNotes,
}

/// Sentinel block: every entry is the literal `"not_onchain"`. A future
/// vault ABI that exposes these views replaces the sentinel with a real
/// field on `VaultData` and removes it from this struct.
#[derive(Debug, Serialize)]
pub struct VaultNotes {
    pub deposit_cap: &'static str,
    pub paused: &'static str,
    pub shutdown: &'static str,
    pub adapters: &'static str,
    pub fees: &'static str,
}

impl Default for VaultNotes {
    fn default() -> Self {
        Self {
            deposit_cap: "not_onchain",
            paused: "not_onchain",
            shutdown: "not_onchain",
            adapters: "not_onchain",
            fees: "not_onchain",
        }
    }
}

/// `data` payload for `rmpc get-vault <address>` (registry mode).
/// Includes every field the registry actually returns plus live ERC-4626
/// accounting state. Field order is the wire order; snapshot tests assert on it.
///
/// `risk_label`, `mandate`, `receipt_token_address`, `deposit_cap` and
/// `exit_fee_bps` are gone (issue #1362): they came from an aspirational
/// registry read shape (`docs/technical/vault-registry-decisions.md` §3.4) that
/// the shipped `VaultRegistry.sol` (#329) never implemented, so this command
/// could not decode a real `getVault` response at all. Issue #1348 removed the
/// same five from the dapp's `VaultRecord`; `deposit_cap` and `exit_fee_bps`
/// have real live getters on the vault contract itself, and the receipt token
/// always equals the vault address.
#[derive(Debug, Default, Serialize)]
pub struct RegistryVaultData {
    /// Vault address (from registry record).
    pub address: String,
    /// Human-readable vault name from the registry.
    pub name: String,
    /// Operational status: `"active"`, `"paused"`, or `"retired"`.
    pub status: String,
    /// Unix timestamp when the vault was registered.
    pub registered_at: u64,
    /// `vault.totalAssets()` — live from chain.
    pub total_assets: DecimalU256,
    /// `vault.totalSupply()` — live from chain.
    pub total_supply: DecimalU256,
    /// Computed share price. `null` when `totalSupply == 0`.
    pub share_price: Option<String>,
    /// `vault.asset()` — underlying ERC-20 address.
    pub asset: String,
    /// `vault.decimals()`.
    pub decimals: u8,
}

/// Decode a `VaultStatus` enum value (uint8) to a stable string.
fn status_to_str(s: u8) -> &'static str {
    match s {
        0 => "active",
        1 => "paused",
        2 => "retired",
        _ => "unknown",
    }
}

pub fn run(config_path: &Path, address: Option<&str>, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-vault: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc get-vault: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-vault: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc get-vault: network environment: {} (chain_id={})",
        network_env.human_label(),
        cfg.chain_id
    );

    if let Some(addr_str) = address {
        // Registry mode: look up vault by address in the VaultRegistry.
        let vault_addr = match Address::from_str(addr_str) {
            Ok(a) => a,
            Err(e) => {
                log::error!("rmpc get-vault: vault address parse error: {e}");
                return EXIT_STARTUP_FAIL;
            }
        };
        let registry_addr = match cfg.registry_address.as_deref() {
            Some(s) => match Address::from_str(s) {
                Ok(a) => a,
                Err(e) => {
                    log::error!("rmpc get-vault: registry_address parse error: {e}");
                    return EXIT_STARTUP_FAIL;
                }
            },
            None => {
                log::error!(
                    "rmpc get-vault: registry_address not set in config; \
                     required for get-vault <address> mode"
                );
                return EXIT_STARTUP_FAIL;
            }
        };
        let result = rt.block_on(read_vault_from_registry(&rpc, registry_addr, vault_addr));
        match result {
            Ok(env) => {
                emit(&env, pretty);
                EXIT_OK
            }
            Err(e) => {
                log::error!("rmpc get-vault: registry read failed: {e}");
                EXIT_STARTUP_FAIL
            }
        }
    } else {
        // Config-vault mode (legacy): read the single vault pinned in operator config.
        let gateway_addr = match Address::from_str(&cfg.gateway_address) {
            Ok(a) => a,
            Err(e) => {
                log::error!("rmpc get-vault: gateway_address parse error: {e}");
                return EXIT_STARTUP_FAIL;
            }
        };
        let vault_addr = match Address::from_str(&cfg.vault_address) {
            Ok(a) => a,
            Err(e) => {
                log::error!("rmpc get-vault: vault_address parse error: {e}");
                return EXIT_STARTUP_FAIL;
            }
        };
        let env = match rt.block_on(read_vault(&rpc, gateway_addr, vault_addr)) {
            Ok(e) => e,
            Err(e) => {
                log::error!("rmpc get-vault: pre-read setup failed: {e}");
                return EXIT_STARTUP_FAIL;
            }
        };
        emit(&env, pretty);
        EXIT_OK
    }
}

async fn read_vault(
    rpc: &FailoverRpcClient,
    gateway: Address,
    vault: Address,
) -> crate::errors::Result<Envelope<VaultData>> {
    let chain_id = rpc.chain_id().await?;
    let block_number = rpc.block_number().await?;
    let block_tag = format!("0x{block_number:x}");

    let data = VaultData {
        address: format!("{vault:#x}"),
        ..Default::default()
    };
    let mut b = PartialBuilder::new(chain_id, block_number, data);

    // gateway.vault()
    match call_vault_addr(rpc, gateway, &block_tag).await {
        Ok(addr) => b.data_mut().gateway_vault = format!("{addr:#x}"),
        Err(e) => b.record_err("gateway_vault", e),
    }

    // vault.asset()
    match call_asset(rpc, vault, &block_tag).await {
        Ok(addr) => b.data_mut().asset = format!("{addr:#x}"),
        Err(e) => b.record_err("asset", e),
    }

    // vault.name() / vault.symbol() / vault.decimals()
    match call_string(rpc, vault, &block_tag, MockVault::nameCall {}).await {
        Ok(s) => b.data_mut().name = s,
        Err(e) => b.record_err("name", e),
    }
    match call_string(rpc, vault, &block_tag, MockVault::symbolCall {}).await {
        Ok(s) => b.data_mut().symbol = s,
        Err(e) => b.record_err("symbol", e),
    }
    match call_decimals(rpc, vault, &block_tag).await {
        Ok(d) => b.data_mut().decimals = d,
        Err(e) => b.record_err("decimals", e),
    }

    // accounting
    let total_assets =
        match call_u256_view(rpc, vault, &block_tag, MockVault::totalAssetsCall {}).await {
            Ok(v) => {
                b.data_mut().total_assets = DecimalU256(v);
                Some(v)
            }
            Err(e) => {
                b.record_err("total_assets", e);
                None
            }
        };
    let total_supply =
        match call_u256_view(rpc, vault, &block_tag, MockVault::totalSupplyCall {}).await {
            Ok(v) => {
                b.data_mut().total_supply = DecimalU256(v);
                Some(v)
            }
            Err(e) => {
                b.record_err("total_supply", e);
                None
            }
        };

    // share_price: only computable when both reads succeeded.
    if let (Some(ta), Some(ts)) = (total_assets, total_supply) {
        let decimals = b.data_mut().decimals;
        let price = compute_share_price(ta, ts, decimals);
        b.data_mut().share_price = price;
    }

    Ok(b.finish())
}

/// Registry-mode read: look up `vault` in the `VaultRegistry` at `registry`,
/// then augment with live ERC-4626 state. Returns `Err` when the registry
/// call fails (e.g. vault not registered) — callers map this to exit 3
/// per the §9 "unregistered address exits non-zero" AC.
async fn read_vault_from_registry(
    rpc: &FailoverRpcClient,
    registry: Address,
    vault: Address,
) -> crate::errors::Result<Envelope<RegistryVaultData>> {
    let chain_id = rpc.chain_id().await?;
    let block_number = rpc.block_number().await?;
    let block_tag = format!("0x{block_number:x}");

    // Fetch the registry record. This is a hard-fail: if the vault is not
    // registered the call reverts with NotRegistered and we propagate the
    // error so the caller exits non-zero.
    let record = call_get_vault(rpc, registry, vault, &block_tag).await?;

    let data = RegistryVaultData {
        address: format!("{vault:#x}"),
        name: record.metadata.name.clone(),
        status: status_to_str(record.status).to_string(),
        registered_at: record.metadata.registeredAt.saturating_to::<u64>(),
        ..Default::default()
    };
    let mut b = PartialBuilder::new(chain_id, block_number, data);

    // Live ERC-4626 state.
    match call_asset(rpc, vault, &block_tag).await {
        Ok(addr) => b.data_mut().asset = format!("{addr:#x}"),
        Err(e) => b.record_err("asset", e),
    }
    match call_decimals(rpc, vault, &block_tag).await {
        Ok(d) => b.data_mut().decimals = d,
        Err(e) => b.record_err("decimals", e),
    }
    let total_assets =
        match call_u256_view(rpc, vault, &block_tag, MockVault::totalAssetsCall {}).await {
            Ok(v) => {
                b.data_mut().total_assets = DecimalU256(v);
                Some(v)
            }
            Err(e) => {
                b.record_err("total_assets", e);
                None
            }
        };
    let total_supply =
        match call_u256_view(rpc, vault, &block_tag, MockVault::totalSupplyCall {}).await {
            Ok(v) => {
                b.data_mut().total_supply = DecimalU256(v);
                Some(v)
            }
            Err(e) => {
                b.record_err("total_supply", e);
                None
            }
        };
    if let (Some(ta), Some(ts)) = (total_assets, total_supply) {
        let decimals = b.data_mut().decimals;
        let price = compute_share_price(ta, ts, decimals);
        b.data_mut().share_price = price;
    }

    Ok(b.finish())
}

/// Call `VaultRegistry.getVault(address)` and return the decoded return value.
/// Returns `Err` if the call fails (including revert for unregistered vault).
///
/// `getVault` returns two top-level outputs — `(VaultMetadata metadata, uint8
/// status)` — so the decode yields a positional tuple (`getVaultReturn`), not
/// a single struct. Decoding it as a nine-field struct is the drift this
/// function was fixed for (issue #1362).
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

/// Compute `totalAssets * 10^decimals / totalSupply` as a decimal
/// string. Returns `None` when `totalSupply == 0` — the price is
/// undefined there and surfacing `null` is the §9-correct way to
/// signal "no answer" for a single-cell field. Saturates on overflow
/// of the multiplication (vanishingly unlikely at sane decimals, but
/// the saturating boundary is defined behavior, not a panic).
fn compute_share_price(total_assets: U256, total_supply: U256, decimals: u8) -> Option<String> {
    if total_supply.is_zero() {
        return None;
    }
    let scale = U256::from(10u64).pow(U256::from(decimals as u64));
    let numerator = total_assets.saturating_mul(scale);
    let price = numerator / total_supply;
    Some(price.to_string())
}

// ---- typed view helpers --------------------------------------------------

async fn call_vault_addr(
    rpc: &FailoverRpcClient,
    gateway: Address,
    block_tag: &str,
) -> std::result::Result<Address, String> {
    let data = RobotMoneyGateway::vaultCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: gateway,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await
        .map_err(|e| format!("eth_call failed: {e}"))?;
    let r = RobotMoneyGateway::vaultCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(r._0)
}

async fn call_asset(
    rpc: &FailoverRpcClient,
    vault: Address,
    block_tag: &str,
) -> std::result::Result<Address, String> {
    let data = MockVault::assetCall {}.abi_encode();
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
    let r = MockVault::assetCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(r._0)
}

async fn call_decimals(
    rpc: &FailoverRpcClient,
    vault: Address,
    block_tag: &str,
) -> std::result::Result<u8, String> {
    let data = MockVault::decimalsCall {}.abi_encode();
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
    let r = MockVault::decimalsCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(r._0)
}

/// Generic helper for `() -> string` views on the vault.
async fn call_string<C>(
    rpc: &FailoverRpcClient,
    vault: Address,
    block_tag: &str,
    call: C,
) -> std::result::Result<String, String>
where
    C: SolCall,
    C::Return: ReturnString,
{
    let data = call.abi_encode();
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
    let r = C::abi_decode_returns(&out, true).map_err(|e| format!("abi decode: {e}"))?;
    Ok(r.into_string())
}

trait ReturnString {
    fn into_string(self) -> String;
}
impl ReturnString for MockVault::nameReturn {
    fn into_string(self) -> String {
        self._0
    }
}
impl ReturnString for MockVault::symbolReturn {
    fn into_string(self) -> String {
        self._0
    }
}

/// Generic helper for `() -> uint256` views on the vault.
async fn call_u256_view<C>(
    rpc: &FailoverRpcClient,
    vault: Address,
    block_tag: &str,
    call: C,
) -> std::result::Result<U256, String>
where
    C: SolCall,
    C::Return: ReturnU256,
{
    let data = call.abi_encode();
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
    let r = C::abi_decode_returns(&out, true).map_err(|e| format!("abi decode: {e}"))?;
    Ok(r.into_u256())
}

trait ReturnU256 {
    fn into_u256(self) -> U256;
}
impl ReturnU256 for MockVault::totalAssetsReturn {
    fn into_u256(self) -> U256 {
        self._0
    }
}
impl ReturnU256 for MockVault::totalSupplyReturn {
    fn into_u256(self) -> U256 {
        self._0
    }
}

fn emit<T: Serialize>(out: &T, pretty: bool) {
    let json = if pretty {
        serde_json::to_string_pretty(out)
    } else {
        serde_json::to_string(out)
    }
    .expect("get-vault output serialises");
    println!("{json}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_price_zero_supply_yields_none() {
        assert!(compute_share_price(U256::from(1u64), U256::ZERO, 6).is_none());
    }

    #[test]
    fn share_price_one_to_one_with_six_decimals() {
        // total_assets = total_supply = 1_000_000 (1.0 with 6 decimals)
        // expected share_price = 10^6 (one share is worth one unit)
        let p = compute_share_price(U256::from(1_000_000u64), U256::from(1_000_000u64), 6).unwrap();
        assert_eq!(p, "1000000");
    }

    #[test]
    fn share_price_premium_above_par() {
        // total_assets = 2_000_000, supply = 1_000_000, decimals = 6
        // → 2 * 10^6 = 2000000
        let p = compute_share_price(U256::from(2_000_000u64), U256::from(1_000_000u64), 6).unwrap();
        assert_eq!(p, "2000000");
    }

    /// Migrated from suite-05 (`testing/fork-e2e-rust/tests/rmpc_get_vault_fork.rs`).
    ///
    /// A malformed `vault_address` in the operator config must cause `run()` to
    /// return `EXIT_STARTUP_FAIL` (3) immediately, without touching the network.
    /// This exercises the `Address::from_str` guard in `get_vault::run()` and
    /// requires no chain or fork fixture.
    #[test]
    fn rmpc_get_vault_rejects_malformed_address() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let keystore = tmp.path().join("keystore.json");
        let cfg_path = tmp.path().join("rmpc.toml");
        let toml = format!(
            r#"chain_id              = 8453
rpc_url               = "http://127.0.0.1:1"
gateway_address       = "0x000000000000000000000000000000000000dEaD"
usdc_address          = "0x{usdc}"
vault_address         = "not-a-hex-address"
gateway_runtime_hash  = "0x{zeros}"
max_fee_per_gas_cap   = 100000000000

[signer]
allow_software_fallback = true
keystore_path           = "{ks}"
"#,
            usdc = "00".repeat(20),
            zeros = "0".repeat(64),
            ks = keystore.display(),
        );
        std::fs::write(&cfg_path, &toml).expect("write rmpc.toml");

        // EXIT_STARTUP_FAIL = 3: the vault_address parse must fail before any
        // RPC attempt is made, so this test never touches the network.
        let code = run(&cfg_path, None, false);
        assert_eq!(
            code, EXIT_STARTUP_FAIL,
            "expected EXIT_STARTUP_FAIL ({EXIT_STARTUP_FAIL}) on malformed vault_address; got {code}"
        );
    }
}
