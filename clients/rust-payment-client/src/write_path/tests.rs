//! Canonical: none — unit tests for the shared write-path orchestration
//!
//! These drive the extracted orchestration against a real `mockito`
//! JSON-RPC server **without spawning the `rmpc` binary** — the seam the
//! monolithic `run()` functions never had (issue #1285). The RPC client
//! is a parameter, so the fee bid, the nonce read, the envelope build and
//! the signature all execute here with the transport, encoder and decoder
//! in the loop.

use super::*;
use crate::config::{Config, SignerConfig};
use alloy_consensus::TxEnvelope;
use alloy_primitives::{address, hex as ahex, keccak256, Address};
use alloy_rlp::Decodable;
use mockito::Matcher;
use serde_json::json;
use tempfile::TempDir;

const GATEWAY: Address = address!("0000000000000000000000000000000000000b00");
const USDC: Address = address!("0000000000000000000000000000000000000c00");
const VAULT: Address = address!("0000000000000000000000000000000000000d00");
/// anvil account #0 — deterministic test fixture only.
const TEST_PRIVKEY: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];
const TEST_PASSPHRASE: &[u8] = b"correct horse battery staple";

fn config(max_fee_cap: u64) -> Config {
    Config {
        chain_id: 31337,
        rpc_url: Some("http://placeholder".into()),
        rpc_urls: None,
        gateway_address: format!("{GATEWAY:#x}"),
        usdc_address: format!("{USDC:#x}"),
        vault_address: format!("{VAULT:#x}"),
        gateway_runtime_hash: format!("0x{}", ahex::encode(keccak256(b"code"))),
        max_fee_per_gas_cap: Some(max_fee_cap),
        max_priority_fee_per_gas_cap: None,
        state_dir: None,
        registry_address: None,
        router_address: None,
        governance_address: None,
        timelock_address: None,
        ic_policy_address: None,
        receipt_address: None,
        vault_addresses: None,
        signer: SignerConfig {
            allow_software_fallback: true,
            keystore_path: std::path::PathBuf::from("/tmp/unused.enc"),
        },
        log: Default::default(),
    }
}

/// A real software signer backed by a throwaway keystore in a temp dir.
/// The `TempDir` is returned so the caller keeps it alive.
fn signer() -> (TempDir, SoftwareSigner) {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("keystore.json");
    SoftwareSigner::create_keystore(&path, &TEST_PRIVKEY, TEST_PASSPHRASE).expect("create");
    let s = SoftwareSigner::load_with_passphrase(&path, TEST_PASSPHRASE, true).expect("load");
    (tmp, s)
}

/// base fee 1 gwei, tip 1 gwei ⇒ maxFee = 2*1 + 1 = 3 gwei.
const CHEAP_FEE_HISTORY: &str = r#"{
    "oldestBlock":"0x1",
    "baseFeePerGas":["0x3b9aca00","0x3b9aca00","0x3b9aca00","0x3b9aca00","0x3b9aca00","0x3b9aca00"],
    "gasUsedRatio":[0.5,0.5,0.5,0.5,0.5],
    "reward":[["0x3b9aca00"],["0x3b9aca00"],["0x3b9aca00"],["0x3b9aca00"],["0x3b9aca00"]]
}"#;

fn jrpc_result(s: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{s}"}}"#)
}

fn jrpc_result_raw(json: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":{json}}}"#)
}

async fn mock_method(server: &mut mockito::ServerGuard, method: &str, body: String) {
    server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(json!({ "method": method })))
        .with_status(200)
        .with_body(body)
        .expect_at_least(0)
        .create_async()
        .await;
}

#[tokio::test]
async fn chain_deadline_uses_block_timestamp_not_wall_clock() {
    let mut server = mockito::Server::new_async().await;
    mock_method(&mut server, "eth_blockNumber", jrpc_result("0x100")).await;
    mock_method(
        &mut server,
        "eth_getBlockByNumber",
        jrpc_result_raw(r#"{"timestamp":"0x64a9f4c0"}"#),
    )
    .await;

    let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();
    // 0x64a9f4c0 = 1_688_859_840 — a 2023 timestamp, so a wall-clock
    // implementation could not produce this number.
    assert_eq!(chain_deadline(&rpc, 300).await.unwrap(), 1_688_860_140);
}

#[tokio::test]
async fn fee_bid_refuses_when_the_bid_exceeds_the_operator_cap() {
    let mut server = mockito::Server::new_async().await;
    mock_method(
        &mut server,
        "eth_feeHistory",
        jrpc_result_raw(CHEAP_FEE_HISTORY),
    )
    .await;
    let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();

    // Cap of 1 gwei against a 3 gwei bid.
    let err = fee_bid(&rpc, &config(1_000_000_000), None)
        .await
        .unwrap_err();
    match err {
        EnvelopeError::FeeCap(e) => assert_eq!(e.name(), "ErrFeeCapExceeded"),
        other => panic!("expected a fee-cap refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn fee_bid_accepts_a_bid_under_the_operator_cap() {
    let mut server = mockito::Server::new_async().await;
    mock_method(
        &mut server,
        "eth_feeHistory",
        jrpc_result_raw(CHEAP_FEE_HISTORY),
    )
    .await;
    let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();

    let bid = fee_bid(&rpc, &config(100_000_000_000), None)
        .await
        .expect("bid under cap");
    assert_eq!(bid.max_fee_per_gas, 3_000_000_000);
    assert_eq!(bid.max_priority_fee_per_gas, 1_000_000_000);
}

/// The whole envelope half of the orchestration — fee bid, nonce read,
/// EIP-1559 build, signature, RLP encode — against mockito. The signed
/// envelope must decode back to the nonce, gateway, fees and calldata the
/// orchestration was handed, and recover to the signer's own address.
#[tokio::test]
async fn signed_envelope_round_trips_through_the_mock_rpc() {
    let mut server = mockito::Server::new_async().await;
    mock_method(
        &mut server,
        "eth_feeHistory",
        jrpc_result_raw(CHEAP_FEE_HISTORY),
    )
    .await;
    mock_method(&mut server, "eth_getTransactionCount", jrpc_result("0x7")).await;
    let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();
    let (_tmp, signer) = signer();
    let cfg = config(100_000_000_000);

    let calldata = vec![0xde, 0xad, 0xbe, 0xef];
    let raw = signed_envelope(
        &rpc,
        &cfg,
        &signer,
        GATEWAY,
        250_000,
        None,
        calldata.clone(),
    )
    .await
    .expect("envelope signed");

    let decoded = TxEnvelope::decode(&mut raw.as_ref()).expect("valid EIP-2718 envelope");
    let TxEnvelope::Eip1559(signed) = decoded else {
        panic!("expected a type-2 envelope");
    };
    assert_eq!(
        signed.tx().nonce,
        7,
        "nonce comes from eth_getTransactionCount"
    );
    assert_eq!(signed.tx().chain_id, cfg.chain_id);
    assert_eq!(signed.tx().gas_limit, 250_000);
    assert_eq!(signed.tx().max_fee_per_gas, 3_000_000_000);
    assert_eq!(signed.tx().max_priority_fee_per_gas, 1_000_000_000);
    assert_eq!(signed.tx().to.to().copied(), Some(GATEWAY));
    assert_eq!(signed.tx().input.as_ref(), calldata.as_slice());
    assert_eq!(
        signed.recover_signer().expect("recoverable"),
        signer.public_address(),
        "the envelope must be signed by the loaded keystore",
    );
}

/// A fee-cap breach must stop the orchestration before it ever reaches
/// `eth_getTransactionCount` — no nonce is burned on a refused write.
#[tokio::test]
async fn signed_envelope_refuses_on_fee_cap_before_reading_the_nonce() {
    let mut server = mockito::Server::new_async().await;
    mock_method(
        &mut server,
        "eth_feeHistory",
        jrpc_result_raw(CHEAP_FEE_HISTORY),
    )
    .await;
    let nonce_mock = server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(
            json!({"method": "eth_getTransactionCount"}),
        ))
        .with_status(200)
        .with_body(jrpc_result("0x7"))
        .expect(0)
        .create_async()
        .await;
    let rpc = FailoverRpcClient::new(vec![server.url()]).unwrap();
    let (_tmp, signer) = signer();

    let err = signed_envelope(
        &rpc,
        &config(1_000_000_000),
        &signer,
        GATEWAY,
        250_000,
        None,
        vec![0x01],
    )
    .await
    .unwrap_err();
    match err {
        EnvelopeError::FeeCap(e) => assert_eq!(e.name(), "ErrFeeCapExceeded"),
        other => panic!("expected a fee-cap refusal, got {other:?}"),
    }
    nonce_mock.assert_async().await;
}

/// The one refusal shape. Every write command's refusal document is built
/// here, so this is the single place the field set is pinned.
#[test]
fn refusal_documents_share_one_field_set() {
    let full = WriteFailure::new("ErrTxReverted")
        .message("transaction reverted on-chain")
        .agent(GATEWAY)
        .order_id(B256::repeat_byte(0xaa))
        .tx_hash("0xdead")
        .checks(ChecksOutput::unknown());
    let v = serde_json::to_value(&full).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["agent", "checks", "error", "message", "order_id", "status", "tx_hash"],
    );
    assert_eq!(v["status"], "refused");

    // Absent optionals are omitted, never emitted as JSON null — the
    // pre-refactor `skip_serializing_if` behaviour.
    let bare = serde_json::to_value(WriteFailure::new("ErrConcurrentInvocation")).unwrap();
    let mut bare_keys: Vec<&str> = bare
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    bare_keys.sort_unstable();
    assert_eq!(bare_keys, ["error", "status"]);
}

#[test]
fn abort_exit_codes_match_the_documented_cli_contract() {
    assert_eq!(WriteAbort::Startup.exit(false), EXIT_STARTUP_FAIL);
    assert_eq!(
        WriteAbort::refused(WriteFailure::new("ErrGatewayPaused")).exit(false),
        EXIT_REFUSAL
    );
}
