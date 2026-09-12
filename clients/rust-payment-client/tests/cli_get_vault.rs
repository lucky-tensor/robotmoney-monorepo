//! Canonical: Plan tracking issue #109 §9 — `rmpc get-vault`
//!
//! Integration tests for `rmpc get-vault` (issue #49).

mod common;

use crate::common::{
    enc_address, enc_u256, jrpc_result, match_eth_call_selector, selector_hex_of, Fixture, USDC,
    VAULT,
};
use alloy_primitives::{hex as ahex, U256};
use alloy_sol_types::SolCall;
use assert_cmd::Command;
use mockito::Matcher;
use rust_payment_client::gateway::{MockVault, RobotMoneyGateway};
use serde_json::{json, Value};

fn rmpc() -> Command {
    Command::cargo_bin("rmpc").expect("rmpc binary built")
}

fn enc_string_returns<C: SolCall>(s: &str) -> String {
    // Use sol_types' abi_encode on the *Return* via a synthetic struct.
    // The simpler approach: hand-roll the dynamic-string ABI return —
    // 32-byte offset (0x20), 32-byte length, padded data.
    let bytes = s.as_bytes();
    let mut blob = Vec::new();
    let mut offset = [0u8; 32];
    offset[31] = 0x20;
    blob.extend_from_slice(&offset);
    let mut length = [0u8; 32];
    let len_bytes = (bytes.len() as u64).to_be_bytes();
    length[24..].copy_from_slice(&len_bytes);
    blob.extend_from_slice(&length);
    blob.extend_from_slice(bytes);
    // pad to 32-byte multiple
    let pad = (32 - (bytes.len() % 32)) % 32;
    blob.extend(std::iter::repeat_n(0u8, pad));
    let _ = C::SELECTOR; // satisfy generic
    format!("0x{}", ahex::encode(blob))
}

fn enc_u8(n: u8) -> String {
    let mut w = [0u8; 32];
    w[31] = n;
    format!("0x{}", ahex::encode(w))
}

#[tokio::test]
async fn get_vault_clean_envelope_with_share_price() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    let block_no = 0x55u64;
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

    // gateway.vault()
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            RobotMoneyGateway::vaultCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_address(VAULT)))
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
    // vault.name()
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::nameCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_string_returns::<MockVault::nameCall>(
            "Robot Money Vault",
        )))
        .expect_at_least(0)
        .create_async()
        .await;
    // vault.symbol()
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::symbolCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_string_returns::<MockVault::symbolCall>(
            "rmUSDC",
        )))
        .expect_at_least(0)
        .create_async()
        .await;
    // vault.decimals() = 6
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::decimalsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u8(6)))
        .expect_at_least(0)
        .create_async()
        .await;
    // vault.totalAssets() = 2_000_000
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalAssetsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(U256::from(2_000_000u64))))
        .expect_at_least(0)
        .create_async()
        .await;
    // vault.totalSupply() = 1_000_000 → share_price = 2 * 10^6 = 2000000
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalSupplyCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(U256::from(1_000_000u64))))
        .expect_at_least(0)
        .create_async()
        .await;

    let fix = Fixture::build(&server.url(), chain_id);
    let out = rmpc()
        .args(["get-vault", "--config", fix.config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();

    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["chain_id"], chain_id);
    assert_eq!(v["block_number"], block_no);
    assert_eq!(v["source"], "json_rpc");
    assert_eq!(v["network_env"], "local_devnet");
    assert_eq!(v["partial"], false);

    let d = &v["data"];
    assert_eq!(
        d["address"].as_str().unwrap().to_lowercase(),
        format!("{VAULT:#x}")
    );
    assert_eq!(
        d["gateway_vault"].as_str().unwrap().to_lowercase(),
        format!("{VAULT:#x}")
    );
    assert_eq!(
        d["asset"].as_str().unwrap().to_lowercase(),
        format!("{USDC:#x}")
    );
    assert_eq!(d["name"], "Robot Money Vault");
    assert_eq!(d["symbol"], "rmUSDC");
    assert_eq!(d["decimals"], 6);
    assert_eq!(d["total_assets"], "2000000");
    assert_eq!(d["total_supply"], "1000000");
    assert_eq!(d["share_price"], "2000000");

    // §9 explicit not_onchain markers
    let notes = &d["notes"];
    assert_eq!(notes["deposit_cap"], "not_onchain");
    assert_eq!(notes["paused"], "not_onchain");
    assert_eq!(notes["shutdown"], "not_onchain");
    assert_eq!(notes["adapters"], "not_onchain");
    assert_eq!(notes["fees"], "not_onchain");
}

#[tokio::test]
async fn get_vault_partial_when_total_assets_reverts() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    let block_no = 1u64;
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
    // gateway.vault, asset, name, symbol, decimals — all OK.
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            RobotMoneyGateway::vaultCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_address(VAULT)))
        .expect_at_least(0)
        .create_async()
        .await;
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
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::nameCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_string_returns::<MockVault::nameCall>("V")))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::symbolCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_string_returns::<MockVault::symbolCall>(
            "V",
        )))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::decimalsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u8(6)))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalAssetsCall,
        >()))
        .with_status(200)
        .with_body(r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted"}}"#)
        .expect_at_least(0)
        .create_async()
        .await;
    // totalSupply remains readable.
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalSupplyCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(U256::from(1u64))))
        .expect_at_least(0)
        .create_async()
        .await;

    let fix = Fixture::build(&server.url(), chain_id);
    let out = rmpc()
        .args(["get-vault", "--config", fix.config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["partial"], true);
    let errs = v["errors"].as_array().unwrap();
    assert!(errs.iter().any(|e| e["field"] == "total_assets"));
    // A failed totalAssets read is null, not the valid zero TVL value.
    assert!(v["data"]["total_assets"].is_null());
    // share_price uncomputable when total_assets read failed
    assert!(v["data"]["share_price"].is_null());
}

/// Sibling of [`get_vault_partial_when_total_assets_reverts`] for the *other*
/// accounting sub-read (issue #1427).
///
/// `read_vault` records `total_assets` and `total_supply` failures in two
/// independent `record_err` arms. PR #1404 repurposed the original
/// `total_supply` test into the `total_assets` one above, leaving the
/// `record_err("total_supply")` arm untested — a regression there would have
/// shipped green. This restores the branch's own coverage: totalSupply reverts
/// while every sibling read succeeds.
#[tokio::test]
async fn get_vault_partial_when_total_supply_reverts() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    let block_no = 1u64;
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
    // gateway.vault, asset, name, symbol, decimals — all OK.
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            RobotMoneyGateway::vaultCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_address(VAULT)))
        .expect_at_least(0)
        .create_async()
        .await;
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
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::nameCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_string_returns::<MockVault::nameCall>("V")))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::symbolCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_string_returns::<MockVault::symbolCall>(
            "V",
        )))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::decimalsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u8(6)))
        .expect_at_least(0)
        .create_async()
        .await;
    // totalAssets remains readable.
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalAssetsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(U256::from(2_000_000u64))))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            MockVault::totalSupplyCall,
        >()))
        .with_status(200)
        .with_body(r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted"}}"#)
        .expect_at_least(0)
        .create_async()
        .await;

    let fix = Fixture::build(&server.url(), chain_id);
    let out = rmpc()
        .args(["get-vault", "--config", fix.config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["partial"], true);
    let errs = v["errors"].as_array().unwrap();
    assert!(
        errs.iter().any(|e| e["field"] == "total_supply"),
        "expected a total_supply entry in errors[], got {errs:?}"
    );
    // Only the reverting sub-read is reported — the sibling accounting read
    // succeeded and must not be tarred with it.
    assert!(
        !errs.iter().any(|e| e["field"] == "total_assets"),
        "total_assets read succeeded and must not appear in errors[], got {errs:?}"
    );

    // Sibling reads that succeeded still carry their values.
    let d = &v["data"];
    assert_eq!(d["total_assets"], "2000000");
    assert_eq!(
        d["gateway_vault"].as_str().unwrap().to_lowercase(),
        format!("{VAULT:#x}")
    );
    assert_eq!(
        d["asset"].as_str().unwrap().to_lowercase(),
        format!("{USDC:#x}")
    );
    assert_eq!(d["name"], "V");
    assert_eq!(d["symbol"], "V");
    assert_eq!(d["decimals"], 6);

    // share_price uncomputable when the total_supply read failed.
    assert!(d["share_price"].is_null());
}
