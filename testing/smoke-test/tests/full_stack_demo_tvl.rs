//! Integration test: explorer API reports the four-vault real-TVL end state
//! *and* the router-weight end state after one `DappStack::boot`.
//!
//! Boots the full compose stack (chain + dapp + explorer-api + indexer) **once**,
//! then makes both of the assertions the dapp tiles depend on against that one
//! stack:
//!
//! 1. GET /v1/vaults — **exactly four** vault entries (PRD §11.1–§11.4), each
//!    **Active** (status == 0) and each reporting **non-zero** `total_assets`.
//!    This exercises the automatic demo-depositor seeding added by issue #532
//!    and extended to all four vaults by issue #563: DappStack::boot calls
//!    seed_demo_depositors so every vault's TVL is non-zero without any manual
//!    follow-up command.
//! 2. GET /v1/router/weights — non-empty `current_weights` summing to exactly
//!    10 000 bps (issue #615), proving the indexer ingests the WeightsSet /
//!    DefaultWeightsSet events emitted by the demo seed.
//!
//! WHY ONE `#[test]`, NOT TWO (issue #1371)
//! These were two `#[test]` fns. Rust's harness runs them in the same process
//! but each built its **own** `Fixture` + `DappStack`, so the binary booted and
//! tore down the entire devnet twice — measured at 23m22s + 21m33s = 44m55s of
//! the 46m56s job (run 34540953030, job 103087891903, 2026-09-10). Both
//! assertions read a *different endpoint of the same explorer-api* populated by
//! the *same* `seed_demo_depositors` call, so the second boot re-created state
//! the first boot had already produced. Merging them into one `#[test]` that
//! boots once removes a full bring-up and changes no assertion: each assertion
//! keeps its own verbatim poll loop, its own independent 90 s indexer-settle
//! deadline, and its own panic message. They are evaluated in the original
//! order, and the first one's success path falls through to the second rather
//! than returning from the test.
//!
//! Do NOT re-split these into two `#[test]` fns, and do not add a second
//! `Fixture::new()` / `DappStack::boot` to this binary: every such pair costs a
//! ~22-minute devnet bring-up in the `smoke-test-devnet-full_stack_demo_tvl`
//! matrix row. A genuinely new assertion belongs inside
//! `explorer_api_reports_four_vault_tvl_and_router_weights_after_boot` as
//! another helper called against the already-booted `dapp`.
//!
//! Canonical docs: docs/prd.md, docs/development/ci-suites.md §14,
//! testing/smoke-test/src/lib.rs, issues #592, #615, #1371.
//!
//! Run with:
//!   cargo test -p smoke-test --release --test full_stack_demo_tvl -- --test-threads=1 --nocapture

use smoke_test::{prerequisites_available, DappStack, DappStackOptions, Fixture, PublicEndpoints};

fn skip_if_no_prereqs(name: &str) -> bool {
    if !prerequisites_available() {
        eprintln!("[{name}] docker/forge/cast not on PATH; skipping.");
        return true;
    }
    false
}

/// Deserialise only the fields we care about from GET /v1/vaults.
#[derive(Debug, serde::Deserialize)]
struct VaultListResponse {
    vaults: Vec<VaultEntry>,
}

/// Deserialise the fields we care about from GET /v1/router/weights.
#[derive(Debug, serde::Deserialize)]
struct RouterWeightsResponse {
    current_weights: Vec<RouterWeightEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct RouterWeightEntry {
    bps: u64,
}

/// Poll GET /v1/router/weights on the explorer-api and return the response.
fn fetch_router_weights(explorer_api_url: &str) -> Result<RouterWeightsResponse, String> {
    let url = format!("{explorer_api_url}/v1/router/weights");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("reqwest builder: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    resp.json::<RouterWeightsResponse>()
        .map_err(|e| format!("GET {url} json decode: {e}"))
}

#[derive(Debug, serde::Deserialize)]
struct VaultEntry {
    /// 0 = Active, 1 = Paused, 2 = Retired (matches on-chain VaultStatus enum).
    status: i16,
    total_assets: Option<String>,
}

impl VaultEntry {
    /// A vault entry satisfies the four-vault real-TVL invariant when it is
    /// Active and its latest snapshot carries a non-zero `total_assets`.
    fn is_active_with_nonzero_tvl(&self) -> bool {
        self.status == 0
            && self
                .total_assets
                .as_deref()
                .is_some_and(|s| s != "0" && !s.is_empty())
    }
}

/// The four-vault real-TVL invariant as a pure predicate over one `/v1/vaults`
/// response: **exactly four** entries and **all four** Active with non-zero
/// `total_assets`.
///
/// Extracted from the poll loop (issue #1371) so the invariant that decides
/// pass-vs-fail is exercised by the hermetic `invariant_predicates` tests at
/// the bottom of this file. Consolidating the two `#[test]` fns into one means
/// the executed-test count no longer changes if an assertion silently stops
/// being made; these predicate tests are what keep that from going unnoticed —
/// loosen this function and they go red without a devnet.
fn four_vault_tvl_invariant_holds(resp: &VaultListResponse) -> bool {
    let active_nonzero = resp
        .vaults
        .iter()
        .filter(|v| v.is_active_with_nonzero_tvl())
        .count();
    resp.vaults.len() == 4 && active_nonzero == 4
}

/// The router-weight invariant as a pure predicate over one
/// `/v1/router/weights` response: **non-empty** `current_weights` summing to
/// exactly 10 000 bps. Extracted for the same reason as
/// [`four_vault_tvl_invariant_holds`].
fn router_weights_invariant_holds(resp: &RouterWeightsResponse) -> bool {
    !resp.current_weights.is_empty()
        && resp.current_weights.iter().map(|w| w.bps).sum::<u64>() == 10_000
}

/// Poll GET /v1/vaults on the explorer-api and return the response.
fn fetch_vaults(explorer_api_url: &str) -> Result<VaultListResponse, String> {
    let url = format!("{explorer_api_url}/v1/vaults");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("reqwest builder: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    resp.json::<VaultListResponse>()
        .map_err(|e| format!("GET {url} json decode: {e}"))
}

/// Assertion 1 (issue #592): GET /v1/vaults must surface the four-vault
/// real-TVL end state — **exactly four** vault entries (PRD §11.1–§11.4), each
/// **Active** (status == 0) and each with **non-zero** `total_assets`. The
/// seeding is automatic: DappStack::boot calls seed_demo_depositors internally
/// (issues #532, #563) so no manual step is needed.
///
/// The explorer-indexer writes vault_snapshot rows as it processes Deposit
/// events; total_assets is populated once the first snapshot lands. We give
/// the indexer up to 90 s to process the seed deposits across all four vaults
/// mined during boot — the four-vault end state needs one snapshot per vault,
/// so it lags a single-vault non-zero check.
///
/// Panics (failing the test) if the invariant is not reached inside the budget.
fn assert_four_active_nonzero_vaults(explorer_api_url: &str) {
    // Poll for up to 90 s giving the indexer time to snapshot all four vaults.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut last_response: Option<VaultListResponse> = None;
    while std::time::Instant::now() < deadline {
        match fetch_vaults(explorer_api_url) {
            Ok(resp) => {
                // Exactly four entries, all Active with non-zero TVL.
                if four_vault_tvl_invariant_holds(&resp) {
                    eprintln!(
                        "[full_stack_demo_tvl] assertion 1/2 PASSED: GET /v1/vaults reports \
                         four Active vaults, each with non-zero total_assets"
                    );
                    return;
                }
                last_response = Some(resp);
            }
            Err(e) => {
                eprintln!("full_stack_demo_tvl: poll error (will retry): {e}");
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }

    let vaults_debug = last_response
        .map(|r| {
            format!(
                "{} entries: [{}]",
                r.vaults.len(),
                r.vaults
                    .iter()
                    .map(|v| format!("status={} total_assets={:?}", v.status, v.total_assets))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .unwrap_or_else(|| "no response received".to_string());

    panic!(
        "expected exactly four Active vaults each with total_assets > 0 after \
         DappStack::boot + 90s indexer wait, but got: {vaults_debug}"
    );
}

/// Assertion 2 (issue #615): GET /v1/router/weights must return non-empty
/// current_weights with the four router-eligible vault entries summing to
/// exactly 10 000 bps (8500/500/500/500, rmRWA weighted per issue #621).
/// This proves the indexer ingests WeightsSet / DefaultWeightsSet events
/// emitted by the demo seed's direct admin calls to
/// PortfolioRouter.setWeights() / setDefaultWeights().
///
/// INDEXER_PORTFOLIO_ROUTER must be set to the PortfolioRouter address in
/// DappStack — confirmed in smoke-test/src/lib.rs.
///
/// Panics (failing the test) if the invariant is not reached inside the budget.
fn assert_router_weights_sum_to_full_allocation(explorer_api_url: &str) {
    // Poll for up to 90 s giving the indexer time to process the WeightsSet /
    // DefaultWeightsSet events emitted during demo seeding. This budget is
    // deliberately independent of the vault-TVL poll above: sharing one
    // deadline across both assertions would silently shrink this one whenever
    // the first assertion took a while to settle.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut last_response: Option<RouterWeightsResponse> = None;
    while std::time::Instant::now() < deadline {
        match fetch_router_weights(explorer_api_url) {
            Ok(resp) => {
                // Exactly 10 000 bps across the router-eligible vaults.
                if router_weights_invariant_holds(&resp) {
                    eprintln!(
                        "[full_stack_demo_tvl] assertion 2/2 PASSED: GET /v1/router/weights \
                         reports non-empty current_weights summing to 10000 bps"
                    );
                    return;
                }
                last_response = Some(resp);
            }
            Err(e) => {
                eprintln!("full_stack_demo_tvl router-weights: poll error (will retry): {e}");
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }

    let debug = last_response
        .map(|r| {
            let total: u64 = r.current_weights.iter().map(|w| w.bps).sum();
            format!(
                "{} weight entries, total bps={}",
                r.current_weights.len(),
                total
            )
        })
        .unwrap_or_else(|| "no response received".to_string());

    panic!(
        "expected GET /v1/router/weights to return non-empty current_weights summing to \
         10000 bps after DappStack::boot + 90s indexer wait, but got: {debug}"
    );
}

/// One devnet bring-up, both explorer-api end-state assertions (issues #592,
/// #615; consolidated by issue #1371).
///
/// The two assertions were previously two `#[test]` fns, each paying its own
/// `Fixture::new()` + `DappStack::boot`. They are made here against a single
/// booted stack, in the original order, with the original poll budgets and
/// panic messages. Both always run: `assert_four_active_nonzero_vaults`
/// returning normally means it *passed*, and control falls through to the
/// router-weight assertion.
#[test]
fn explorer_api_reports_four_vault_tvl_and_router_weights_after_boot() {
    if skip_if_no_prereqs("explorer_api_reports_four_vault_tvl_and_router_weights_after_boot") {
        return;
    }

    let fixture = Fixture::new().expect("fixture boot");
    let opts = DappStackOptions {
        dapp_port: None,
        explorer_api_port: None,
        public_endpoints: PublicEndpoints::Local,
    };
    // Bound to `dapp` (not `_`) so the stack stays up for both assertions and is
    // torn down by Drop only after the second one has run.
    let dapp = DappStack::boot(&fixture, opts).expect("DappStack::boot");

    let explorer_api_url = dapp.endpoints.explorer_api_url.clone();

    // Assertion 1 — GET /v1/vaults: four Active vaults, each non-zero TVL.
    assert_four_active_nonzero_vaults(&explorer_api_url);

    // Assertion 2 — GET /v1/router/weights: non-empty, summing to 10 000 bps.
    assert_router_weights_sum_to_full_allocation(&explorer_api_url);
}

// ── Hermetic invariant-predicate tests (issue #1371) ────────────────────────
//
// WHY THESE EXIST
// Consolidating the two devnet `#[test]` fns into one (see the module header)
// halves the CI cost but also removes a signal: the binary used to report
// `2 passed`, so an assertion that stopped being made changed the count.
// It reports `1 passed` for the devnet test now, and would keep doing so if
// `assert_router_weights_sum_to_full_allocation` were quietly dropped from the
// end of the test body.
//
// These tests close that gap from the other side: they pin the two pure
// predicates that decide pass-vs-fail, against the exact failure shapes the
// devnet assertions exist to catch (three vaults, a paused vault, a zero-TVL
// vault, weights that do not sum to a full allocation). Loosening either
// invariant turns them red — in milliseconds, with no Docker, in the same
// `smoke-test-devnet-full_stack_demo_tvl` job. They also raise that job's
// executed-test count from 1 to 7, so `cargo_test_require_executed.sh` has more
// than a single result line to stand on.
//
// Verified mutation-sensitive before landing (issue #1371): loosening
// `four_vault_tvl_invariant_holds` to `!resp.vaults.is_empty() && active_nonzero
// >= 1` reds `fewer_or_more_than_four_vaults_fail_the_tvl_invariant` and
// `a_non_active_or_zero_tvl_vault_fails_the_tvl_invariant`; loosening
// `router_weights_invariant_holds`'s `== 10_000` to `> 0` reds
// `weights_that_do_not_sum_to_a_full_allocation_fail_the_invariant`.
//
// They are deliberately NOT a substitute for the devnet assertions: they prove
// the predicate rejects a broken end state, not that the stack reaches a good
// one. Both halves are required.
#[cfg(test)]
mod invariant_predicates {
    use super::{
        four_vault_tvl_invariant_holds, router_weights_invariant_holds, RouterWeightsResponse,
        VaultListResponse,
    };

    fn vaults(json: &str) -> VaultListResponse {
        serde_json::from_str(json).expect("vault fixture json")
    }

    fn weights(json: &str) -> RouterWeightsResponse {
        serde_json::from_str(json).expect("router-weights fixture json")
    }

    const FOUR_GOOD_VAULTS: &str = r#"{"vaults":[
        {"status":0,"total_assets":"1000000"},
        {"status":0,"total_assets":"2000000"},
        {"status":0,"total_assets":"3000000"},
        {"status":0,"total_assets":"4000000"}]}"#;

    #[test]
    fn four_active_nonzero_vaults_satisfy_the_tvl_invariant() {
        assert!(four_vault_tvl_invariant_holds(&vaults(FOUR_GOOD_VAULTS)));
    }

    #[test]
    fn fewer_or_more_than_four_vaults_fail_the_tvl_invariant() {
        let three = r#"{"vaults":[
            {"status":0,"total_assets":"1"},
            {"status":0,"total_assets":"2"},
            {"status":0,"total_assets":"3"}]}"#;
        let five = r#"{"vaults":[
            {"status":0,"total_assets":"1"},
            {"status":0,"total_assets":"2"},
            {"status":0,"total_assets":"3"},
            {"status":0,"total_assets":"4"},
            {"status":0,"total_assets":"5"}]}"#;
        assert!(!four_vault_tvl_invariant_holds(&vaults(three)));
        assert!(!four_vault_tvl_invariant_holds(&vaults(five)));
        assert!(!four_vault_tvl_invariant_holds(&vaults(r#"{"vaults":[]}"#)));
    }

    #[test]
    fn a_non_active_or_zero_tvl_vault_fails_the_tvl_invariant() {
        // status 1 = Paused, 2 = Retired.
        let paused = r#"{"vaults":[
            {"status":1,"total_assets":"1"},
            {"status":0,"total_assets":"2"},
            {"status":0,"total_assets":"3"},
            {"status":0,"total_assets":"4"}]}"#;
        // A vault the indexer has seen but never snapshotted a deposit for.
        let zero = r#"{"vaults":[
            {"status":0,"total_assets":"0"},
            {"status":0,"total_assets":"2"},
            {"status":0,"total_assets":"3"},
            {"status":0,"total_assets":"4"}]}"#;
        let missing = r#"{"vaults":[
            {"status":0,"total_assets":null},
            {"status":0,"total_assets":"2"},
            {"status":0,"total_assets":"3"},
            {"status":0,"total_assets":"4"}]}"#;
        let empty_string = r#"{"vaults":[
            {"status":0,"total_assets":""},
            {"status":0,"total_assets":"2"},
            {"status":0,"total_assets":"3"},
            {"status":0,"total_assets":"4"}]}"#;
        assert!(!four_vault_tvl_invariant_holds(&vaults(paused)));
        assert!(!four_vault_tvl_invariant_holds(&vaults(zero)));
        assert!(!four_vault_tvl_invariant_holds(&vaults(missing)));
        assert!(!four_vault_tvl_invariant_holds(&vaults(empty_string)));
    }

    #[test]
    fn the_8500_500_500_500_split_satisfies_the_router_weight_invariant() {
        // The four router-eligible vaults, rmRWA weighted per issue #621.
        let full = r#"{"current_weights":[
            {"bps":8500},{"bps":500},{"bps":500},{"bps":500}]}"#;
        assert!(router_weights_invariant_holds(&weights(full)));
    }

    #[test]
    fn weights_that_do_not_sum_to_a_full_allocation_fail_the_invariant() {
        let short = r#"{"current_weights":[
            {"bps":8500},{"bps":500},{"bps":500}]}"#;
        let over = r#"{"current_weights":[
            {"bps":8500},{"bps":500},{"bps":500},{"bps":1000}]}"#;
        assert!(!router_weights_invariant_holds(&weights(short)));
        assert!(!router_weights_invariant_holds(&weights(over)));
    }

    #[test]
    fn empty_router_weights_fail_the_invariant() {
        // The pre-#615 false-green shape: the indexer never ingested a
        // WeightsSet event, so the endpoint answers 200 with nothing in it.
        assert!(!router_weights_invariant_holds(&weights(
            r#"{"current_weights":[]}"#
        )));
    }
}
