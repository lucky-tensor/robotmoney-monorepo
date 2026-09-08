//! Canonical: Plan tracking issue #109 §5.1 — Protocol-scope vault registry reads
//!
//! Integration tests for `rmpc get-vaults` and `rmpc get-vault --address <addr>`
//! (issue #297).

mod common;

use crate::common::{
    enc_address, enc_u256, jrpc_result, match_eth_call_selector, selector_hex_of, USDC, VAULT,
};
use alloy_primitives::{address, hex as ahex, Address, U256};
use assert_cmd::Command;
use mockito::Matcher;
use rust_payment_client::gateway::{MockVault, VaultRegistry};
use serde_json::{json, Value};
use std::path::PathBuf;
use tempfile::TempDir;

/// A registry address constant for tests.
const REGISTRY: Address = address!("0000000000000000000000000000000000000e00");

fn rmpc() -> Command {
    Command::cargo_bin("rmpc").expect("rmpc binary built")
}

/// Fixture that includes a `registry_address` in the TOML config.
struct RegistryFixture {
    _tmp: TempDir,
    pub config_path: PathBuf,
}

impl RegistryFixture {
    fn build(rpc_url: &str, chain_id: u64) -> Self {
        use crate::common::{GATEWAY, GATEWAY_CODE, TEST_PASSPHRASE};
        use alloy_primitives::keccak256;
        use rust_payment_client::signer::software::SoftwareSigner;

        let tmp = TempDir::new().expect("tempdir");
        let keystore_path = tmp.path().join("keystore.json");
        const TEST_PRIVKEY: [u8; 32] = [
            0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38,
            0xff, 0x94, 0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b,
            0xf4, 0xf2, 0xff, 0x80,
        ];
        SoftwareSigner::create_keystore(&keystore_path, &TEST_PRIVKEY, TEST_PASSPHRASE)
            .expect("create keystore");

        let runtime_hash = format!("0x{}", ahex::encode(keccak256(GATEWAY_CODE)));
        let config_path = tmp.path().join("rmpc.toml");
        let toml = format!(
            r#"chain_id              = {chain_id}
rpc_url               = "{rpc_url}"
gateway_address       = "{GATEWAY:#x}"
usdc_address          = "{USDC:#x}"
vault_address         = "{VAULT:#x}"
registry_address      = "{REGISTRY:#x}"
gateway_runtime_hash  = "{runtime_hash}"
max_fee_per_gas_cap   = 100000000000

[signer]
allow_software_fallback = true
keystore_path           = "{}"
"#,
            keystore_path.display(),
        );
        std::fs::write(&config_path, toml).expect("write config");
        Self {
            _tmp: tmp,
            config_path,
        }
    }
}

/// ABI-encode a realistic `VaultRegistry.getVault` return value.
///
/// **Deliberately does not go through `VaultRegistry::getVaultCall`** (issue
/// #1362). The old version of this helper encoded through the very binding the
/// command decodes with, so it round-tripped the client's own assumption: the
/// committed ABI declared a nine-field `VaultRecord`, the mock encoded a
/// nine-field `VaultRecord`, and the test stayed green while `rmpc get-vaults`
/// could not decode a single real registry response.
///
/// The layout below is taken from `contracts/VaultRegistry.sol` directly —
///
/// ```solidity
/// function getVault(address vault)
///     external view returns (VaultMetadata memory metadata, VaultStatus status);
/// struct VaultMetadata { string name; address asset; uint256 registeredAt; }
/// ```
///
/// — i.e. **two** top-level outputs, `((string,address,uint256),uint8)`, encoded
/// as return params. Nothing here reads `abi/VaultRegistry.json`, so these
/// tests fail if that file drifts from the contract again.
fn enc_vault_record(name: &str, asset: Address, registered_at: u64, status: u8) -> String {
    use alloy_sol_types::{sol_data, SolType};

    /// `struct VaultMetadata { string name; address asset; uint256 registeredAt; }`
    type VaultMetadataAbi = (sol_data::String, sol_data::Address, sol_data::Uint<256>);
    /// `returns (VaultMetadata metadata, VaultStatus status)` — two outputs.
    type GetVaultReturnAbi = (VaultMetadataAbi, sol_data::Uint<8>);

    let blob = GetVaultReturnAbi::abi_encode_params(&(
        (name.to_string(), asset, U256::from(registered_at)),
        status,
    ));
    format!("0x{}", ahex::encode(blob))
}

/// ABI-encode a `listVaults` return value (address[]).
fn enc_address_array(addrs: &[Address]) -> String {
    use alloy_sol_types::SolCall;
    let blob = VaultRegistry::listVaultsCall::abi_encode_returns(&(addrs.to_vec(),));
    format!("0x{}", ahex::encode(blob))
}

// ---- get-vaults tests -------------------------------------------------------

/// Empty registry: listVaults() returns [], command exits 0, vaults is [].
#[tokio::test]
async fn get_vaults_empty_registry() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    let block_no = 0x10u64;

    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_chainId"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{chain_id:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{block_no:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    // listVaults() → []
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            VaultRegistry::listVaultsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_address_array(&[])))
        .expect_at_least(0)
        .create_async()
        .await;

    let fix = RegistryFixture::build(&server.url(), chain_id);
    let out = rmpc()
        .args(["get-vaults", "--config", fix.config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();

    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["chain_id"], chain_id);
    assert_eq!(v["block_number"], block_no);
    assert_eq!(v["source"], "json_rpc");
    assert_eq!(v["partial"], false);
    let vaults = v["data"]["vaults"].as_array().unwrap();
    assert!(vaults.is_empty(), "expected empty vaults array: {v}");
}

/// One registered active vault: outputs a vaults array with correct fields.
#[tokio::test]
async fn get_vaults_one_registered_vault() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    let block_no = 0x20u64;
    let total_assets = U256::from(5_000_000u64);

    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_chainId"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{chain_id:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{block_no:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    // listVaults() → [VAULT]
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            VaultRegistry::listVaultsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_address_array(&[VAULT])))
        .expect_at_least(0)
        .create_async()
        .await;
    // getVault(VAULT) → VaultRecord
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            VaultRegistry::getVaultCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_vault_record(
            "RobotMoney USDC Vault",
            USDC,
            1_700_000_000,
            0, // Active
        )))
        .expect_at_least(0)
        .create_async()
        .await;
    // vault.totalAssets()
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalAssetsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(total_assets)))
        .expect_at_least(0)
        .create_async()
        .await;

    let fix = RegistryFixture::build(&server.url(), chain_id);
    let out = rmpc()
        .args(["get-vaults", "--config", fix.config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();

    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["partial"], false);
    let vaults = v["data"]["vaults"].as_array().unwrap();
    assert_eq!(vaults.len(), 1, "expected one vault: {v}");
    let vault = &vaults[0];
    assert_eq!(
        vault["address"].as_str().unwrap().to_lowercase(),
        format!("{VAULT:#x}")
    );
    assert_eq!(vault["name"], "RobotMoney USDC Vault");
    assert_eq!(
        vault["asset"].as_str().unwrap().to_lowercase(),
        format!("{USDC:#x}")
    );
    assert_eq!(vault["status"], "active");
    assert_eq!(vault["registered_at"], 1_700_000_000u64);
    assert_eq!(vault["total_assets"].as_str().unwrap(), "5000000");
}

/// Paused vault: status field in output is "paused".
#[tokio::test]
async fn get_vaults_paused_vault_status() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    let block_no = 0x30u64;

    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_chainId"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{chain_id:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{block_no:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            VaultRegistry::listVaultsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_address_array(&[VAULT])))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            VaultRegistry::getVaultCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_vault_record(
            "Paused Vault",
            USDC,
            1_700_000_000,
            1, // Paused
        )))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalAssetsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(U256::ZERO)))
        .expect_at_least(0)
        .create_async()
        .await;

    let fix = RegistryFixture::build(&server.url(), chain_id);
    let out = rmpc()
        .args(["get-vaults", "--config", fix.config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();

    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let vaults = v["data"]["vaults"].as_array().unwrap();
    assert_eq!(vaults[0]["status"], "paused");
}

// ---- get-vault --address tests ----------------------------------------------

/// Happy path: get-vault --address reads registry + live ERC-4626 state.
#[tokio::test]
async fn get_vault_address_happy_path() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    let block_no = 0x40u64;
    let total_assets = U256::from(2_000_000u64);
    let total_supply = U256::from(1_000_000u64);

    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_chainId"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{chain_id:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{block_no:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    // getVault(VAULT) — registry call
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            VaultRegistry::getVaultCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_vault_record(
            "Robot Money Vault",
            USDC,
            1_715_000_000,
            0, // Active
        )))
        .expect_at_least(0)
        .create_async()
        .await;
    // vault.asset()
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::assetCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_address(USDC)))
        .expect_at_least(0)
        .create_async()
        .await;
    // vault.decimals() = 6
    let mut w = [0u8; 32];
    w[31] = 6u8;
    let dec_hex = format!("0x{}", ahex::encode(w));
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::decimalsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&dec_hex))
        .expect_at_least(0)
        .create_async()
        .await;
    // vault.totalAssets()
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalAssetsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(total_assets)))
        .expect_at_least(0)
        .create_async()
        .await;
    // vault.totalSupply()
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalSupplyCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(total_supply)))
        .expect_at_least(0)
        .create_async()
        .await;

    let fix = RegistryFixture::build(&server.url(), chain_id);
    let out = rmpc()
        .args([
            "get-vault",
            "--config",
            fix.config_path.to_str().unwrap(),
            "--address",
            &format!("{VAULT:#x}"),
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["chain_id"], chain_id);
    assert_eq!(v["block_number"], block_no);
    assert_eq!(v["source"], "json_rpc");
    assert_eq!(v["partial"], false);

    let d = &v["data"];
    assert_eq!(
        d["address"].as_str().unwrap().to_lowercase(),
        format!("{VAULT:#x}")
    );
    assert_eq!(d["name"], "Robot Money Vault");
    assert_eq!(d["status"], "active");
    assert_eq!(d["registered_at"], 1_715_000_000u64);
    assert_eq!(d["total_assets"].as_str().unwrap(), "2000000");
    assert_eq!(d["total_supply"].as_str().unwrap(), "1000000");
    // share_price = 2_000_000 * 10^6 / 1_000_000 = 2_000_000
    assert_eq!(d["share_price"].as_str().unwrap(), "2000000");
    assert_eq!(
        d["asset"].as_str().unwrap().to_lowercase(),
        format!("{USDC:#x}")
    );
    assert_eq!(d["decimals"], 6);
}

/// Unregistered address: getVault reverts → command exits non-zero.
#[tokio::test]
async fn get_vault_address_unregistered_exits_nonzero() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    let block_no = 0x50u64;

    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_chainId"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{chain_id:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{block_no:x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    // getVault reverts (the contract's error is NotRegistered())
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            VaultRegistry::getVaultCall,
        >()))
        .with_status(200)
        .with_body(r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted"}}"#)
        .expect_at_least(0)
        .create_async()
        .await;

    let fix = RegistryFixture::build(&server.url(), chain_id);
    let unregistered = "0x0000000000000000000000000000000000001234";
    rmpc()
        .args([
            "get-vault",
            "--config",
            fix.config_path.to_str().unwrap(),
            "--address",
            unregistered,
        ])
        .assert()
        .failure();
}

/// get-vaults without registry_address in config exits non-zero.
#[test]
fn get_vaults_without_registry_address_fails() {
    // Use a Fixture (no registry_address) rather than RegistryFixture.
    // We don't even need a mock server — the command must fail before any RPC.
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("rmpc.toml");
    let toml = format!(
        r#"chain_id              = 31337
rpc_url               = "http://127.0.0.1:1"
gateway_address       = "0x{gw}"
usdc_address          = "0x{usdc}"
vault_address         = "0x{vault}"
gateway_runtime_hash  = "0x{zeros}"
max_fee_per_gas_cap   = 100000000000

[signer]
allow_software_fallback = true
keystore_path           = "{ks}"
"#,
        gw = "00".repeat(20),
        usdc = "00".repeat(20),
        vault = "00".repeat(20),
        zeros = "0".repeat(64),
        ks = tmp.path().join("ks.json").display(),
    );
    std::fs::write(&cfg_path, &toml).expect("write config");
    rmpc()
        .args(["get-vaults", "--config", cfg_path.to_str().unwrap()])
        .assert()
        .failure();
}

/// get-vault --address without registry_address in config exits non-zero.
#[test]
fn get_vault_address_without_registry_address_fails() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("rmpc.toml");
    let toml = format!(
        r#"chain_id              = 31337
rpc_url               = "http://127.0.0.1:1"
gateway_address       = "0x{gw}"
usdc_address          = "0x{usdc}"
vault_address         = "0x{vault}"
gateway_runtime_hash  = "0x{zeros}"
max_fee_per_gas_cap   = 100000000000

[signer]
allow_software_fallback = true
keystore_path           = "{ks}"
"#,
        gw = "00".repeat(20),
        usdc = "00".repeat(20),
        vault = "00".repeat(20),
        zeros = "0".repeat(64),
        ks = tmp.path().join("ks.json").display(),
    );
    std::fs::write(&cfg_path, &toml).expect("write config");
    rmpc()
        .args([
            "get-vault",
            "--config",
            cfg_path.to_str().unwrap(),
            "--address",
            "0x0000000000000000000000000000000000001234",
        ])
        .assert()
        .failure();
}
