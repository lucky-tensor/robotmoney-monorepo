//! Canonical: Plan tracking issue #109 — "Router-weight governance" phase
//! Implements: issue #309 (rmpc get-governance subprocess assertion)
//!
//! `rmpc get-governance` — direct on-chain read of RouterGovernance state.
//!
//! Sub-reads (all `eth_call`, pinned to a single `eth_blockNumber` snapshot):
//!
//! - `RouterGovernance.currentProposalId()` → current proposal id (0 = none).
//! - `RouterGovernance.cadenceParams()` → voting period, delay, quorum, total power.
//! - `RouterGovernance.activeProposal()` → proposal details (skipped when id = 0).
//! - `RouterGovernance.currentWeights()` → current router weights as seen by governance.
//!
//! Config field `governance_address` must be set; exits `EXIT_STARTUP_FAIL`
//! when absent.
//!
//! Exit codes:
//! - 0 — envelope emitted.
//! - 3 — config / RPC connectivity / address-parse failure.

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::gateway::RouterGovernance;
use crate::network_env::NetworkEnv;
use crate::output::emit;
use crate::read_output::{DecimalU256, Envelope, PartialBuilder};
use crate::rpc::{CallRequest, FailoverRpcClient};

const EXIT_OK: i32 = 0;
const EXIT_STARTUP_FAIL: i32 = 3;

/// One entry in the weight vector as read from governance.
#[derive(Debug, Default, Serialize)]
pub struct WeightEntry {
    pub vault: String,
    pub bps: u64,
}

/// Optional proposal summary.
#[derive(Debug, Serialize)]
pub struct ProposalSummary {
    /// Proposal id (decimal string).
    pub proposal_id: String,
    pub proposer: String,
    pub proposed_vaults: Vec<String>,
    pub proposed_bps: Vec<u64>,
    /// Block timestamp when voting ends.
    pub voting_deadline: u64,
    pub executable_after: u64,
    /// Total voting power cast FOR (decimal string).
    pub votes_for: DecimalU256,
    /// Total voting power cast AGAINST. Always "0" — this contract only
    /// supports FOR votes; included for API completeness.
    pub votes_against: DecimalU256,
    /// Total voting power abstained. Always "0" — this contract only
    /// supports FOR votes; included for API completeness.
    pub votes_abstain: DecimalU256,
    pub executed: bool,
    pub cancelled: bool,
}

/// `data` payload for `rmpc get-governance`.
#[derive(Debug, Default, Serialize)]
pub struct GovernanceData {
    /// RouterGovernance address (from operator config).
    pub address: String,
    /// Current proposal id. `"0"` means no active proposal.
    pub current_proposal_id: DecimalU256,
    /// Voting period in seconds.
    pub voting_period_secs: u64,
    /// Execution delay in seconds.
    pub execution_delay_secs: u64,
    /// Quorum threshold (decimal string).
    pub quorum_threshold: DecimalU256,
    /// Total voting power outstanding (decimal string).
    pub total_voting_power: DecimalU256,
    /// Current router weights as seen by governance. Empty when no weights set.
    pub current_weights: Vec<WeightEntry>,
    /// Active proposal, if any. `null` when `current_proposal_id == 0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_proposal: Option<ProposalSummary>,
}

/// Entry point invoked from `main.rs`. Returns the process exit code.
pub fn run(config_path: &Path, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-governance: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let governance_addr = match cfg.governance_address.as_deref() {
        Some(s) => match Address::from_str(s) {
            Ok(a) => a,
            Err(e) => {
                log::error!("rmpc get-governance: governance_address parse error: {e}");
                return EXIT_STARTUP_FAIL;
            }
        },
        None => {
            log::error!(
                "rmpc get-governance: governance_address not set in config; \
                 add `governance_address = \"0x...\"` to the operator TOML"
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
            log::error!("rmpc get-governance: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-governance: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc get-governance: network environment: {} (chain_id={})",
        network_env.human_label(),
        cfg.chain_id
    );

    let env = match rt.block_on(read_governance(&rpc, governance_addr)) {
        Ok(e) => e,
        Err(e) => {
            log::error!("rmpc get-governance: pre-read setup failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    emit(&env, pretty);
    EXIT_OK
}

async fn read_governance(
    rpc: &FailoverRpcClient,
    governance: Address,
) -> crate::errors::Result<Envelope<GovernanceData>> {
    let chain_id = rpc.chain_id().await?;
    let block_number = rpc.block_number().await?;
    let block_tag = format!("0x{block_number:x}");

    let data = GovernanceData {
        address: format!("{governance:#x}"),
        ..Default::default()
    };
    let mut b = PartialBuilder::new(chain_id, block_number, data);

    // currentProposalId().
    let proposal_id = match call_current_proposal_id(rpc, governance, &block_tag).await {
        Ok(id) => {
            b.data_mut().current_proposal_id = DecimalU256(id);
            id
        }
        Err(e) => {
            b.record_err("current_proposal_id".to_string(), e.to_string());
            U256::ZERO
        }
    };

    // cadenceParams().
    match call_cadence_params(rpc, governance, &block_tag).await {
        Ok((vp, ed, qt, tvp)) => {
            b.data_mut().voting_period_secs = vp;
            b.data_mut().execution_delay_secs = ed;
            b.data_mut().quorum_threshold = DecimalU256(qt);
            b.data_mut().total_voting_power = DecimalU256(tvp);
        }
        Err(e) => {
            b.record_err("cadence_params".to_string(), e.to_string());
        }
    }

    // currentWeights().
    match call_current_weights(rpc, governance, &block_tag).await {
        Ok((vaults, bps)) => {
            let entries: Vec<WeightEntry> = vaults
                .into_iter()
                .zip(bps)
                .map(|(v, bps_val)| WeightEntry {
                    vault: format!("{v:#x}"),
                    bps: bps_val.saturating_to::<u64>(),
                })
                .collect();
            b.data_mut().current_weights = entries;
        }
        Err(e) => {
            b.record_err("current_weights".to_string(), e.to_string());
        }
    }

    // activeProposal() — only read when there's an active proposal.
    if proposal_id > U256::ZERO {
        match call_active_proposal(rpc, governance, &block_tag).await {
            Ok(p) => {
                b.data_mut().active_proposal = Some(p);
            }
            Err(e) => {
                b.record_err("active_proposal".to_string(), e.to_string());
            }
        }
    }

    Ok(b.finish())
}

async fn call_current_proposal_id(
    rpc: &FailoverRpcClient,
    governance: Address,
    block_tag: &str,
) -> crate::errors::Result<U256> {
    let data = RouterGovernance::currentProposalIdCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: governance,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await?;
    let r =
        RouterGovernance::currentProposalIdCall::abi_decode_returns(&out, true).map_err(|e| {
            crate::errors::RmpcError::ErrRpcDecode(format!("currentProposalId abi decode: {e}"))
        })?;
    Ok(r._0)
}

async fn call_cadence_params(
    rpc: &FailoverRpcClient,
    governance: Address,
    block_tag: &str,
) -> crate::errors::Result<(u64, u64, U256, U256)> {
    let data = RouterGovernance::cadenceParamsCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: governance,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await?;
    let r = RouterGovernance::cadenceParamsCall::abi_decode_returns(&out, true).map_err(|e| {
        crate::errors::RmpcError::ErrRpcDecode(format!("cadenceParams abi decode: {e}"))
    })?;
    // Underscore-prefixed because that is how `RouterGovernance.sol` names
    // these return parameters, and the binding is now generated straight from
    // the Foundry artifact rather than hand-trimmed (issue #1362).
    Ok((
        r._votingPeriod,
        r._executionDelay,
        r._quorumThreshold,
        r._totalVotingPower,
    ))
}

async fn call_current_weights(
    rpc: &FailoverRpcClient,
    governance: Address,
    block_tag: &str,
) -> crate::errors::Result<(Vec<Address>, Vec<U256>)> {
    let data = RouterGovernance::currentWeightsCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: governance,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await?;
    let r = RouterGovernance::currentWeightsCall::abi_decode_returns(&out, true).map_err(|e| {
        crate::errors::RmpcError::ErrRpcDecode(format!("currentWeights abi decode: {e}"))
    })?;
    Ok((r.vaults, r.bps))
}

async fn call_active_proposal(
    rpc: &FailoverRpcClient,
    governance: Address,
    block_tag: &str,
) -> std::result::Result<ProposalSummary, String> {
    let data = RouterGovernance::activeProposalCall {}.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to: governance,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await
        .map_err(|e| format!("eth_call failed: {e}"))?;
    let r = RouterGovernance::activeProposalCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;

    Ok(ProposalSummary {
        proposal_id: r.id.to_string(),
        proposer: format!("{:#x}", r.proposer),
        proposed_vaults: r.vaults.iter().map(|v| format!("{v:#x}")).collect(),
        proposed_bps: r.bps.iter().map(|b| b.saturating_to::<u64>()).collect(),
        voting_deadline: r.votingDeadline,
        executable_after: r.executableAfter,
        votes_for: DecimalU256(r.votesFor),
        votes_against: DecimalU256(U256::ZERO),
        votes_abstain: DecimalU256(U256::ZERO),
        executed: r.executed,
        cancelled: r.cancelled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_fails_fast_without_governance_address() {
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
        let code = run(&cfg_path, false);
        assert_eq!(code, EXIT_STARTUP_FAIL);
    }

    #[test]
    fn governance_data_serialises_without_proposal() {
        let data = GovernanceData {
            address: "0x0000000000000000000000000000000000000001".to_string(),
            current_proposal_id: DecimalU256(U256::ZERO),
            current_weights: vec![],
            active_proposal: None,
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::to_value(&data).unwrap();
        assert!(v["active_proposal"].is_null() || v.get("active_proposal").is_none());
        assert_eq!(v["current_proposal_id"].as_str().unwrap(), "0");
    }
}
