//! Canonical: none — shared fixtures for rmpc integration tests
//!
//! Shared test fixtures for the `rmpc` CLI integration tests.
//!
//! These helpers build a temp keystore + temp config file pointing at a
//! mockito JSON-RPC server, and set up the full happy-path response set.
//! Individual tests override the mocks they care about.

#![allow(dead_code)] // each integration target only uses a subset

use alloy_primitives::{address, hex as ahex, keccak256, Address, U256};
use alloy_sol_types::SolCall;
use mockito::Matcher;
use rust_payment_client::gateway::{Erc20, RobotMoneyGateway};
use rust_payment_client::signer::software::SoftwareSigner;
use serde_json::json;
use std::path::PathBuf;
use tempfile::TempDir;

pub const SIGNER_ADDRESS: Address = address!("f39fd6e51aad88f6f4ce6ab8827279cfffb92266");
pub const GATEWAY: Address = address!("0000000000000000000000000000000000000b00");
pub const USDC: Address = address!("0000000000000000000000000000000000000c00");
pub const VAULT: Address = address!("0000000000000000000000000000000000000d00");
pub const SHARE_RECEIVER: Address = address!("00000000000000000000000000000000000000ee");
/// Where a redemption's USDC lands, per the agent policy.
pub const ASSET_RECIPIENT: Address = address!("00000000000000000000000000000000000000ef");

/// Canned gateway runtime bytecode. The preflight only cares about its
/// keccak256, so any non-empty blob works.
pub const GATEWAY_CODE: &[u8] = &[0x60, 0x80, 0x60, 0x40, 0x52, 0xfe, 0xfe, 0xfe];

/// 32-byte private key whose corresponding address matches [`SIGNER_ADDRESS`]
/// (anvil account #0 — `0xac0974…ff80`, address
/// `0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266`). Used purely as a
/// deterministic test fixture.
const TEST_PRIVKEY: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];

pub const TEST_PASSPHRASE: &[u8] = b"correct horse battery staple";

/// A fully wired test environment.
pub struct Fixture {
    /// Owns the temp dir; drop-order matters (TempDir last).
    _tmp: TempDir,
    pub keystore_path: PathBuf,
    pub config_path: PathBuf,
}

impl Fixture {
    /// Build a temp keystore + config TOML pointing at `rpc_url`, with
    /// `chain_id` baked in.
    pub fn build(rpc_url: &str, chain_id: u64) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let keystore_path = tmp.path().join("keystore.json");
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
            keystore_path,
            config_path,
        }
    }
}

// --- ABI return helpers ----------------------------------------------------

pub fn enc_bool(v: bool) -> String {
    let mut w = [0u8; 32];
    w[31] = if v { 1 } else { 0 };
    format!("0x{}", ahex::encode(w))
}

pub fn enc_address(a: Address) -> String {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    format!("0x{}", ahex::encode(w))
}

pub fn enc_u256(v: U256) -> String {
    format!("0x{}", ahex::encode(v.to_be_bytes::<32>()))
}

pub fn enc_agents(
    active: bool,
    valid_until: u64,
    max_per_payment: U256,
    max_per_window: U256,
    share_receiver: Address,
) -> String {
    enc_agents_with_withdrawal(
        active,
        valid_until,
        max_per_payment,
        max_per_window,
        share_receiver,
        Address::ZERO,
        U256::ZERO,
        U256::ZERO,
    )
}

/// `enc_agents` with the withdrawal fields exposed. Used by tests that
/// exercise the agent-key compromise blast radius surfacing introduced
/// in issue #429 (asset_recipient, maxWithdrawPerPayment,
/// maxWithdrawPerWindow non-zero).
#[allow(dead_code, clippy::too_many_arguments)]
pub fn enc_agents_with_withdrawal(
    active: bool,
    valid_until: u64,
    max_per_payment: U256,
    max_per_window: U256,
    share_receiver: Address,
    asset_recipient: Address,
    max_withdraw_per_payment: U256,
    max_withdraw_per_window: U256,
) -> String {
    let mut blob = Vec::with_capacity(32 * 8);
    let mut w = [0u8; 32];
    w[31] = if active { 1 } else { 0 };
    blob.extend_from_slice(&w);
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&valid_until.to_be_bytes());
    blob.extend_from_slice(&w);
    blob.extend_from_slice(&max_per_payment.to_be_bytes::<32>());
    blob.extend_from_slice(&max_per_window.to_be_bytes::<32>());
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(share_receiver.as_slice());
    blob.extend_from_slice(&w);
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(asset_recipient.as_slice());
    blob.extend_from_slice(&w);
    blob.extend_from_slice(&max_withdraw_per_payment.to_be_bytes::<32>());
    blob.extend_from_slice(&max_withdraw_per_window.to_be_bytes::<32>());
    format!("0x{}", ahex::encode(blob))
}

pub fn jrpc_result(s: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{s}"}}"#)
}

pub fn jrpc_result_raw(json: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":{json}}}"#)
}

pub fn selector_hex_of<C: SolCall>() -> String {
    format!("0x{}", ahex::encode(C::SELECTOR))
}

pub fn match_eth_call_selector(selector: &str) -> Matcher {
    let prefix = selector.to_string();
    Matcher::AllOf(vec![
        Matcher::PartialJson(json!({"method": "eth_call"})),
        Matcher::Regex(format!(r#""data":"{prefix}"#)),
    ])
}

/// Match an `eth_call` whose `data` field begins with `selector` (0x-prefixed)
/// followed immediately by `first_arg` (an ABI-padded 32-byte word, hex-encoded
/// without `0x`). Used to verify the exact `owner` address in
/// `vault.allowance(owner, gateway)` calls (AZ-GW-2 / issue #1069).
///
/// The JSON body encodes the calldata as `"data":"0x<selector_bytes><arg_bytes>…"`.
/// This matcher ensures the method is `eth_call` and that the data starts with the
/// exact selector + first argument bytes.
pub fn match_eth_call_selector_with_first_arg(selector: &str, first_arg_hex: &str) -> Matcher {
    // The JSON field is `"data":"0x<hex>"`. Strip 0x from selector to get raw bytes.
    let sel = selector.strip_prefix("0x").unwrap_or(selector);
    let arg = first_arg_hex.strip_prefix("0x").unwrap_or(first_arg_hex);
    Matcher::AllOf(vec![
        Matcher::PartialJson(json!({"method": "eth_call"})),
        Matcher::Regex(format!(r#""data":"0x{sel}{arg}"#)),
    ])
}

/// Encode an `Address` as a 32-byte ABI word (zero-padded on the left),
/// returned as a lowercase hex string without the `0x` prefix.
pub fn abi_addr_hex(a: Address) -> String {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    ahex::encode(w)
}

/// Wire the full happy-path JSON-RPC response set for the preflight.
/// `agent_address` controls which address the `agents()` view appears to
/// be keyed on — all callers happen to use the same software-signer
/// address, but the parameter is left explicit.
pub async fn install_happy_path_mocks(
    server: &mut mockito::ServerGuard,
    chain_id: u64,
    _agent_address: Address,
) {
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
        .match_body(Matcher::PartialJson(json!({"method": "eth_getCode"})))
        .with_status(200)
        .with_body(jrpc_result(&format!("0x{}", ahex::encode(GATEWAY_CODE))))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            RobotMoneyGateway::pausedCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_bool(false)))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            RobotMoneyGateway::usdcCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_address(USDC)))
        .expect_at_least(0)
        .create_async()
        .await;
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
            RobotMoneyGateway::agentsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_agents(
            true,
            u64::MAX,
            U256::from(1_000_000u64),
            U256::from(100_000_000u64),
            SHARE_RECEIVER,
        )))
        .expect_at_least(0)
        .create_async()
        .await;
    // effectiveDepositWindowGross() = 0 (issue #497 — rolling deposit window)
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            RobotMoneyGateway::effectiveDepositWindowGrossCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(U256::ZERO)))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            Erc20::allowanceCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(U256::from(u128::MAX))))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            Erc20::balanceOfCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(U256::from(u128::MAX))))
        .expect_at_least(0)
        .create_async()
        .await;

    // Deadline uses rpc.block_number() + rpc.block_timestamp()
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
        .with_status(200)
        .with_body(jrpc_result("0x100"))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(
            json!({"method": "eth_getBlockByNumber"}),
        ))
        .with_status(200)
        .with_body(jrpc_result_raw(r#"{"timestamp":"0x64a9f4c0"}"#))
        .expect_at_least(0)
        .create_async()
        .await;
}

/// Wire the withdraw-path preflight reads on top of
/// [`install_happy_path_mocks`].
///
/// **Call this AFTER `install_happy_path_mocks`.** Both mock sets are
/// registered with `expect_at_least(0)`, which mockito never treats as
/// "missing hits", so it falls through to the *last* registered matching
/// mock. Registering this set first would leave the generic `agents()`
/// mock — which reports zero withdrawal caps — serving every request, and
/// every withdrawal would refuse before reaching the path under test.
/// A per-test override registered with the default expectation still wins
/// its first hit ahead of both sets.
///
/// The three vault reads the withdraw preflight makes are already covered
/// by `install_happy_path_mocks`: `vault.paused()` shares its selector
/// with `gateway.paused()` (false), and the share allowance/balance reads
/// are the same ERC-20 selectors it stubs at `u128::MAX`.
pub async fn install_withdraw_preflight_mocks(
    server: &mut mockito::ServerGuard,
    max_withdraw_per_payment: U256,
    max_withdraw_per_window: U256,
    withdraw_window_gross: U256,
) {
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            RobotMoneyGateway::agentsCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_agents_with_withdrawal(
            true,
            u64::MAX,
            U256::from(1_000_000u64),
            U256::from(100_000_000u64),
            SHARE_RECEIVER,
            ASSET_RECIPIENT,
            max_withdraw_per_payment,
            max_withdraw_per_window,
        )))
        .expect_at_least(0)
        .create_async()
        .await;
    // effectiveWithdrawWindowGross(agent) — issue #449's rolling window.
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            RobotMoneyGateway::effectiveWithdrawWindowGrossCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(withdraw_window_gross)))
        .expect_at_least(0)
        .create_async()
        .await;
}
