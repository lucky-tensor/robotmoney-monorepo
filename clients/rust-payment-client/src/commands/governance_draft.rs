//! Canonical: docs/product/20260623-product-proposal-investment-committee-v0.md §3.4, §6.2
//! Canonical: docs/technical/governance-decisions.md
//! Implements: issue #1248 — (fusion) governance handoff and the separation invariants
//!
//! `rmpc governance draft-proposal` — turn a **released** consensus receipt
//! into a draft `RouterGovernance.propose(vaults, bps)` call for a human to
//! review. This command **never signs, never takes a nonce lock, and never
//! broadcasts** — it imports no signer, no [`crate::nonce`], no
//! [`crate::tx::broadcast`]. That is a structural property of this module,
//! not a runtime check: there is no code path in this file that could submit
//! a transaction even if every flag were set wrong. The committee that
//! authored the receipt and the `RouterGovernance` voter set that must
//! approve it are separate governing bodies (§3.4); this worker is
//! convenience tooling standing entirely on the recommending side of that
//! line, never the approving side.
//!
//! # What it does
//!
//! 1. **Observes `ReceiptReleased`.** Either a single `--receipt-id` (as a
//!    log-watcher would receive from the event) or a `--from-block` /
//!    `--to-block` range scanned via `eth_getLogs`. A receipt that has not
//!    been released (`ConsensusRecommendationReceipt.isReleased == false`) is
//!    refused — release is a human admin-discretion gate (D5) and this
//!    command does not second-guess it.
//! 2. **Fetches and validates the payload** at `--receipt-url` (mirrors
//!    `rmpc receipt verify`). A receipt with no `weights` vector has nothing
//!    to draft and is skipped, not treated as an error — most receipts are
//!    published, not applied (§2.1), and that is the intended design.
//! 3. **Maps buckets to vaults** through the operator config's
//!    `[vault_addresses]` table
//!    (`tests/fixtures/consensus-receipt.bucket-vault-map.json`).
//! 4. **Re-checks `PortfolioRouter.isRouterEligibleAndActive`** for every
//!    mapped vault *at draft time* — a vault Active when the receipt was
//!    recorded may be Paused by the time a human gets around to reviewing
//!    the draft, and `propose()` would revert with `VaultNotEligible` on
//!    exactly that vault. **Fallback:** any ineligible vault is dropped from
//!    the vector and its bps redistributed proportionally across the
//!    remaining eligible vaults, settling the last (in canonical bucket
//!    order) to the exact 10 000-bps remainder — the same
//!    round-all-but-the-last / settle-the-last-bucket shape the receipt's own
//!    bps conversion uses. If every mapped vault is ineligible there is
//!    nothing left to propose and the command refuses.
//! 5. **Applies the one-active-proposal queueing policy.** `propose()`
//!    reverts with `ActiveProposalExists` while the current proposal is
//!    `Active` or `Queued`. This command replicates that state check
//!    read-only and reports the draft as `blocked_active_proposal` (with the
//!    blocking proposal id) rather than presenting it as ready to submit.
//! 6. **Emits a draft** — the resolved `vaults`/`bps` vector, the exact
//!    `RouterGovernance.propose` calldata (hex), and enough context for a
//!    human to decide whether to submit it (via `rmpc propose`, a Safe, or
//!    the runbook's timelock path). Nothing here submits it.

use std::collections::BTreeMap;
use std::str::FromStr;

use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, SolEvent};
use serde::Serialize;
use serde_json::json;

use crate::config::Config;
use crate::consensus_receipt::{BucketWeight, ConsensusReceipt, CANONICAL_BUCKET_ORDER};
use crate::gateway::{ConsensusRecommendationReceipt, PortfolioRouter, RouterGovernance};
use crate::output::emit;
use crate::rpc::{CallRequest, FailoverRpcClient, RawLog};

const EXIT_OK: i32 = 0;
const EXIT_REFUSAL: i32 = 2;
const EXIT_STARTUP_FAIL: i32 = 3;

/// `weight_bps` values sum to this across the canonical four buckets.
const BPS_DENOMINATOR: u32 = 10_000;

/// Bucket name → vault symbol, pinned by
/// `tests/fixtures/consensus-receipt.bucket-vault-map.json`. Declared as a
/// match rather than a map so an unrecognised bucket name is a compile-time
/// impossibility, not a silent lookup miss.
fn bucket_to_symbol(bucket: &str) -> Option<&'static str> {
    match bucket {
        "agent_tokens" => Some("rmAGENT"),
        "conservative_defi_yield" => Some("rmUSDC"),
        "protocol_tokens" => Some("rmPROTO"),
        "real_world_assets" => Some("rmRWA"),
        _ => None,
    }
}

// ─── Args ────────────────────────────────────────────────────────────────────

/// Where the receipt bytes come from — mirrors `commands::receipt::ReceiptSource`.
#[derive(Debug, Clone)]
pub enum ReceiptSource {
    Url(String),
    File(std::path::PathBuf),
}

/// Args for `rmpc governance draft-proposal`.
#[derive(Debug, Clone)]
pub struct Args {
    pub config_path: std::path::PathBuf,
    /// Single-receipt mode: the released receipt's id (0x-prefixed hex,
    /// exactly what `ReceiptReleased.receiptId` carries).
    pub receipt_id: Option<String>,
    /// Where to fetch the receipt payload for `--receipt-id` mode.
    pub source: Option<ReceiptSource>,
    /// Scan mode: earliest block to search `ReceiptReleased` logs from.
    pub from_block: Option<u64>,
    /// Scan mode: latest block to search to. Defaults to `"latest"`.
    pub to_block: Option<String>,
    /// Scan mode: HTTP URL template for fetching each released receipt's
    /// payload, with `{receipt_id}` substituted. Required when `--from-block`
    /// is used without `--receipt-url`/`--receipt-file`.
    pub receipt_url_template: Option<String>,
    pub pretty: bool,
}

// ─── Output types ────────────────────────────────────────────────────────────

/// One resolved (vault, bps) pair in a draft.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DraftVault {
    pub symbol: String,
    pub vault: String,
    pub weight_bps: u32,
}

/// The draft for a single released receipt.
#[derive(Debug, Clone, Serialize)]
pub struct Draft {
    pub receipt_id: String,
    pub session_id: String,
    pub subject_id: String,
    /// `"skipped_no_weights"`, `"ready_for_review"`, or `"blocked_active_proposal"`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub vaults: Vec<DraftVault>,
    pub excluded_vaults: Vec<DraftVault>,
    pub fallback_applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking_proposal_id: Option<String>,
    /// `RouterGovernance.propose(vaults, bps)` calldata, hex-encoded.
    /// Present only when `status == "ready_for_review"`. Never signed, never
    /// broadcast by this command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub propose_calldata: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DraftOutput {
    pub ok: bool,
    pub drafts: Vec<Draft>,
}

#[derive(Debug, Serialize)]
pub struct DraftFailure {
    pub ok: bool,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A single vault's eligibility check result, injected so the redistribution
/// logic is testable without an RPC connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EligibilityEntry {
    eligible: bool,
}

// ─── Redistribution (issue #1248 task 5.2 — "a defined fallback") ────────────

/// Drop ineligible vaults from `entries` and redistribute their combined bps
/// proportionally across the remaining eligible vaults (by original bps
/// share), settling the LAST remaining entry (in the input's — i.e. canonical
/// bucket — order) to the exact [`BPS_DENOMINATOR`] remainder so the vector
/// always sums correctly. Returns `(kept, excluded)`.
///
/// `Err(())` iff every vault is ineligible — there is nothing left to draft.
fn redistribute_excluding_ineligible(
    entries: Vec<DraftVault>,
    eligibility: &[EligibilityEntry],
) -> Result<(Vec<DraftVault>, Vec<DraftVault>), ()> {
    debug_assert_eq!(entries.len(), eligibility.len());

    let mut kept: Vec<DraftVault> = Vec::new();
    let mut excluded: Vec<DraftVault> = Vec::new();
    for (entry, elig) in entries.into_iter().zip(eligibility.iter()) {
        if elig.eligible {
            kept.push(entry);
        } else {
            excluded.push(entry);
        }
    }

    if kept.is_empty() {
        return Err(());
    }
    if excluded.is_empty() {
        return Ok((kept, excluded));
    }

    let kept_orig_total: u64 = kept.iter().map(|v| v.weight_bps as u64).sum();
    let n = kept.len();
    let mut running: u64 = 0;
    for (i, v) in kept.iter_mut().enumerate() {
        if i + 1 == n {
            // Settle the last kept vault to the exact remainder so the
            // vector always sums to BPS_DENOMINATOR, matching the receipt's
            // own settle-the-last-bucket bps conversion.
            let remainder = BPS_DENOMINATOR as u64 - running;
            v.weight_bps = remainder as u32;
        } else {
            // Proportional to this vault's ORIGINAL share of the vaults that
            // survive, rounded down; the remainder always lands on the last
            // vault above, never distributed as leftover dust here.
            let share = (v.weight_bps as u64 * BPS_DENOMINATOR as u64) / kept_orig_total.max(1);
            v.weight_bps = share as u32;
            running += share;
        }
    }

    Ok((kept, excluded))
}

// ─── Proposal-state replica (issue #1248 task 5.3 — queueing policy) ─────────

/// Mirrors `RouterGovernance._state()`. `propose()` reverts with
/// `ActiveProposalExists` exactly when this returns `Active` or `Queued`; the
/// draft worker replicates that read-only so it can report the same
/// blocking condition before a human ever calls `propose()`.
fn is_blocking_proposal_state(
    now: u64,
    voting_deadline: u64,
    executable_after: u64,
    votes_for: U256,
    snapshot_quorum: U256,
    executed: bool,
    cancelled: bool,
) -> bool {
    let _ = executable_after; // not needed to distinguish Active/Queued from terminal states
    if cancelled || executed {
        return false;
    }
    if now <= voting_deadline {
        return true; // Active
    }
    votes_for >= snapshot_quorum // Queued (Defeated is not blocking)
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Run `rmpc governance draft-proposal`. Returns the process exit code.
pub fn run(args: Args) -> i32 {
    let cfg = match Config::from_path(&args.config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc governance draft-proposal: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let receipt_addr = match required_address(cfg.receipt_address.as_deref(), "receipt_address") {
        Ok(a) => a,
        Err(msg) => {
            log::error!("rmpc governance draft-proposal: {msg}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let router_addr = match required_address(cfg.router_address.as_deref(), "router_address") {
        Ok(a) => a,
        Err(msg) => {
            log::error!("rmpc governance draft-proposal: {msg}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let governance_addr =
        match required_address(cfg.governance_address.as_deref(), "governance_address") {
            Ok(a) => a,
            Err(msg) => {
                log::error!("rmpc governance draft-proposal: {msg}");
                return EXIT_STARTUP_FAIL;
            }
        };
    let vault_addresses: BTreeMap<String, Address> = match &cfg.vault_addresses {
        Some(m) => match parse_vault_addresses(m) {
            Ok(v) => v,
            Err(e) => {
                log::error!("rmpc governance draft-proposal: {e}");
                return EXIT_STARTUP_FAIL;
            }
        },
        None => {
            log::error!(
                "rmpc governance draft-proposal: [vault_addresses] not set in config; \
                 add a per-deployment table (rmUSDC/rmPROTO/rmAGENT/rmRWA -> 0x...)"
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
            log::error!("rmpc governance draft-proposal: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc governance draft-proposal: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    // Resolve which receipt id(s) to draft for.
    let receipt_ids: Vec<(alloy_primitives::B256, Option<ReceiptSource>)> =
        if let Some(id) = &args.receipt_id {
            let parsed = match parse_b256(id) {
                Ok(b) => b,
                Err(msg) => {
                    log::error!("rmpc governance draft-proposal: --receipt-id: {msg}");
                    return EXIT_STARTUP_FAIL;
                }
            };
            vec![(parsed, args.source.clone())]
        } else if let Some(from_block) = args.from_block {
            let to_block = args
                .to_block
                .clone()
                .unwrap_or_else(|| "latest".to_string());
            match rt.block_on(scan_released(&rpc, receipt_addr, from_block, &to_block)) {
                Ok(ids) => ids
                    .into_iter()
                    .map(|id| {
                        let source = args.receipt_url_template.as_ref().map(|tmpl| {
                            ReceiptSource::Url(tmpl.replace("{receipt_id}", &format!("{id:#x}")))
                        });
                        (id, source)
                    })
                    .collect(),
                Err(e) => {
                    log::error!("rmpc governance draft-proposal: scan failed: {e}");
                    return EXIT_STARTUP_FAIL;
                }
            }
        } else {
            log::error!(
                "rmpc governance draft-proposal: exactly one of --receipt-id or --from-block \
                 is required"
            );
            return EXIT_STARTUP_FAIL;
        };

    let mut drafts = Vec::with_capacity(receipt_ids.len());
    for (receipt_id, source) in receipt_ids {
        let source = match source {
            Some(s) => s,
            None => {
                emit_failure(
                    &DraftFailure {
                        ok: false,
                        error: "ErrReceiptSourceMissing".to_string(),
                        message: Some(format!(
                            "receipt {receipt_id:#x}: no --receipt-url/--receipt-file/\
                             --receipt-url-template supplied"
                        )),
                    },
                    args.pretty,
                );
                return EXIT_REFUSAL;
            }
        };
        match rt.block_on(draft_one(
            &rpc,
            receipt_addr,
            router_addr,
            governance_addr,
            &vault_addresses,
            receipt_id,
            &source,
        )) {
            Ok(d) => drafts.push(d),
            Err(DraftError::Refusal { error, message }) => {
                emit_failure(
                    &DraftFailure {
                        ok: false,
                        error,
                        message: Some(message),
                    },
                    args.pretty,
                );
                return EXIT_REFUSAL;
            }
            Err(DraftError::StartupFail(msg)) => {
                log::error!("rmpc governance draft-proposal: {msg}");
                return EXIT_STARTUP_FAIL;
            }
        }
    }

    emit(&DraftOutput { ok: true, drafts }, args.pretty);
    EXIT_OK
}

enum DraftError {
    Refusal { error: String, message: String },
    StartupFail(String),
}

#[allow(clippy::too_many_arguments)]
async fn draft_one(
    rpc: &FailoverRpcClient,
    receipt_addr: Address,
    router_addr: Address,
    governance_addr: Address,
    vault_addresses: &BTreeMap<String, Address>,
    receipt_id: alloy_primitives::B256,
    source: &ReceiptSource,
) -> Result<Draft, DraftError> {
    // 1. Observe: the receipt must actually be released. Release is an admin
    //    discretion gate (D5); this worker never second-guesses it.
    let released = call_is_released(rpc, receipt_addr, receipt_id)
        .await
        .map_err(DraftError::StartupFail)?;
    if !released {
        return Err(DraftError::Refusal {
            error: "ErrReceiptNotReleased".to_string(),
            message: format!("receipt {receipt_id:#x} is not released; refusing to draft"),
        });
    }

    // 2. Fetch and validate the payload.
    let raw = match source {
        ReceiptSource::File(p) => std::fs::read(p).map_err(|e| DraftError::Refusal {
            error: "ErrReceiptReadFailed".to_string(),
            message: format!("read {}: {e}", p.display()),
        })?,
        ReceiptSource::Url(url) => fetch_url(url).await.map_err(|e| DraftError::Refusal {
            error: "ErrReceiptFetchFailed".to_string(),
            message: e,
        })?,
    };
    let receipt = ConsensusReceipt::from_json_slice(&raw).map_err(|e| DraftError::Refusal {
        error: e.code().to_string(),
        message: format!("{e}"),
    })?;
    receipt.validate().map_err(|e| DraftError::Refusal {
        error: e.code().to_string(),
        message: format!("{e}"),
    })?;
    let derived_id = receipt.receipt_id();
    if derived_id != receipt_id {
        return Err(DraftError::Refusal {
            error: "ErrReceiptIdMismatch".to_string(),
            message: format!(
                "fetched payload derives receipt_id {derived_id:#x}, expected {receipt_id:#x}"
            ),
        });
    }

    let weights = match &receipt.weights {
        Some(w) => w,
        None => {
            return Ok(Draft {
                receipt_id: format!("{receipt_id:#x}"),
                session_id: receipt.session_id.clone(),
                subject_id: receipt.subject_id.clone(),
                status: "skipped_no_weights".to_string(),
                reason: Some(
                    "receipt carries no weights vector — most receipts are published, not \
                     applied (§2.1); nothing to draft"
                        .to_string(),
                ),
                vaults: Vec::new(),
                excluded_vaults: Vec::new(),
                fallback_applied: false,
                blocking_proposal_id: None,
                propose_calldata: None,
            });
        }
    };

    // 3. Map buckets to vaults.
    let entries = match resolve_vault_entries(weights, vault_addresses) {
        Ok(e) => e,
        Err(msg) => {
            return Err(DraftError::Refusal {
                error: "ErrVaultMapIncomplete".to_string(),
                message: msg,
            })
        }
    };

    // 4. Re-check eligibility at draft time, block tag pinned once for a
    //    consistent read across every vault.
    let block_number = rpc
        .block_number()
        .await
        .map_err(|e| DraftError::StartupFail(format!("eth_blockNumber failed: {e}")))?;
    let block_tag = format!("0x{block_number:x}");
    let mut eligibility = Vec::with_capacity(entries.len());
    for e in &entries {
        let addr = Address::from_str(&e.vault)
            .map_err(|err| DraftError::StartupFail(format!("vault address {}: {err}", e.vault)))?;
        let ok = call_is_router_eligible_and_active(rpc, router_addr, addr, &block_tag)
            .await
            .map_err(DraftError::StartupFail)?;
        eligibility.push(EligibilityEntry { eligible: ok });
    }

    let (kept, excluded) =
        redistribute_excluding_ineligible(entries, &eligibility).map_err(|()| {
            DraftError::Refusal {
                error: "ErrNoEligibleVaults".to_string(),
                message: "every vault in the drafted vector is ineligible or non-Active; \
                      nothing left to propose"
                    .to_string(),
            }
        })?;
    let fallback_applied = !excluded.is_empty();

    // 5. Queueing policy: is there a blocking (Active/Queued) proposal?
    let blocking_id = current_blocking_proposal(rpc, governance_addr, &block_tag)
        .await
        .map_err(DraftError::StartupFail)?;

    let (status, calldata) = if let Some(_id) = &blocking_id {
        ("blocked_active_proposal".to_string(), None)
    } else {
        let vaults: Vec<Address> = kept
            .iter()
            .map(|v| Address::from_str(&v.vault).expect("validated above"))
            .collect();
        let bps: Vec<U256> = kept.iter().map(|v| U256::from(v.weight_bps)).collect();
        let calldata = RouterGovernance::proposeCall { vaults, bps }.abi_encode();
        (
            "ready_for_review".to_string(),
            Some(format!("0x{}", hex::encode(calldata))),
        )
    };

    Ok(Draft {
        receipt_id: format!("{receipt_id:#x}"),
        session_id: receipt.session_id.clone(),
        subject_id: receipt.subject_id.clone(),
        status,
        reason: None,
        vaults: kept,
        excluded_vaults: excluded,
        fallback_applied,
        blocking_proposal_id: blocking_id,
        propose_calldata: calldata,
    })
}

fn resolve_vault_entries(
    weights: &[BucketWeight],
    vault_addresses: &BTreeMap<String, Address>,
) -> Result<Vec<DraftVault>, String> {
    // CANONICAL_BUCKET_ORDER pins the iteration order so redistribution's
    // "settle the last" rule is deterministic and matches the receipt's own
    // canonical ordering, not JSON declaration order.
    let mut by_bucket: BTreeMap<&str, u32> = BTreeMap::new();
    for w in weights {
        by_bucket.insert(w.bucket.as_str(), w.weight_bps);
    }

    let mut out = Vec::with_capacity(CANONICAL_BUCKET_ORDER.len());
    for bucket in CANONICAL_BUCKET_ORDER {
        let weight_bps = *by_bucket
            .get(bucket)
            .ok_or_else(|| format!("weights vector is missing canonical bucket {bucket:?}"))?;
        let symbol = bucket_to_symbol(bucket)
            .unwrap_or_else(|| unreachable!("CANONICAL_BUCKET_ORDER only contains mapped buckets"));
        let addr = vault_addresses.get(symbol).ok_or_else(|| {
            format!("[vault_addresses] is missing required symbol {symbol:?} (bucket {bucket:?})")
        })?;
        out.push(DraftVault {
            symbol: symbol.to_string(),
            vault: format!("{addr:#x}"),
            weight_bps,
        });
    }
    Ok(out)
}

fn parse_vault_addresses(
    m: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Address>, String> {
    let mut out = BTreeMap::new();
    for (k, v) in m {
        let addr = Address::from_str(v).map_err(|e| format!("vault_addresses.{k}: {e}"))?;
        out.insert(k.clone(), addr);
    }
    Ok(out)
}

fn required_address(s: Option<&str>, field: &str) -> Result<Address, String> {
    let s = s.ok_or_else(|| {
        format!("{field} not set in config; add `{field} = \"0x...\"` to the operator TOML")
    })?;
    Address::from_str(s).map_err(|e| format!("{field} parse error: {e}"))
}

fn parse_b256(s: &str) -> Result<alloy_primitives::B256, String> {
    let hex = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(hex).map_err(|e| format!("hex decode error: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!(
            "must be exactly 32 bytes (64 hex chars), got {}",
            bytes.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(alloy_primitives::B256::from(arr))
}

async fn call_is_released(
    rpc: &FailoverRpcClient,
    receipt_addr: Address,
    receipt_id: alloy_primitives::B256,
) -> Result<bool, String> {
    let data = ConsensusRecommendationReceipt::isReleasedCall {
        receiptId: receipt_id,
    }
    .abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: receipt_addr,
                from: None,
                data: data.into(),
            },
            Some("latest"),
        )
        .await
        .map_err(|e| format!("eth_call(isReleased) failed: {e}"))?;
    let r = ConsensusRecommendationReceipt::isReleasedCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("isReleased abi decode: {e}"))?;
    Ok(r._0)
}

async fn call_is_router_eligible_and_active(
    rpc: &FailoverRpcClient,
    router_addr: Address,
    vault: Address,
    block_tag: &str,
) -> Result<bool, String> {
    let data = PortfolioRouter::isRouterEligibleAndActiveCall { vault }.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: router_addr,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await
        .map_err(|e| format!("eth_call(isRouterEligibleAndActive) failed: {e}"))?;
    let r = PortfolioRouter::isRouterEligibleAndActiveCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("isRouterEligibleAndActive abi decode: {e}"))?;
    Ok(r.ok)
}

/// Returns `Some(proposal_id)` iff the current proposal is `Active` or
/// `Queued` — the same condition under which `RouterGovernance.propose()`
/// itself would revert with `ActiveProposalExists`.
async fn current_blocking_proposal(
    rpc: &FailoverRpcClient,
    governance_addr: Address,
    block_tag: &str,
) -> Result<Option<String>, String> {
    let data = RouterGovernance::currentProposalIdCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: governance_addr,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await
        .map_err(|e| format!("eth_call(currentProposalId) failed: {e}"))?;
    let current_id = RouterGovernance::currentProposalIdCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("currentProposalId abi decode: {e}"))?
        ._0;
    if current_id.is_zero() {
        return Ok(None);
    }

    let data = RouterGovernance::activeProposalCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: governance_addr,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await
        .map_err(|e| format!("eth_call(activeProposal) failed: {e}"))?;
    let p = RouterGovernance::activeProposalCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("activeProposal abi decode: {e}"))?;

    let block_num_dec = u64::from_str_radix(block_tag.trim_start_matches("0x"), 16)
        .map_err(|e| format!("block_tag parse: {e}"))?;
    let now = rpc
        .block_timestamp(block_num_dec)
        .await
        .map_err(|e| format!("eth_getBlockByNumber failed: {e}"))?;

    let blocking = is_blocking_proposal_state(
        now,
        p.votingDeadline,
        p.executableAfter,
        p.votesFor,
        p.snapshotQuorum,
        p.executed,
        p.cancelled,
    );
    Ok(if blocking {
        Some(p.id.to_string())
    } else {
        None
    })
}

/// Scan `ReceiptReleased` logs in `[from_block, to_block]` and return every
/// `receiptId` found, in emission order.
async fn scan_released(
    rpc: &FailoverRpcClient,
    receipt_addr: Address,
    from_block: u64,
    to_block: &str,
) -> Result<Vec<alloy_primitives::B256>, String> {
    let topic0 = ConsensusRecommendationReceipt::ReceiptReleased::SIGNATURE_HASH;
    let filter = json!({
        "address": receipt_addr,
        "fromBlock": format!("0x{from_block:x}"),
        "toBlock": to_block,
        "topics": [topic0],
    });
    let logs: Vec<RawLog> = rpc
        .get_logs(filter)
        .await
        .map_err(|e| format!("eth_getLogs(ReceiptReleased) failed: {e}"))?;

    let mut ids = Vec::with_capacity(logs.len());
    for l in &logs {
        // topics[1] is the indexed receiptId.
        if let Some(id) = l.topics.get(1) {
            ids.push(*id);
        }
    }
    Ok(ids)
}

async fn fetch_url(url: &str) -> Result<Vec<u8>, String> {
    const MAX_BODY_BYTES: usize = 1024 * 1024;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url} returned {}", resp.status()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("read body from {url}: {e}"))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(format!(
            "receipt body from {url} is {} bytes (max {MAX_BODY_BYTES})",
            body.len()
        ));
    }
    Ok(body.to_vec())
}

fn emit_failure(out: &DraftFailure, pretty: bool) {
    emit(out, pretty);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(symbol: &str, addr_byte: u8, bps: u32) -> DraftVault {
        DraftVault {
            symbol: symbol.to_string(),
            vault: format!("{:#x}", Address::repeat_byte(addr_byte)),
            weight_bps: bps,
        }
    }

    #[test]
    fn redistribute_noop_when_all_eligible() {
        let entries = vec![vault("rmAGENT", 1, 2_500), vault("rmUSDC", 2, 7_500)];
        let elig = vec![
            EligibilityEntry { eligible: true },
            EligibilityEntry { eligible: true },
        ];
        let (kept, excluded) = redistribute_excluding_ineligible(entries.clone(), &elig).unwrap();
        assert_eq!(kept, entries);
        assert!(excluded.is_empty());
    }

    #[test]
    fn redistribute_drops_ineligible_and_settles_last() {
        // agent_tokens (2500) is Paused; the remaining 7500 (rmUSDC alone)
        // absorbs it, and the last kept vault settles to the exact remainder.
        let entries = vec![
            vault("rmAGENT", 1, 2_500),
            vault("rmUSDC", 2, 3_000),
            vault("rmPROTO", 3, 4_500),
        ];
        let elig = vec![
            EligibilityEntry { eligible: false },
            EligibilityEntry { eligible: true },
            EligibilityEntry { eligible: true },
        ];
        let (kept, excluded) = redistribute_excluding_ineligible(entries, &elig).unwrap();
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].symbol, "rmAGENT");
        assert_eq!(kept.len(), 2);
        let total: u32 = kept.iter().map(|v| v.weight_bps).sum();
        assert_eq!(total, BPS_DENOMINATOR);
        // rmUSDC: floor(3000 * 10000 / 7500) = 4000; rmPROTO settles the rest.
        assert_eq!(kept[0].weight_bps, 4_000);
        assert_eq!(kept[1].weight_bps, 6_000);
    }

    #[test]
    fn redistribute_refuses_when_every_vault_ineligible() {
        let entries = vec![vault("rmAGENT", 1, 5_000), vault("rmUSDC", 2, 5_000)];
        let elig = vec![
            EligibilityEntry { eligible: false },
            EligibilityEntry { eligible: false },
        ];
        assert!(redistribute_excluding_ineligible(entries, &elig).is_err());
    }

    #[test]
    fn redistribute_single_survivor_takes_everything() {
        let entries = vec![vault("rmAGENT", 1, 5_000), vault("rmUSDC", 2, 5_000)];
        let elig = vec![
            EligibilityEntry { eligible: false },
            EligibilityEntry { eligible: true },
        ];
        let (kept, excluded) = redistribute_excluding_ineligible(entries, &elig).unwrap();
        assert_eq!(excluded.len(), 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].weight_bps, BPS_DENOMINATOR);
    }

    #[test]
    fn blocking_state_active_before_deadline() {
        assert!(is_blocking_proposal_state(
            100,
            200,
            300,
            U256::from(0u64),
            U256::from(1u64),
            false,
            false
        ));
    }

    #[test]
    fn blocking_state_queued_after_quorum() {
        assert!(is_blocking_proposal_state(
            400,
            200,
            300,
            U256::from(5u64),
            U256::from(1u64),
            false,
            false
        ));
    }

    #[test]
    fn blocking_state_defeated_is_not_blocking() {
        assert!(!is_blocking_proposal_state(
            400,
            200,
            300,
            U256::from(0u64),
            U256::from(1u64),
            false,
            false
        ));
    }

    #[test]
    fn blocking_state_executed_or_cancelled_is_not_blocking() {
        assert!(!is_blocking_proposal_state(
            100,
            200,
            300,
            U256::from(5u64),
            U256::from(1u64),
            true,
            false
        ));
        assert!(!is_blocking_proposal_state(
            100,
            200,
            300,
            U256::from(5u64),
            U256::from(1u64),
            false,
            true
        ));
    }

    #[test]
    fn resolve_vault_entries_requires_all_four_canonical_buckets() {
        let mut vault_addresses = BTreeMap::new();
        vault_addresses.insert("rmAGENT".to_string(), Address::repeat_byte(1));
        vault_addresses.insert("rmUSDC".to_string(), Address::repeat_byte(2));
        vault_addresses.insert("rmPROTO".to_string(), Address::repeat_byte(3));
        vault_addresses.insert("rmRWA".to_string(), Address::repeat_byte(4));

        let weights = vec![
            BucketWeight {
                bucket: "agent_tokens".to_string(),
                weight_bps: 2_500,
            },
            BucketWeight {
                bucket: "conservative_defi_yield".to_string(),
                weight_bps: 2_500,
            },
            BucketWeight {
                bucket: "protocol_tokens".to_string(),
                weight_bps: 2_500,
            },
            BucketWeight {
                bucket: "real_world_assets".to_string(),
                weight_bps: 2_500,
            },
        ];
        let out = resolve_vault_entries(&weights, &vault_addresses).unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].symbol, "rmAGENT");
        assert_eq!(out[3].symbol, "rmRWA");
    }

    #[test]
    fn resolve_vault_entries_refuses_incomplete_vault_map() {
        let mut vault_addresses = BTreeMap::new();
        vault_addresses.insert("rmAGENT".to_string(), Address::repeat_byte(1));
        // rmUSDC/rmPROTO/rmRWA missing.
        let weights = vec![
            BucketWeight {
                bucket: "agent_tokens".to_string(),
                weight_bps: 2_500,
            },
            BucketWeight {
                bucket: "conservative_defi_yield".to_string(),
                weight_bps: 2_500,
            },
            BucketWeight {
                bucket: "protocol_tokens".to_string(),
                weight_bps: 2_500,
            },
            BucketWeight {
                bucket: "real_world_assets".to_string(),
                weight_bps: 2_500,
            },
        ];
        assert!(resolve_vault_entries(&weights, &vault_addresses).is_err());
    }

    #[test]
    fn run_fails_fast_without_receipt_address() {
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
        let args = Args {
            config_path: cfg_path,
            receipt_id: Some(format!("0x{}", "ab".repeat(32))),
            source: Some(ReceiptSource::Url("http://127.0.0.1:1/receipt".to_string())),
            from_block: None,
            to_block: None,
            receipt_url_template: None,
            pretty: false,
        };
        let code = run(args);
        assert_eq!(code, EXIT_STARTUP_FAIL);
    }

    #[test]
    fn run_fails_fast_without_receipt_id_or_from_block() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let keystore = tmp.path().join("keystore.json");
        let cfg_path = tmp.path().join("rmpc.toml");
        let toml = format!(
            r#"chain_id              = 31337
rpc_url               = "http://127.0.0.1:1"
gateway_address       = "0x000000000000000000000000000000000000dEaD"
usdc_address          = "0x{usdc}"
vault_address         = "0x{vault}"
receipt_address       = "0x000000000000000000000000000000000000dEaD"
router_address        = "0x000000000000000000000000000000000000dEaD"
governance_address    = "0x000000000000000000000000000000000000dEaD"
gateway_runtime_hash  = "0x{zeros}"
max_fee_per_gas_cap   = 100000000000

[signer]
allow_software_fallback = true
keystore_path           = "{ks}"

[vault_addresses]
rmUSDC  = "0x0000000000000000000000000000000000000001"
rmPROTO = "0x0000000000000000000000000000000000000002"
rmAGENT = "0x0000000000000000000000000000000000000003"
rmRWA   = "0x0000000000000000000000000000000000000004"
"#,
            usdc = "00".repeat(20),
            vault = "00".repeat(20),
            zeros = "0".repeat(64),
            ks = keystore.display(),
        );
        std::fs::write(&cfg_path, &toml).expect("write rmpc.toml");
        let args = Args {
            config_path: cfg_path,
            receipt_id: None,
            source: None,
            from_block: None,
            to_block: None,
            receipt_url_template: None,
            pretty: false,
        };
        let code = run(args);
        assert_eq!(code, EXIT_STARTUP_FAIL);
    }

    #[test]
    fn draft_output_serializes_correctly() {
        let out = DraftOutput {
            ok: true,
            drafts: vec![Draft {
                receipt_id: "0xabc".to_string(),
                session_id: "s1".to_string(),
                subject_id: "subj1".to_string(),
                status: "ready_for_review".to_string(),
                reason: None,
                vaults: vec![vault("rmUSDC", 2, 10_000)],
                excluded_vaults: vec![],
                fallback_applied: false,
                blocking_proposal_id: None,
                propose_calldata: Some("0xdeadbeef".to_string()),
            }],
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["drafts"][0]["status"], "ready_for_review");
        assert_eq!(v["drafts"][0]["propose_calldata"], "0xdeadbeef");
        assert!(v["drafts"][0].get("reason").is_none());
    }
}
