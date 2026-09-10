//! Canonical: none — integration tests for `rmpc withdraw-router`
//!
//! End-to-end coverage for `rmpc withdraw-router` (issue #1285).
//!
//! Like `withdraw`, this command had no end-to-end coverage: 465 of
//! `withdraw`'s 577 lines appeared verbatim inside it, and none of them
//! were exercised through `run()`. This suite mirrors `cli_withdraw.rs`,
//! plus the two things that are router-specific: the identity-bound
//! `vaults[i]` / `sharesPerLeg[i]` pairing (issue #967) and the explicit
//! `--confirm` gate.
//!
//! CI: run by `suite-07-rmpc-integration.yml`'s `rmpc-parity` job, which
//! names this binary explicitly with `--test cli_withdraw_router`.
//! `cargo test --lib` (suite 6) does not build it.

mod common;

use crate::common::{
    enc_bool, enc_u256, install_happy_path_mocks, install_withdraw_preflight_mocks, jrpc_result,
    jrpc_result_raw, match_eth_call_selector, selector_hex_of, Fixture, ASSET_RECIPIENT, GATEWAY,
    SIGNER_ADDRESS, TEST_PASSPHRASE, VAULT,
};
use alloy_primitives::{address, b256, hex as ahex, Address, Bytes, LogData, B256, U256};
use alloy_sol_types::SolEvent;
use assert_cmd::Command;
use mockito::Matcher;
use rust_payment_client::gateway::{Erc20, RobotMoneyGateway};
use rust_payment_client::signer::software::PASSPHRASE_ENV_VAR;
use serde_json::{json, Value};

fn rmpc() -> Command {
    Command::cargo_bin("rmpc").expect("rmpc binary built")
}

const ORDER_ID: B256 = b256!("5555555555555555555555555555555555555555555555555555555555555555");
const IDEMPOTENCY_KEY: B256 =
    b256!("6666666666666666666666666666666666666666666666666666666666666666");
const PAYMENT_ID: B256 = b256!("7777777777777777777777777777777777777777777777777777777777777777");
const TX_HASH: B256 = b256!("8888888888888888888888888888888888888888888888888888888888888888");
const ROUTER: Address = address!("00000000000000000000000000000000000000f0");
/// Second leg's vault. `VAULT` is the first.
const VAULT_B: Address = address!("0000000000000000000000000000000000000d01");

const LEG_A: u64 = 300_000;
const LEG_B: u64 = 200_000;

fn fee_history_body() -> String {
    r#"{
        "oldestBlock":"0x1",
        "baseFeePerGas":["0x3b9aca00","0x3b9aca00","0x3b9aca00","0x3b9aca00","0x3b9aca00","0x3b9aca00"],
        "gasUsedRatio":[0.5,0.5,0.5,0.5,0.5],
        "reward":[["0x3b9aca00"],["0x3b9aca00"],["0x3b9aca00"],["0x3b9aca00"],["0x3b9aca00"]]
    }"#
    .to_string()
}

/// A receipt carrying the `AgentWithdrawalRouted` log.
fn receipt_with_routed_withdrawal_body() -> String {
    let ev = RobotMoneyGateway::AgentWithdrawalRouted {
        paymentId: PAYMENT_ID,
        orderId: ORDER_ID,
        agent: SIGNER_ADDRESS,
        router: ROUTER,
        shareHolder: SIGNER_ADDRESS,
        sharesPerLeg: vec![U256::from(LEG_A), U256::from(LEG_B)],
        assetsPerLeg: vec![U256::from(LEG_A - 1_000), U256::from(LEG_B - 1_000)],
        assetRecipient: ASSET_RECIPIENT,
        windowId: 42u64,
    };
    let topics = ev.encode_topics();
    let data: Vec<u8> = ev.encode_data();
    let log = LogData::new_unchecked(
        topics.iter().map(|t| B256::from(t.0)).collect(),
        Bytes::from(data),
    );
    let topics_hex: Vec<String> = log
        .topics()
        .iter()
        .map(|t| format!("\"0x{}\"", ahex::encode(t.as_slice())))
        .collect();
    let data_hex = format!("0x{}", ahex::encode(log.data.as_ref()));

    format!(
        r#"{{
            "transactionHash":"{TX_HASH:#x}",
            "transactionIndex":"0x0",
            "blockHash":"0x0000000000000000000000000000000000000000000000000000000000000001",
            "blockNumber":"0x42",
            "from":"{SIGNER_ADDRESS:#x}",
            "to":"{GATEWAY:#x}",
            "cumulativeGasUsed":"0x5208",
            "gasUsed":"0x5208",
            "contractAddress":null,
            "logs":[{{
                "address":"{GATEWAY:#x}",
                "topics":[{topics}],
                "data":"{data_hex}",
                "blockHash":"0x0000000000000000000000000000000000000000000000000000000000000001",
                "blockNumber":"0x42",
                "transactionHash":"{TX_HASH:#x}",
                "transactionIndex":"0x0",
                "logIndex":"0x0",
                "removed":false
            }}],
            "status":"0x1",
            "logsBloom":"0x{bloom}",
            "type":"0x2",
            "effectiveGasPrice":"0x3b9aca00"
        }}"#,
        topics = topics_hex.join(","),
        bloom = "00".repeat(256),
    )
}

fn reverted_receipt_body() -> String {
    format!(
        r#"{{
            "transactionHash":"{TX_HASH:#x}",
            "transactionIndex":"0x0",
            "blockHash":"0x0000000000000000000000000000000000000000000000000000000000000001",
            "blockNumber":"0x42",
            "from":"{SIGNER_ADDRESS:#x}",
            "to":"{GATEWAY:#x}",
            "cumulativeGasUsed":"0x5208",
            "gasUsed":"0x5208",
            "contractAddress":null,
            "logs":[],
            "status":"0x0",
            "logsBloom":"0x{bloom}",
            "type":"0x2",
            "effectiveGasPrice":"0x3b9aca00"
        }}"#,
        bloom = "00".repeat(256),
    )
}

async fn install_post_preflight_mocks(server: &mut mockito::ServerGuard, receipt_body: &str) {
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_feeHistory"})))
        .with_status(200)
        .with_body(jrpc_result_raw(&fee_history_body()))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(
            json!({"method": "eth_getTransactionCount"}),
        ))
        .with_status(200)
        .with_body(jrpc_result("0x7"))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(
            json!({"method": "eth_sendRawTransaction"}),
        ))
        .with_status(200)
        .with_body(jrpc_result(&format!("{TX_HASH:#x}")))
        .expect_at_least(0)
        .create_async()
        .await;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(
            json!({"method": "eth_getTransactionReceipt"}),
        ))
        .with_status(200)
        .with_body(jrpc_result_raw(receipt_body))
        .expect_at_least(0)
        .create_async()
        .await;
}

/// The full happy-path mock set. The withdraw-specific preflight reads
/// are registered AFTER the shared set so they win — see
/// `install_withdraw_preflight_mocks` on mockito's ordering rule.
async fn install_router_happy_path(server: &mut mockito::ServerGuard, chain_id: u64) {
    install_router_happy_path_with_caps(server, chain_id, U256::from(u128::MAX)).await;
}

async fn install_router_happy_path_with_caps(
    server: &mut mockito::ServerGuard,
    chain_id: u64,
    max_withdraw_per_payment: U256,
) {
    install_happy_path_mocks(server, chain_id, SIGNER_ADDRESS).await;
    install_withdraw_preflight_mocks(
        server,
        max_withdraw_per_payment,
        U256::from(u128::MAX),
        U256::ZERO,
    )
    .await;
    install_post_preflight_mocks(server, &receipt_with_routed_withdrawal_body()).await;
}

fn unique_state_dir() -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("rmpc-router-test-{stamp}-{}", std::process::id()))
}

/// A two-leg router withdrawal. `--confirm` is added by the callers that
/// want to get past the preview gate.
fn router_args(config_path: &str, state_dir: &std::path::Path) -> Command {
    let mut cmd = rmpc();
    cmd.env(
        PASSPHRASE_ENV_VAR,
        std::str::from_utf8(TEST_PASSPHRASE).unwrap(),
    )
    .env("RMPC_STATE_DIR", state_dir)
    .args([
        "withdraw-router",
        "--config",
        config_path,
        "--shares-per-leg",
        &format!("{LEG_A},{LEG_B}"),
        "--vaults",
        &format!("{VAULT:#x},{VAULT_B:#x}"),
        "--order-id",
        &format!("{ORDER_ID:#x}"),
        "--idempotency-key",
        &format!("{IDEMPOTENCY_KEY:#x}"),
    ]);
    cmd
}

fn stdout_json(out: &std::process::Output) -> Value {
    serde_json::from_str(String::from_utf8(out.stdout.clone()).unwrap().trim())
        .expect("stdout is JSON")
}

#[tokio::test]
async fn router_happy_path_emits_per_leg_amounts_and_exits_zero() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    install_router_happy_path(&mut server, chain_id).await;

    let fix = Fixture::build(&server.url(), chain_id);
    let state_dir = unique_state_dir();

    let out = router_args(fix.config_path.to_str().unwrap(), &state_dir)
        .args(["--confirm", "--receipt-timeout-secs", "5"])
        .assert()
        .success()
        .get_output()
        .clone();

    let v = stdout_json(&out);
    assert_eq!(v["status"], "success");
    assert_eq!(v["payment_id"], format!("{PAYMENT_ID:#x}"));
    assert_eq!(v["order_id"], format!("{ORDER_ID:#x}"));
    assert_eq!(
        v["router"].as_str().unwrap().to_lowercase(),
        format!("{ROUTER:#x}")
    );
    assert_eq!(
        v["asset_recipient"].as_str().unwrap().to_lowercase(),
        format!("{ASSET_RECIPIENT:#x}")
    );
    assert_eq!(
        v["shares_per_leg"],
        json!([LEG_A.to_string(), LEG_B.to_string()])
    );
    assert_eq!(
        v["assets_per_leg"],
        json!([(LEG_A - 1_000).to_string(), (LEG_B - 1_000).to_string()])
    );
    assert_eq!(v["tx_hash"], format!("{TX_HASH:#x}"));
}

/// The `--confirm` gate: without it the command must refuse and must not
/// broadcast, even though every preflight passed.
#[tokio::test]
async fn router_without_confirm_previews_and_never_broadcasts() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    install_happy_path_mocks(&mut server, chain_id, SIGNER_ADDRESS).await;
    install_withdraw_preflight_mocks(
        &mut server,
        U256::from(u128::MAX),
        U256::from(u128::MAX),
        U256::ZERO,
    )
    .await;
    let broadcast_mock = server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(
            json!({"method": "eth_sendRawTransaction"}),
        ))
        .with_status(200)
        .with_body(jrpc_result(&format!("{TX_HASH:#x}")))
        .expect(0)
        .create_async()
        .await;

    let fix = Fixture::build(&server.url(), chain_id);
    let state_dir = unique_state_dir();

    let out = router_args(fix.config_path.to_str().unwrap(), &state_dir)
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["status"], "refused");
    assert_eq!(v["error"], "ErrConfirmNotProvided");
    assert!(v["message"].as_str().unwrap().contains("--confirm"));
    // The preview refusal carries the preflight snapshot the operator is
    // being asked to sign off on.
    assert!(v["checks"].is_object());
    // The preview goes to stderr, so stdout stays a single JSON document.
    assert!(String::from_utf8(out.stderr.clone())
        .unwrap()
        .contains("preview"));
    broadcast_mock.assert_async().await;
}

/// Issue #967: `vaults[i]` is identity-bound to `sharesPerLeg[i]`, so a
/// length mismatch is refused during argument parsing.
#[test]
fn router_refuses_mismatched_vault_and_leg_lengths() {
    let fix = Fixture::build("http://127.0.0.1:1", 31337);
    let out = rmpc()
        .env(
            PASSPHRASE_ENV_VAR,
            std::str::from_utf8(TEST_PASSPHRASE).unwrap(),
        )
        .env("RMPC_STATE_DIR", unique_state_dir())
        .args([
            "withdraw-router",
            "--config",
            fix.config_path.to_str().unwrap(),
            "--shares-per-leg",
            &format!("{LEG_A},{LEG_B}"),
            "--vaults",
            &format!("{VAULT:#x}"),
            "--order-id",
            &format!("{ORDER_ID:#x}"),
            "--confirm",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stdout.is_empty(),
        "argument refusals log, they do not print JSON"
    );
}

#[test]
fn router_base_mainnet_refuses_software_signer_before_signing() {
    let fix = Fixture::build("http://127.0.0.1:1", 8453);
    let state_dir = unique_state_dir();

    let out = router_args(fix.config_path.to_str().unwrap(), &state_dir)
        .env_remove(PASSPHRASE_ENV_VAR)
        .args(["--confirm"])
        .assert()
        .failure()
        .get_output()
        .clone();

    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["status"], "refused");
    assert_eq!(v["error"], "ErrProductionSignerRequired");
    assert_eq!(v["order_id"], format!("{ORDER_ID:#x}"));
}

#[tokio::test]
async fn router_chain_id_mismatch_refuses_with_named_error() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_chainId"})))
        .with_status(200)
        .with_body(jrpc_result("0x1"))
        .create_async()
        .await;
    install_router_happy_path(&mut server, chain_id).await;

    let fix = Fixture::build(&server.url(), chain_id);
    let out = router_args(fix.config_path.to_str().unwrap(), &unique_state_dir())
        .args(["--confirm"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["error"], "ErrChainIdMismatch");
    assert_eq!(v["checks"]["chain_id_match"], false);
}

#[tokio::test]
async fn router_paused_gateway_refuses_with_named_error() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            RobotMoneyGateway::pausedCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_bool(true)))
        .create_async()
        .await;
    install_router_happy_path(&mut server, chain_id).await;

    let fix = Fixture::build(&server.url(), chain_id);
    let out = router_args(fix.config_path.to_str().unwrap(), &unique_state_dir())
        .args(["--confirm"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["error"], "ErrGatewayPaused");
    assert_eq!(v["checks"]["gateway_paused"], true);
}

/// The window cap is checked against the SUM of the legs, not each leg —
/// splitting a redemption across legs must not evade the policy cap.
#[tokio::test]
async fn router_total_shares_over_policy_cap_refuses_before_signing() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    // Each leg alone is under the cap; their sum is not.
    install_router_happy_path_with_caps(&mut server, chain_id, U256::from(LEG_A + LEG_B - 1)).await;
    let broadcast_mock = server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(
            json!({"method": "eth_sendRawTransaction"}),
        ))
        .with_status(200)
        .with_body(jrpc_result(&format!("{TX_HASH:#x}")))
        .expect(0)
        .create_async()
        .await;

    let fix = Fixture::build(&server.url(), chain_id);
    let out = router_args(fix.config_path.to_str().unwrap(), &unique_state_dir())
        .args(["--confirm"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["error"], "ErrConfig");
    assert!(v["message"]
        .as_str()
        .unwrap()
        .contains("maxWithdrawPerPayment"));
    broadcast_mock.assert_async().await;
}

/// RPC-7: each identity-bound leg's vault-share allowance is checked, so
/// an unapproved gateway is refused client-side, not on chain.
#[tokio::test]
async fn router_leg_with_insufficient_share_allowance_refuses() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    server
        .mock("POST", "/")
        .match_body(match_eth_call_selector(&selector_hex_of::<
            Erc20::allowanceCall,
        >()))
        .with_status(200)
        .with_body(jrpc_result(&enc_u256(U256::from(1u64))))
        .create_async()
        .await;
    install_router_happy_path(&mut server, chain_id).await;

    let fix = Fixture::build(&server.url(), chain_id);
    let out = router_args(fix.config_path.to_str().unwrap(), &unique_state_dir())
        .args(["--confirm"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["error"], "ErrShareAllowanceInsufficient");
    // The message names the offending leg's vault.
    assert!(v["message"]
        .as_str()
        .unwrap()
        .contains(&format!("{VAULT:#x}")));
}

#[tokio::test]
async fn router_fee_cap_exceeded_refuses() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    install_happy_path_mocks(&mut server, chain_id, SIGNER_ADDRESS).await;
    install_withdraw_preflight_mocks(
        &mut server,
        U256::from(u128::MAX),
        U256::from(u128::MAX),
        U256::ZERO,
    )
    .await;
    let huge_fee_history = r#"{
        "oldestBlock":"0x1",
        "baseFeePerGas":["0x9184e72a000","0x9184e72a000","0x9184e72a000","0x9184e72a000","0x9184e72a000","0x9184e72a000"],
        "gasUsedRatio":[0.5,0.5,0.5,0.5,0.5],
        "reward":[["0x3b9aca00"],["0x3b9aca00"],["0x3b9aca00"],["0x3b9aca00"],["0x3b9aca00"]]
    }"#;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({"method": "eth_feeHistory"})))
        .with_status(200)
        .with_body(jrpc_result_raw(huge_fee_history))
        .create_async()
        .await;
    install_post_preflight_mocks(&mut server, &receipt_with_routed_withdrawal_body()).await;

    let fix = Fixture::build(&server.url(), chain_id);
    let out = router_args(fix.config_path.to_str().unwrap(), &unique_state_dir())
        .args(["--confirm"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["error"], "ErrFeeCapExceeded");
    assert!(v["checks"].is_object());
}

#[tokio::test]
async fn router_concurrent_invocation_locked() {
    use rust_payment_client::nonce::AgentLock;
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    install_router_happy_path(&mut server, chain_id).await;

    let fix = Fixture::build(&server.url(), chain_id);
    let state_dir = unique_state_dir();
    let held = AgentLock::acquire(&state_dir, &SIGNER_ADDRESS).expect("held");

    let out = router_args(fix.config_path.to_str().unwrap(), &state_dir)
        .args(["--confirm"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["error"], "ErrConcurrentInvocation");
    drop(held);
}

/// AZ-RPC-1: a receipt timeout is not a failure. The refusal surfaces the
/// broadcast tx_hash and the replay entry stays put, so a retry is refused
/// rather than broadcasting a second transaction for the same paymentId.
#[tokio::test]
async fn router_receipt_timeout_refuses_and_keeps_the_replay_entry() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(
            json!({"method": "eth_getTransactionReceipt"}),
        ))
        .with_status(200)
        .with_body(r#"{"jsonrpc":"2.0","id":1,"result":null}"#)
        .expect_at_least(1)
        .create_async()
        .await;
    install_router_happy_path(&mut server, chain_id).await;

    let fix = Fixture::build(&server.url(), chain_id);
    let state_dir = unique_state_dir();

    let out = router_args(fix.config_path.to_str().unwrap(), &state_dir)
        .args(["--confirm", "--receipt-timeout-secs", "1"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["error"], "ErrRpcTransport");
    assert_eq!(v["tx_hash"], format!("{TX_HASH:#x}"));

    let parsed: Value = serde_json::from_str(
        &std::fs::read_to_string(state_dir.join("submitted_order_ids.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        parsed["entries"].as_array().unwrap().len(),
        1,
        "a receipt timeout must NOT drop the replay entry (AZ-RPC-1)",
    );
}

/// AZ-RPC-2: a confirmed revert clears the optimistic replay entry so the
/// operator can retry the same order.
#[tokio::test]
async fn router_reverted_tx_emits_err_tx_reverted_and_clears_the_replay_entry() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    install_happy_path_mocks(&mut server, chain_id, SIGNER_ADDRESS).await;
    install_withdraw_preflight_mocks(
        &mut server,
        U256::from(u128::MAX),
        U256::from(u128::MAX),
        U256::ZERO,
    )
    .await;
    install_post_preflight_mocks(&mut server, &reverted_receipt_body()).await;

    let fix = Fixture::build(&server.url(), chain_id);
    let state_dir = unique_state_dir();

    let out = router_args(fix.config_path.to_str().unwrap(), &state_dir)
        .args(["--confirm", "--receipt-timeout-secs", "5"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(2));
    let v = stdout_json(&out);
    assert_eq!(v["error"], "ErrTxReverted");
    assert_eq!(v["tx_hash"], format!("{TX_HASH:#x}"));

    let parsed: Value = serde_json::from_str(
        &std::fs::read_to_string(state_dir.join("submitted_order_ids.json")).unwrap(),
    )
    .unwrap();
    assert!(
        parsed["entries"].as_array().unwrap().is_empty(),
        "a confirmed revert must clear the optimistic replay entry (AZ-RPC-2)",
    );
}

/// A replayed router withdrawal is refused locally before signing, and the
/// refusal points at the original tx_hash.
#[tokio::test]
async fn router_duplicate_retry_refused_before_broadcast() {
    let mut server = mockito::Server::new_async().await;
    let chain_id = 31337u64;
    install_router_happy_path(&mut server, chain_id).await;

    let fix = Fixture::build(&server.url(), chain_id);
    let state_dir = unique_state_dir();

    let out1 = router_args(fix.config_path.to_str().unwrap(), &state_dir)
        .args(["--confirm", "--receipt-timeout-secs", "5"])
        .assert()
        .success()
        .get_output()
        .clone();
    let prior_tx = stdout_json(&out1)["tx_hash"].as_str().unwrap().to_string();

    let out2 = router_args(fix.config_path.to_str().unwrap(), &state_dir)
        .args(["--confirm", "--receipt-timeout-secs", "5"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out2.status.code(), Some(2));
    let v = stdout_json(&out2);
    assert_eq!(v["error"], "ErrOrderIdAlreadySubmitted");
    assert_eq!(v["tx_hash"].as_str().unwrap(), prior_tx.as_str());
}
