//! Smoke-test for the native Base ETH faucet drip round-trip (issue #466).
//!
//! Verifies:
//!   1. The harness EOA holds non-zero native ETH at devnet boot (the
//!      precondition for any ETH drip preflight in the dapp).
//!   2. `Fixture::fund_eth_from_harness` performs a real signed value
//!      transfer and the recipient's native balance increases by the
//!      exact amount — same code path the dapp's `dripEth` exercises.
//!   3. Concurrent drips from the one faucet key all land, none rejected
//!      with `replacement transaction underpriced` (issue #1374).
//!   4. A drip the faucet cannot afford still fails loudly, with a message
//!      that cannot be confused with that nonce race (issue #1374).
//!
//! Run with:
//!   cargo test -p smoke-test -- faucet_eth --test-threads=1 --nocapture
//!
//! Canonical: docs/architecture.md §5.3 — Human Dapp (faucet UX)

use alloy_primitives::{Address, U256};
use smoke_test::{prerequisites_available, Fixture, HARNESS_USDC_HOLDER_ADDRESS_HEX};

fn skip_if_no_prereqs(name: &str) -> bool {
    if !prerequisites_available() {
        eprintln!("[{name}] docker/forge/cast not on PATH; skipping.");
        return true;
    }
    false
}

/// Loud-skip, never silent-skip: the issue #1374 regression tests below are
/// the only evidence that the funding path is nonce-safe, so a runner without
/// docker/forge/cast must FAIL rather than quietly report a pass on nothing.
/// suite-14 verifies Docker before this binary runs, so this panic means a
/// genuinely broken runner, not an expected environment.
fn require_prereqs(name: &str) {
    assert!(
        prerequisites_available(),
        "[{name}] docker/forge/cast are required for this devnet regression test and were not \
         found on PATH — refusing to report a pass without exercising the funding path"
    );
}

fn fixture() -> &'static Fixture {
    use std::sync::OnceLock;
    static CELL: OnceLock<Fixture> = OnceLock::new();
    CELL.get_or_init(|| Fixture::new().expect("smoke-test fixture boot failed"))
}

fn eth_balance(fx: &Fixture, holder: Address) -> U256 {
    let raw: String = rpc_call(
        fx.rpc_url(),
        "eth_getBalance",
        serde_json::json!([format!("{holder:#x}"), "latest"]),
    );
    U256::from_str_radix(raw.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO)
}

#[test]
fn harness_holds_nonzero_native_eth_at_boot() {
    if skip_if_no_prereqs("harness_holds_nonzero_native_eth_at_boot") {
        return;
    }
    let fx = fixture();
    let harness: Address = HARNESS_USDC_HOLDER_ADDRESS_HEX.parse().unwrap();
    let balance = eth_balance(fx, harness);
    assert!(
        balance > U256::ZERO,
        "harness EOA should hold a non-zero native ETH balance at boot, got 0"
    );
}

#[test]
fn faucet_eth_drip_increases_recipient_balance_by_exact_amount() {
    if skip_if_no_prereqs("faucet_eth_drip_increases_recipient_balance_by_exact_amount") {
        return;
    }
    let fx = fixture();
    let recipient = fx.agent();
    // 0.01 ETH — mirrors FAUCET_DRIP_AMOUNT_ETH in chainClassifier.ts.
    let amount_wei = "10000000000000000";

    let before = eth_balance(fx, recipient);
    let tx_hash = fx
        .fund_eth_from_harness(recipient, amount_wei)
        .expect("fund_eth_from_harness");
    assert!(
        tx_hash.starts_with("0x") && tx_hash.len() == 66,
        "tx_hash {tx_hash:?}"
    );
    let after = eth_balance(fx, recipient);
    let amount = U256::from_str_radix(amount_wei, 10).unwrap();
    assert_eq!(
        after,
        before + amount,
        "recipient native balance did not grow by exact FAUCET_DRIP_AMOUNT_ETH"
    );
}

fn rpc_call<T: for<'de> serde::Deserialize<'de>>(
    url: &str,
    method: &str,
    params: serde_json::Value,
) -> T {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    client
        .post(url)
        .json(&body)
        .send()
        .expect("RPC request failed")
        .json::<serde_json::Value>()
        .expect("RPC response is not JSON")
        .get("result")
        .and_then(|r| serde_json::from_value(r.clone()).ok())
        .expect("no result field in RPC response")
}

/// Issue #1374: the funding path must be structurally unable to hand two
/// same-account sends a colliding nonce.
///
/// Fixture bring-up funds several accounts from a small number of shared
/// keys, and `seed_demo_depositors` / `DappStack::boot` do some of it on
/// concurrent threads. Before the fix those sends bypassed the harness nonce
/// tracker entirely and let `cast send` derive a nonce from a `latest`-tagged
/// read, so two of them collided and geth rejected the loser with
/// `-32000: replacement transaction underpriced` — a bring-up panic that read
/// like a test failure on whatever PR happened to be running.
///
/// This drives the real faucet key on the real devnet with the concurrency
/// that shape needs, and asserts BOTH that no send was rejected and that
/// every recipient actually received its ETH (so a send silently vanishing
/// cannot pass either).
#[test]
fn concurrent_faucet_drips_never_collide_on_a_nonce() {
    require_prereqs("concurrent_faucet_drips_never_collide_on_a_nonce");
    let fx = fixture();
    const DRIPS: u64 = 6;
    // 0.001 ETH each — enough to be observable, small enough that six of them
    // stay far inside the harness EOA's genesis grant.
    const AMOUNT_WEI: &str = "1000000000000000";
    // Fresh addresses nothing else in the harness touches, so a balance delta
    // is unambiguous.
    let recipients: Vec<Address> = (0..DRIPS)
        .map(|i| {
            format!("0x{:040x}", 0x1374_0000_u64 + i)
                .parse()
                .expect("recipient address")
        })
        .collect();
    for r in &recipients {
        assert_eq!(
            eth_balance(fx, *r),
            U256::ZERO,
            "recipient {r:#x} should start empty"
        );
    }

    let results: Vec<Result<String, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = recipients
            .iter()
            .map(|r| {
                s.spawn(move || {
                    fx.fund_eth_from_harness(*r, AMOUNT_WEI)
                        .map_err(|e| e.to_string())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("drip thread panicked"))
            .collect()
    });

    for (r, result) in recipients.iter().zip(&results) {
        match result {
            Ok(tx_hash) => assert!(
                tx_hash.starts_with("0x") && tx_hash.len() == 66,
                "drip to {r:#x} returned a malformed tx hash {tx_hash:?}"
            ),
            Err(msg) => {
                assert!(
                    !msg.contains("replacement transaction underpriced"),
                    "concurrent drip to {r:#x} lost a nonce race — this is issue #1374: {msg}"
                );
                panic!("concurrent drip to {r:#x} failed: {msg}");
            }
        }
    }

    let amount = U256::from_str_radix(AMOUNT_WEI, 10).unwrap();
    for r in &recipients {
        assert_eq!(
            eth_balance(fx, *r),
            amount,
            "recipient {r:#x} did not receive its concurrent drip"
        );
    }
}

/// Issue #1374 negative self-test: the nonce-race fix must not swallow a
/// genuine inability to fund.
///
/// Asking the faucet for more ETH than it holds is a hard failure the node
/// refuses on estimation. It must surface immediately — labelled, with the
/// node's own reason intact, and textually distinct from the nonce race — and
/// it must leave the faucet's nonce sequence usable, so one loud failure does
/// not degrade into every later drip hanging behind an unspent nonce.
#[test]
fn a_drip_the_faucet_cannot_afford_fails_loudly_and_distinguishably() {
    require_prereqs("a_drip_the_faucet_cannot_afford_fails_loudly_and_distinguishably");
    let fx = fixture();
    let recipient: Address = "0x0000000000000000000000000000000013740099"
        .parse()
        .unwrap();
    // 100,000 ETH — the harness EOA's genesis grant is three orders of
    // magnitude smaller (`genesis_alloc::DEFAULT_HARNESS_ETH_WEI` = 1000 ETH).
    const UNAFFORDABLE_WEI: &str = "100000000000000000000000";

    let err = fx
        .fund_eth_from_harness(recipient, UNAFFORDABLE_WEI)
        .expect_err("a drip larger than the faucet's balance must not report success")
        .to_string();
    assert!(
        err.contains("fund_eth_from_harness"),
        "the failure must name the funding call it came from: {err}"
    );
    assert!(
        err.to_lowercase().contains("insufficient funds"),
        "the node's own reason must survive to the harness error: {err}"
    );
    assert!(
        !err.contains("replacement transaction underpriced"),
        "a genuine funding failure must not read as the #1374 nonce race: {err}"
    );
    assert_eq!(
        eth_balance(fx, recipient),
        U256::ZERO,
        "a failed drip must not move any ETH"
    );

    // The unspent nonce is handed back, so the faucet still works afterwards.
    const AMOUNT_WEI: &str = "1000000000000000";
    let follow_up: Address = "0x0000000000000000000000000000000013740098"
        .parse()
        .unwrap();
    fx.fund_eth_from_harness(follow_up, AMOUNT_WEI)
        .expect("the faucet must still be usable after a failed drip");
    assert_eq!(
        eth_balance(fx, follow_up),
        U256::from_str_radix(AMOUNT_WEI, 10).unwrap(),
        "the drip after a failed one must land, not stall behind an unspent nonce"
    );
}
