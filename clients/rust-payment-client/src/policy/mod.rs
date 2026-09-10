//! Canonical: docs/architecture.md §9 — Client Preflight
//! (See also: Plan tracking issue #109 §4.4 — Preflight)
//!
//! `policy` module — client-side preflight that mirrors the on-chain
//! `RobotMoneyGateway` policy.
//!
//! Per `Plan tracking issue #109` §3.4 and issue #14: every check
//! the contract enforces at execution time is replayed by `rmpc` *before*
//! any signature is produced. A failure is a **hard refusal** — the daemon
//! exits non-zero, emits a high-severity log line, and never broadcasts.
//!
//! The contract being authoritative does not justify shipping a
//! transaction the client cannot prove is going to the audited bytecode;
//! the `keccak256(eth_getCode(gateway))` pin is therefore a non-negotiable
//! gate that runs before every send.
//!
//! Checks performed (issue #14, doc §3.4):
//!
//! 1. `eth_chainId` matches `config.chain_id`
//! 2. `keccak256(eth_getCode(gateway))` matches `config.gateway_runtime_hash`
//!    (eth_getCode returning empty bytecode is itself a refusal)
//! 3. `gateway.paused() == false`
//! 4. `gateway.usdc()` matches `config.usdc_address`
//! 5. `gateway.vault()` matches `config.vault_address`
//! 6. `gateway.agents(self).active && validUntil >= now`
//! 7. `amount <= agents(self).maxPerPayment`
//! 8. `effectiveDepositWindowGross(self) + amount <= maxPerWindow`
//!    (issue #497: rolling-window gross replaces per-calendar-window mapping)
//! 9. `usdc.allowance(self, gateway) >= amount`
//! 10. `usdc.balanceOf(self) >= amount`
//!
//! For withdrawals, checks 7–8 use the withdrawal-specific cap fields
//! `maxWithdrawPerPayment` and `maxWithdrawPerWindow` together with
//! `agentWithdrawWindowGross` instead of the deposit fields. See
//! `run_withdraw_gateway` and issue #371.
//!
//! Each rule maps onto a specific [`RmpcError`] variant. Operator tooling
//! matches on those names; renaming them is a breaking change.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{keccak256, Address, U256};
use alloy_sol_types::SolCall;

use crate::config::Config;
use crate::errors::{Result, RmpcError};
use crate::gateway::{Erc20, RobotMoneyGateway};
use crate::rpc::{CallRequest, FailoverRpcClient};
use serde::Serialize;

/// Window length in seconds. Mirrors `RobotMoneyGateway.WINDOW_SECONDS`
/// (constant on-chain, baked into the contract). Re-reading it on every
/// preflight would be a wasted RPC; if the contract redeploys with a
/// different constant the pinned `gateway_runtime_hash` will already
/// reject before we get this far.
pub const WINDOW_SECONDS: u64 = 86_400;

/// Inputs that vary per-invocation. `signer_address` is the EOA the
/// software signer will sign with; `amount` is the deposit value in USDC's
/// smallest unit (6 decimals).
#[derive(Debug, Clone, Copy)]
pub struct PreflightInputs {
    pub signer_address: Address,
    pub amount: U256,
}

/// Structured report returned on success. Callers (self-check, deposit,
/// withdraw) can introspect the on-chain state the preflight observed without
/// re-issuing the same RPCs.
#[derive(Debug, Clone)]
pub struct PreflightReport {
    pub chain_id: u64,
    pub gateway_runtime_hash_ok: bool,
    pub paused: bool,
    pub agent_active: bool,
    pub agent_valid_until: u64,
    pub max_per_payment: U256,
    pub max_per_window: U256,
    pub window_gross: U256,
    pub allowance: U256,
    pub balance: U256,
}

/// Preflight runner — stateless façade over [`FailoverRpcClient`]. Construct once
/// per command invocation; cheap to clone.
#[derive(Debug, Clone)]
pub struct Preflight<'a> {
    rpc: &'a FailoverRpcClient,
    config: &'a Config,
}

impl<'a> Preflight<'a> {
    pub fn new(rpc: &'a FailoverRpcClient, config: &'a Config) -> Self {
        Self { rpc, config }
    }

    /// Execute every preflight rule. Returns on the first refusal. The
    /// order is: cheap chain-level checks first (chain id, code hash,
    /// paused, addresses), then per-agent reads, then balance/allowance.
    /// This minimises wasted RPC on the unhappy path.
    pub async fn run(&self, inputs: PreflightInputs) -> Result<PreflightReport> {
        self.run_inner(inputs, true).await
    }

    /// Gateway-level preflight only (checks 1–8). Skips the USDC
    /// allowance and balance checks (9–10) which are deposit-specific.
    /// Withdraw callers use this and then run vault-specific checks
    /// separately via `withdraw_vault_preflight`.
    pub async fn run_gateway_only(&self, inputs: PreflightInputs) -> Result<PreflightReport> {
        self.run_inner(inputs, false).await
    }

    /// Withdraw-specific gateway preflight. Runs checks 1–6 (chain id, code
    /// hash, paused, usdc/vault addresses, agent active+expiry) then checks
    /// 7–8 using the withdrawal-specific caps:
    ///   7w. `shares <= agents(self).maxWithdrawPerPayment`
    ///   8w. `agentWithdrawWindowGross(self, window) + shares <=
    ///         agents(self).maxWithdrawPerWindow`
    ///
    /// USDC allowance/balance checks are skipped (N/A for withdrawals).
    /// Vault-level share checks run separately in `withdraw_vault_preflight`.
    ///
    /// Addresses issue #371: the old `run_gateway_only` path checked the
    /// deposit caps (`maxPerPayment`, `maxPerWindow`) instead of the
    /// withdrawal caps, which could reject valid withdrawals and pass
    /// invalid ones.
    pub async fn run_withdraw_gateway(&self, inputs: PreflightInputs) -> Result<PreflightReport> {
        self.run_withdraw_inner(inputs).await
    }

    /// Inner implementation of the withdrawal gateway preflight.
    /// Uses `maxWithdrawPerPayment`, `maxWithdrawPerWindow`, and
    /// `agentWithdrawWindowGross` for cap checks (issue #371).
    async fn run_withdraw_inner(&self, inputs: PreflightInputs) -> Result<PreflightReport> {
        // 1. chain id
        let chain_id = self.rpc.chain_id().await?;
        if chain_id != self.config.chain_id {
            return Err(RmpcError::ErrChainIdMismatch);
        }

        let gateway_addr = parse_addr(&self.config.gateway_address, "gateway_address")?;

        // 2. code hash pin
        let code = self.rpc.get_code(gateway_addr, None).await?;
        if code.is_empty() {
            return Err(RmpcError::ErrCodeHashMismatch);
        }
        let observed_hash = keccak256(code.as_ref());
        let expected_hash = parse_b256_hex(&self.config.gateway_runtime_hash)?;
        if observed_hash.as_slice() != expected_hash.as_slice() {
            return Err(RmpcError::ErrCodeHashMismatch);
        }

        // 3. paused()
        let paused = self.call_view_paused(gateway_addr).await?;
        if paused {
            return Err(RmpcError::ErrGatewayPaused);
        }

        // 4-5. usdc()/vault() addresses pinned in config
        let usdc_addr_on_chain = self.call_view_usdc(gateway_addr).await?;
        let usdc_addr_cfg = parse_addr(&self.config.usdc_address, "usdc_address")?;
        if usdc_addr_on_chain != usdc_addr_cfg {
            return Err(RmpcError::ErrConfig(format!(
                "gateway.usdc() = {usdc_addr_on_chain:?} does not match configured usdc_address = {usdc_addr_cfg:?}"
            )));
        }
        let vault_addr_on_chain = self.call_view_vault(gateway_addr).await?;
        let vault_addr_cfg = parse_addr(&self.config.vault_address, "vault_address")?;
        if vault_addr_on_chain != vault_addr_cfg {
            return Err(RmpcError::ErrConfig(format!(
                "gateway.vault() = {vault_addr_on_chain:?} does not match configured vault_address = {vault_addr_cfg:?}"
            )));
        }

        // 6. agents(self) — active + validUntil
        let agent = self
            .call_view_agents(gateway_addr, inputs.signer_address)
            .await?;
        if !agent.active {
            return Err(RmpcError::ErrAgentNotAuthorized);
        }
        let now = now_unix();
        if (agent.validUntil as u64) < now {
            return Err(RmpcError::ErrAgentNotAuthorized);
        }

        // 7w. shares <= maxWithdrawPerPayment
        if inputs.amount > agent.maxWithdrawPerPayment {
            return Err(RmpcError::ErrConfig(format!(
                "shares {} exceeds agent maxWithdrawPerPayment {}",
                inputs.amount, agent.maxWithdrawPerPayment,
            )));
        }

        // 8w. effectiveWithdrawWindowGross + shares <= maxWithdrawPerWindow.
        //     Issue #449 — the on-chain cap is now enforced on a strict
        //     rolling window, so the preflight reads the rolling gross
        //     (zero when the agent's anchor has aged past WINDOW_SECONDS)
        //     instead of the per-calendar-window mapping.
        let window_gross = self
            .call_view_agent_withdraw_window_gross(gateway_addr, inputs.signer_address)
            .await?;
        let projected = window_gross.saturating_add(inputs.amount);
        if projected > agent.maxWithdrawPerWindow {
            return Err(RmpcError::ErrConfig(format!(
                "rollingWithdrawGross {} + shares {} exceeds maxWithdrawPerWindow {}",
                window_gross, inputs.amount, agent.maxWithdrawPerWindow,
            )));
        }

        Ok(PreflightReport {
            chain_id,
            gateway_runtime_hash_ok: true,
            paused: false,
            agent_active: agent.active,
            agent_valid_until: agent.validUntil,
            max_per_payment: agent.maxWithdrawPerPayment,
            max_per_window: agent.maxWithdrawPerWindow,
            window_gross,
            allowance: U256::ZERO,
            balance: U256::ZERO,
        })
    }

    async fn run_inner(
        &self,
        inputs: PreflightInputs,
        check_usdc: bool,
    ) -> Result<PreflightReport> {
        // 1. chain id
        let chain_id = self.rpc.chain_id().await?;
        if chain_id != self.config.chain_id {
            return Err(RmpcError::ErrChainIdMismatch);
        }

        let gateway_addr = parse_addr(&self.config.gateway_address, "gateway_address")?;

        // 2. code hash pin
        let code = self.rpc.get_code(gateway_addr, None).await?;
        if code.is_empty() {
            return Err(RmpcError::ErrCodeHashMismatch);
        }
        let observed_hash = keccak256(code.as_ref());
        let expected_hash = parse_b256_hex(&self.config.gateway_runtime_hash)?;
        if observed_hash.as_slice() != expected_hash.as_slice() {
            return Err(RmpcError::ErrCodeHashMismatch);
        }

        // 3. paused()
        let paused = self.call_view_paused(gateway_addr).await?;
        if paused {
            return Err(RmpcError::ErrGatewayPaused);
        }

        // 4-5. usdc()/vault() addresses pinned in config
        let usdc_addr_on_chain = self.call_view_usdc(gateway_addr).await?;
        let usdc_addr_cfg = parse_addr(&self.config.usdc_address, "usdc_address")?;
        if usdc_addr_on_chain != usdc_addr_cfg {
            return Err(RmpcError::ErrConfig(format!(
                "gateway.usdc() = {usdc_addr_on_chain:?} does not match configured usdc_address = {usdc_addr_cfg:?}"
            )));
        }
        let vault_addr_on_chain = self.call_view_vault(gateway_addr).await?;
        let vault_addr_cfg = parse_addr(&self.config.vault_address, "vault_address")?;
        if vault_addr_on_chain != vault_addr_cfg {
            return Err(RmpcError::ErrConfig(format!(
                "gateway.vault() = {vault_addr_on_chain:?} does not match configured vault_address = {vault_addr_cfg:?}"
            )));
        }

        // 6. agents(self) — active + validUntil
        let agent = self
            .call_view_agents(gateway_addr, inputs.signer_address)
            .await?;
        if !agent.active {
            return Err(RmpcError::ErrAgentNotAuthorized);
        }
        let now = now_unix();
        if (agent.validUntil as u64) < now {
            return Err(RmpcError::ErrAgentNotAuthorized);
        }

        // 7. amount <= maxPerPayment
        if inputs.amount > agent.maxPerPayment {
            return Err(RmpcError::ErrConfig(format!(
                "amount {} exceeds agent maxPerPayment {}",
                inputs.amount, agent.maxPerPayment,
            )));
        }

        // 8. effectiveDepositWindowGross + amount <= maxPerWindow
        //    Issue #497 — deposit accounting switched to a rolling window
        //    (agentDepositWindow) matching the withdrawal-side pattern. The
        //    preflight now reads the rolling gross via effectiveDepositWindowGross
        //    instead of the deprecated per-calendar-window agentWindowGross.
        let window_gross = self
            .call_view_agent_deposit_window_gross(gateway_addr, inputs.signer_address)
            .await?;
        let projected = window_gross.saturating_add(inputs.amount);
        if projected > agent.maxPerWindow {
            return Err(RmpcError::ErrConfig(format!(
                "rollingDepositGross {} + amount {} exceeds maxPerWindow {}",
                window_gross, inputs.amount, agent.maxPerWindow,
            )));
        }

        let (allowance, balance) = if check_usdc {
            // 9. allowance(self, gateway) >= amount
            let allowance = self
                .call_view_allowance(usdc_addr_cfg, inputs.signer_address, gateway_addr)
                .await?;
            if allowance < inputs.amount {
                return Err(RmpcError::ErrAllowanceInsufficient);
            }

            // 10. balanceOf(self) >= amount
            let balance = self
                .call_view_balance_of(usdc_addr_cfg, inputs.signer_address)
                .await?;
            if balance < inputs.amount {
                return Err(RmpcError::ErrBalanceInsufficient);
            }
            (allowance, balance)
        } else {
            (U256::ZERO, U256::ZERO)
        };

        Ok(PreflightReport {
            chain_id,
            gateway_runtime_hash_ok: true,
            paused: false,
            agent_active: agent.active,
            agent_valid_until: agent.validUntil,
            max_per_payment: agent.maxPerPayment,
            max_per_window: agent.maxPerWindow,
            window_gross,
            allowance,
            balance,
        })
    }

    /// Vault-side preflight for a redemption leg (issue #312, #1285):
    ///
    /// 1. `vault.paused() == false`
    /// 2. `vault.allowance(agent, gateway) >= shares`
    /// 3. `vault.balanceOf(agent) >= shares`
    ///
    /// The redeem burns the agent's *vault shares*, which the gateway
    /// pulls from the source vault, so these are the share-side mirror of
    /// the USDC allowance/balance checks in [`Self::run`]. `withdraw` runs
    /// it once for the source vault; `withdraw-router` runs it per
    /// identity-bound `(vault, shares)` leg.
    ///
    /// This is the policy layer's rule. It previously lived in
    /// `commands::withdraw` with its own private copies of the three
    /// `eth_call` decoders, which made `commands::withdraw_router` import
    /// a policy rule from a sibling command module.
    pub async fn run_withdraw_vault(
        &self,
        vault: Address,
        gateway: Address,
        agent: Address,
        shares: U256,
    ) -> Result<()> {
        if self.call_view_paused(vault).await? {
            return Err(RmpcError::ErrVaultPaused);
        }
        if self.call_view_allowance(vault, agent, gateway).await? < shares {
            return Err(RmpcError::ErrShareAllowanceInsufficient);
        }
        if self.call_view_balance_of(vault, agent).await? < shares {
            return Err(RmpcError::ErrShareBalanceInsufficient);
        }
        Ok(())
    }

    // --- typed view helpers ---------------------------------------------

    async fn call_view_paused(&self, gateway: Address) -> Result<bool> {
        let data = RobotMoneyGateway::pausedCall {}.abi_encode();
        let out = self
            .rpc
            .eth_call(
                &CallRequest {
                    to: gateway,
                    from: None,
                    data: data.into(),
                },
                None,
            )
            .await?;
        let decoded = RobotMoneyGateway::pausedCall::abi_decode_returns(&out, true)
            .map_err(|e| RmpcError::ErrRpcDecode(format!("paused() decode: {e}")))?;
        Ok(decoded._0)
    }

    async fn call_view_usdc(&self, gateway: Address) -> Result<Address> {
        let data = RobotMoneyGateway::usdcCall {}.abi_encode();
        let out = self
            .rpc
            .eth_call(
                &CallRequest {
                    to: gateway,
                    from: None,
                    data: data.into(),
                },
                None,
            )
            .await?;
        let decoded = RobotMoneyGateway::usdcCall::abi_decode_returns(&out, true)
            .map_err(|e| RmpcError::ErrRpcDecode(format!("usdc() decode: {e}")))?;
        Ok(decoded._0)
    }

    async fn call_view_vault(&self, gateway: Address) -> Result<Address> {
        let data = RobotMoneyGateway::vaultCall {}.abi_encode();
        let out = self
            .rpc
            .eth_call(
                &CallRequest {
                    to: gateway,
                    from: None,
                    data: data.into(),
                },
                None,
            )
            .await?;
        let decoded = RobotMoneyGateway::vaultCall::abi_decode_returns(&out, true)
            .map_err(|e| RmpcError::ErrRpcDecode(format!("vault() decode: {e}")))?;
        Ok(decoded._0)
    }

    async fn call_view_agents(
        &self,
        gateway: Address,
        agent: Address,
    ) -> Result<RobotMoneyGateway::agentsReturn> {
        let data = RobotMoneyGateway::agentsCall { _0: agent }.abi_encode();
        let out = self
            .rpc
            .eth_call(
                &CallRequest {
                    to: gateway,
                    from: None,
                    data: data.into(),
                },
                None,
            )
            .await?;
        RobotMoneyGateway::agentsCall::abi_decode_returns(&out, true)
            .map_err(|e| RmpcError::ErrRpcDecode(format!("agents() decode: {e}")))
    }

    async fn call_view_agent_window_gross(
        &self,
        gateway: Address,
        agent: Address,
        window_id: u64,
    ) -> Result<U256> {
        let data = RobotMoneyGateway::agentWindowGrossCall {
            _0: agent,
            _1: window_id,
        }
        .abi_encode();
        let out = self
            .rpc
            .eth_call(
                &CallRequest {
                    to: gateway,
                    from: None,
                    data: data.into(),
                },
                None,
            )
            .await?;
        let decoded = RobotMoneyGateway::agentWindowGrossCall::abi_decode_returns(&out, true)
            .map_err(|e| RmpcError::ErrRpcDecode(format!("agentWindowGross decode: {e}")))?;
        Ok(decoded._0)
    }

    /// Read the agent's rolling-window deposit gross from the gateway
    /// via `effectiveDepositWindowGross(agent)`. Issue #497 replaced the
    /// fixed `agentWindowGross(agent, windowId)` mapping with a
    /// rolling-window accumulator anchored on the agent's first deposit
    /// of each window — the view returns the cumulative USDC deposited
    /// against the current rolling cap (zero when the anchor has aged out).
    async fn call_view_agent_deposit_window_gross(
        &self,
        gateway: Address,
        agent: Address,
    ) -> Result<U256> {
        let data = RobotMoneyGateway::effectiveDepositWindowGrossCall { agent }.abi_encode();
        let out = self
            .rpc
            .eth_call(
                &CallRequest {
                    to: gateway,
                    from: None,
                    data: data.into(),
                },
                None,
            )
            .await?;
        let decoded =
            RobotMoneyGateway::effectiveDepositWindowGrossCall::abi_decode_returns(&out, true)
                .map_err(|e| {
                    RmpcError::ErrRpcDecode(format!("effectiveDepositWindowGross decode: {e}"))
                })?;
        Ok(decoded._0)
    }

    /// Read the agent's rolling-window withdrawal gross from the gateway
    /// via `effectiveWithdrawWindowGross(agent)`. Issue #449 replaced the
    /// fixed `agentWithdrawWindowGross(agent, windowId)` mapping with a
    /// rolling-window accumulator anchored on the agent's first withdrawal
    /// of each window — the view returns the cumulative shares redeemed
    /// against the current rolling cap (zero when the anchor has aged out).
    async fn call_view_agent_withdraw_window_gross(
        &self,
        gateway: Address,
        agent: Address,
    ) -> Result<U256> {
        let data = RobotMoneyGateway::effectiveWithdrawWindowGrossCall { agent }.abi_encode();
        let out = self
            .rpc
            .eth_call(
                &CallRequest {
                    to: gateway,
                    from: None,
                    data: data.into(),
                },
                None,
            )
            .await?;
        let decoded =
            RobotMoneyGateway::effectiveWithdrawWindowGrossCall::abi_decode_returns(&out, true)
                .map_err(|e| {
                    RmpcError::ErrRpcDecode(format!("effectiveWithdrawWindowGross decode: {e}"))
                })?;
        Ok(decoded._0)
    }

    async fn call_view_allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<U256> {
        let data = Erc20::allowanceCall { owner, spender }.abi_encode();
        let out = self
            .rpc
            .eth_call(
                &CallRequest {
                    to: token,
                    from: None,
                    data: data.into(),
                },
                None,
            )
            .await?;
        let decoded = Erc20::allowanceCall::abi_decode_returns(&out, true)
            .map_err(|e| RmpcError::ErrRpcDecode(format!("allowance decode: {e}")))?;
        Ok(decoded._0)
    }

    async fn call_view_balance_of(&self, token: Address, who: Address) -> Result<U256> {
        let data = Erc20::balanceOfCall { account: who }.abi_encode();
        let out = self
            .rpc
            .eth_call(
                &CallRequest {
                    to: token,
                    from: None,
                    data: data.into(),
                },
                None,
            )
            .await?;
        let decoded = Erc20::balanceOfCall::abi_decode_returns(&out, true)
            .map_err(|e| RmpcError::ErrRpcDecode(format!("balanceOf decode: {e}")))?;
        Ok(decoded._0)
    }
}

/// Preflight snapshot, in the same order as [`PreflightReport`]. Numeric
/// values that may exceed `u64` are serialised as decimal strings so the
/// JSON survives `JSON.parse` in JavaScript callers without precision loss.
#[derive(Debug, Serialize)]
pub struct ChecksOutput {
    pub chain_id_match: bool,
    pub gateway_code_hash_match: bool,
    pub gateway_paused: bool,
    pub agent_active: bool,
    pub agent_valid_until: u64,
    pub max_per_payment: String,
    pub max_per_window: String,
    pub window_gross: String,
    pub allowance: String,
    pub balance: String,
}

impl ChecksOutput {
    pub(crate) fn from_report(r: &PreflightReport) -> Self {
        Self {
            chain_id_match: true,
            gateway_code_hash_match: r.gateway_runtime_hash_ok,
            gateway_paused: r.paused,
            agent_active: r.agent_active,
            agent_valid_until: r.agent_valid_until,
            max_per_payment: r.max_per_payment.to_string(),
            max_per_window: r.max_per_window.to_string(),
            window_gross: r.window_gross.to_string(),
            allowance: r.allowance.to_string(),
            balance: r.balance.to_string(),
        }
    }

    /// Best-effort partial snapshot when only the [`RmpcError`] is
    /// available. Mirrors the per-error logic that `self-check`'s `run`
    /// uses for the same purpose.
    pub(crate) fn from_err_partial(err: &RmpcError) -> Self {
        let mut c = Self::unknown();
        match err {
            RmpcError::ErrChainIdMismatch => {}
            RmpcError::ErrCodeHashMismatch => {
                c.chain_id_match = true;
            }
            RmpcError::ErrGatewayPaused => {
                c.chain_id_match = true;
                c.gateway_code_hash_match = true;
                c.gateway_paused = true;
            }
            _ => {
                c.chain_id_match = true;
                c.gateway_code_hash_match = true;
            }
        }
        c
    }

    pub(crate) fn unknown() -> Self {
        Self {
            chain_id_match: false,
            gateway_code_hash_match: false,
            gateway_paused: false,
            agent_active: false,
            agent_valid_until: 0,
            max_per_payment: "0".into(),
            max_per_window: "0".into(),
            window_gross: "0".into(),
            allowance: "0".into(),
            balance: "0".into(),
        }
    }
}

// --- helpers ------------------------------------------------------------

fn parse_addr(s: &str, field: &str) -> Result<Address> {
    Address::from_str(s).map_err(|e| RmpcError::ErrConfig(format!("{field}: {e}")))
}

fn parse_b256_hex(s: &str) -> Result<[u8; 32]> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped)
        .map_err(|e| RmpcError::ErrConfig(format!("gateway_runtime_hash: {e}")))?;
    if bytes.len() != 32 {
        return Err(RmpcError::ErrConfig(format!(
            "gateway_runtime_hash: expected 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
