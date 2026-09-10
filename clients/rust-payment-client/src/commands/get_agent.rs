//! Canonical: Plan tracking issue #109 §9 — Phase 3 Direct Chain-Read Query Tooling
//! ADR: docs/technical/rmpc-read-output-contract.md
//! Security: docs/code-review/20260518-code-review-internal-codex.md §5 (agent-key compromise blast radius)
//!
//! `rmpc get-agent --agent 0x…` — direct on-chain read of an agent's
//! authorization record on the gateway, plus the current window's
//! deposit usage.
//!
//! Sub-reads (all `eth_call`, pinned to one block):
//!
//! - `agents(address)` → `(active, validUntil, maxPerPayment,
//!   maxPerWindow, shareReceiver, assetRecipient, maxWithdrawPerPayment,
//!   maxWithdrawPerWindow)` — the canonical authorization tuple. The
//!   withdrawal fields surface the agent-compromise blast radius
//!   identified in finding #5 of the 2026-05-18 coin-theft review.
//! - `eth_getBlockByNumber(blockNumber).timestamp` → used to compute the
//!   calendar `window_id = timestamp / WINDOW_SECONDS`, surfaced for
//!   operator context and reproducibility against the pinned block (not the
//!   daemon's wall clock — see ADR §3.4). It is **not** the source of
//!   `window_gross` (RPC-1, #1024).
//! - `effectiveDepositWindowGross(agent)` → cumulative deposit value the
//!   agent has put through the current **rolling** deposit window — the
//!   exposure the gateway enforces the deposit cap on. Issue #1024 (RPC-1)
//!   replaced the deprecated calendar mapping `agentWindowGross(agent,
//!   windowId)`, which reset at each `WINDOW_SECONDS` boundary regardless of
//!   the agent's deposit anchor and could diverge from the enforced value.
//! - `vault.allowance(agent, gateway)` → outstanding share allowance the
//!   agent has granted the gateway. Together with the policy withdrawal
//!   caps, this defines the maximum value an attacker who compromises
//!   the agent key can steal via `withdraw()` (issue #429).
//!
//! Output is the §9 envelope with `data: AgentData`. `maxPerPayment`,
//! `maxPerWindow`, `max_withdraw_per_payment`, `max_withdraw_per_window`,
//! `share_allowance`, and `window_gross` are `DecimalU256` so they
//! serialize as JSON strings.
//!
//! Exit codes: 0 (envelope, possibly partial), 3 (pre-read setup fail).

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::gateway::{Erc20, RobotMoneyGateway};
use crate::network_env::NetworkEnv;
use crate::output::emit;
use crate::policy::WINDOW_SECONDS;
use crate::read_output::{DecimalU256, Envelope, PartialBuilder};
use crate::rpc::{CallRequest, FailoverRpcClient};

const EXIT_OK: i32 = 0;
const EXIT_STARTUP_FAIL: i32 = 3;

/// `data` payload for `rmpc get-agent`. Field order matches the
/// operator-visible JSON; downstream snapshot tests assert on it.
#[derive(Debug, Default, Serialize)]
pub struct AgentData {
    /// Target agent address (lowercase 0x-hex).
    pub agent: String,
    /// `agents(agent).active` — false for unauthorized / revoked.
    pub active: bool,
    /// `agents(agent).validUntil` (unix seconds). `0` when the slot is
    /// unset; the contract treats `now > validUntil` as expired.
    pub valid_until: u64,
    /// `agents(agent).maxPerPayment` — `uint256` decimal string.
    pub max_per_payment: DecimalU256,
    /// `agents(agent).maxPerWindow` — `uint256` decimal string.
    pub max_per_window: DecimalU256,
    /// `agents(agent).shareReceiver` — lowercase 0x-hex.
    pub share_receiver: String,
    /// `agents(agent).assetRecipient` — lowercase 0x-hex. The USDC
    /// recipient when the agent calls `withdraw()`. Zero address
    /// signals withdrawals are disabled (issue #429).
    pub asset_recipient: String,
    /// `agents(agent).maxWithdrawPerPayment` — `uint256` decimal string.
    /// A non-zero value means the policy allows agent-initiated
    /// withdrawals; this is the per-call share cap.
    pub max_withdraw_per_payment: DecimalU256,
    /// `agents(agent).maxWithdrawPerWindow` — `uint256` decimal string.
    /// Per-window share cap; sets the agent-compromise blast radius
    /// for the current window.
    pub max_withdraw_per_window: DecimalU256,
    /// Derived: `maxWithdrawPerPayment > 0`. Operators MUST treat
    /// `true` as a security-relevant signal — an agent-key compromise
    /// can redeem shares up to the per-window cap while this is set.
    pub withdrawals_enabled: bool,
    /// The address whose vault allowance to the gateway is reported in
    /// `share_allowance`. For router-withdrawal policies (`shareReceiver !=
    /// agent`) the gateway pulls shares from `shareReceiver`, so the
    /// relevant allowance owner is `shareReceiver`. For direct-vault policies
    /// (`shareReceiver == agent`) the gateway pulls from the agent, so the
    /// owner is the agent address. (AZ-GW-2, issue #1069.)
    pub share_allowance_owner: String,
    /// Outstanding vault share allowance granted to the gateway by
    /// `share_allowance_owner`. For direct-vault policies this is
    /// `vault.allowance(agent, gateway)`; for router-withdrawal policies
    /// this is `vault.allowance(shareReceiver, gateway)` — the allowance
    /// the gateway actually spends when `withdrawFromRouter` is called.
    /// Combined with `max_withdraw_per_payment`/`max_withdraw_per_window`
    /// this is the bound on what a compromised agent can withdraw without
    /// further on-chain action by the depositor (issue #429, AZ-GW-2, issue #1069).
    pub share_allowance: DecimalU256,
    /// Calendar window id at the pinned block:
    /// `block_timestamp / WINDOW_SECONDS`. Retained for operator context
    /// only; it is **not** the source of `window_gross` (RPC-1, #1024).
    pub window_id: u64,
    /// `effectiveDepositWindowGross(agent)` — the agent's cumulative
    /// deposit gross against the current **rolling** window (the value the
    /// gateway enforces the deposit cap on), as a `uint256` decimal string.
    /// Issue #1024 (RPC-1) switched this from the deprecated calendar-window
    /// mapping `agentWindowGross(agent, window_id)` to the rolling-window
    /// effective view already used by the preflight (`policy/mod.rs`), so the
    /// reported figure matches the gateway's actual admit/deny exposure.
    pub window_gross: DecimalU256,
}

pub fn run(config_path: &Path, agent_hex: &str, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-agent: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let agent = match Address::from_str(agent_hex) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc get-agent: --agent parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let gateway_addr = match Address::from_str(&cfg.gateway_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc get-agent: gateway_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let vault_addr = match Address::from_str(&cfg.vault_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc get-agent: vault_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc get-agent: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-agent: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc get-agent: network environment: {} (chain_id={})",
        network_env.human_label(),
        cfg.chain_id
    );
    let env = match rt.block_on(read_agent(&rpc, gateway_addr, vault_addr, agent)) {
        Ok(e) => e,
        Err(e) => {
            log::error!("rmpc get-agent: pre-read setup failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    emit(&env, pretty);
    EXIT_OK
}

async fn read_agent(
    rpc: &FailoverRpcClient,
    gateway: Address,
    vault: Address,
    agent: Address,
) -> crate::errors::Result<Envelope<AgentData>> {
    let chain_id = rpc.chain_id().await?;
    let block_number = rpc.block_number().await?;
    let block_tag = format!("0x{block_number:x}");

    let data = AgentData {
        agent: format!("{agent:#x}"),
        ..Default::default()
    };
    let mut b = PartialBuilder::new(chain_id, block_number, data);

    // agents() tuple — now an 8-field record including the withdrawal
    // policy fields (assetRecipient + maxWithdrawPerPayment +
    // maxWithdrawPerWindow). Issue #429: surface this so operators
    // can see withdrawal-enabled policies in `rmpc get-agent`.
    //
    // AZ-GW-2 (issue #1069): after reading the agents tuple we can
    // determine whether this is a router-withdrawal policy by comparing
    // shareReceiver to the agent address. The gateway's `withdrawFromRouter`
    // path pulls shares from `policy.shareReceiver`, so for router-withdrawal
    // policies the relevant blast-radius allowance is
    // `vault.allowance(shareReceiver, gateway)`, not `vault.allowance(agent,
    // gateway)`. The `share_allowance_owner` field surfaces which address was
    // queried so operators and the dapp can distinguish the two cases.
    let mut allowance_owner = agent; // default: direct-vault (shareReceiver == agent)
    match call_agents(rpc, gateway, &block_tag, agent).await {
        Ok(t) => {
            b.data_mut().active = t.active;
            b.data_mut().valid_until = t.valid_until;
            b.data_mut().max_per_payment = DecimalU256(t.max_per_payment);
            b.data_mut().max_per_window = DecimalU256(t.max_per_window);
            b.data_mut().share_receiver = format!("{:#x}", t.share_receiver);
            b.data_mut().asset_recipient = format!("{:#x}", t.asset_recipient);
            b.data_mut().max_withdraw_per_payment = DecimalU256(t.max_withdraw_per_payment);
            b.data_mut().max_withdraw_per_window = DecimalU256(t.max_withdraw_per_window);
            // `withdrawals_enabled` mirrors the on-chain gateway guard
            // (`if (p.maxWithdrawPerPayment == 0) revert
            // WithdrawalNotEnabled()`). Derived rather than stored so
            // it can never drift from the canonical field.
            b.data_mut().withdrawals_enabled = !t.max_withdraw_per_payment.is_zero();
            // AZ-GW-2 / issue #1069: router-withdrawal policies hold shares
            // at `shareReceiver` (not the agent). The single-vault `withdraw`
            // path requires `shareReceiver == agent` (the contract comment at
            // RobotMoneyGateway.sol:1057 confirms this), so `shareReceiver !=
            // agent` is the reliable indicator of a router-withdrawal policy.
            if t.share_receiver != agent {
                allowance_owner = t.share_receiver;
            }
            b.data_mut().share_allowance_owner = format!("{:#x}", allowance_owner);
        }
        Err(e) => {
            // agents() call failed — record the share_allowance_owner as the
            // agent address so the partial output is still meaningful.
            b.data_mut().share_allowance_owner = format!("{agent:#x}");
            b.record_err("agents", e);
        }
    }

    // Share allowance on the pinned vault. Read even when withdrawals are
    // disabled — a leftover non-zero allowance with a future re-enable is
    // still part of the blast radius operators need to see (issue #429:
    // "revoke stale gateway share allowances").
    //
    // AZ-GW-2 / issue #1069: use `allowance_owner` (shareReceiver for
    // router-withdrawal policies, agent for direct-vault) so the reported
    // allowance matches what the gateway actually spends.
    match call_share_allowance(rpc, vault, &block_tag, allowance_owner, gateway).await {
        Ok(v) => b.data_mut().share_allowance = DecimalU256(v),
        Err(e) => b.record_err("share_allowance", e),
    }

    // Calendar window id from the chain timestamp. Retained for operator
    // context (RPC-1, #1024): the on-chain deposit cap is enforced on a
    // rolling window, so this calendar bucket is no longer the source of
    // `window_gross` — it is surfaced for reproducibility against the pinned
    // block only.
    match rpc.block_timestamp(block_number).await {
        Ok(ts) => b.data_mut().window_id = ts / WINDOW_SECONDS,
        Err(e) => b.record_err("window_id", format!("eth_getBlockByNumber failed: {e}")),
    }

    // Rolling-window deposit gross — the value the gateway actually enforces
    // the deposit cap against (RPC-1, #1024). Read from the parameterless
    // `effectiveDepositWindowGross(agent)` view (the deposit analogue of the
    // withdraw rolling read migrated under #449), not the deprecated
    // per-calendar-window `agentWindowGross(agent, window_id)` mapping. The
    // read is independent of `window_id`, so it runs unconditionally.
    match call_effective_deposit_window_gross(rpc, gateway, &block_tag, agent).await {
        Ok(v) => b.data_mut().window_gross = DecimalU256(v),
        Err(e) => b.record_err("window_gross", e),
    }

    Ok(b.finish())
}

struct AgentTuple {
    active: bool,
    valid_until: u64,
    max_per_payment: U256,
    max_per_window: U256,
    share_receiver: Address,
    asset_recipient: Address,
    max_withdraw_per_payment: U256,
    max_withdraw_per_window: U256,
}

async fn call_agents(
    rpc: &FailoverRpcClient,
    gateway: Address,
    block_tag: &str,
    agent: Address,
) -> std::result::Result<AgentTuple, String> {
    let data = RobotMoneyGateway::agentsCall { _0: agent }.abi_encode();
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
    let r = RobotMoneyGateway::agentsCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(AgentTuple {
        active: r.active,
        valid_until: r.validUntil,
        max_per_payment: r.maxPerPayment,
        max_per_window: r.maxPerWindow,
        share_receiver: r.shareReceiver,
        asset_recipient: r.assetRecipient,
        max_withdraw_per_payment: r.maxWithdrawPerPayment,
        max_withdraw_per_window: r.maxWithdrawPerWindow,
    })
}

/// `vault.allowance(agent, gateway)` — the outstanding share allowance
/// the agent has granted the gateway. Issue #429: surfacing this
/// quantifies the agent-compromise blast radius for the withdrawal
/// path (a stolen agent key can redeem up to
/// `min(share_allowance, max_withdraw_per_window)` shares).
async fn call_share_allowance(
    rpc: &FailoverRpcClient,
    vault: Address,
    block_tag: &str,
    owner: Address,
    spender: Address,
) -> std::result::Result<U256, String> {
    let data = Erc20::allowanceCall { owner, spender }.abi_encode();
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
    let r = Erc20::allowanceCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(r._0)
}

/// `effectiveDepositWindowGross(agent)` — the agent's cumulative deposit
/// gross against the current **rolling** deposit window. Issue #1024 (RPC-1)
/// replaced the deprecated calendar-window mapping
/// `agentWindowGross(agent, window_id)` with this parameterless rolling-window
/// view (the deposit analogue of the withdraw read migrated under #449), so
/// the reported `window_gross` matches the exposure the gateway actually
/// enforces the deposit cap on — the same view the preflight uses
/// (`policy/mod.rs`).
async fn call_effective_deposit_window_gross(
    rpc: &FailoverRpcClient,
    gateway: Address,
    block_tag: &str,
    agent: Address,
) -> std::result::Result<U256, String> {
    let data = RobotMoneyGateway::effectiveDepositWindowGrossCall { agent }.abi_encode();
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
    let r = RobotMoneyGateway::effectiveDepositWindowGrossCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(r._0)
}
