//! Canonical: Plan tracking issue #109 §9 — Phase 3 Direct Chain-Read Query Tooling
//! ADR: docs/technical/rmpc-read-output-contract.md
//!
//! `rmpc get-roles --address 0x…` — direct on-chain read of role
//! membership for a target EOA against the gateway's
//! `AccessControlEnumerable` surface.
//!
//! Sub-reads (all `eth_call`, pinned to a single block):
//!
//! - `DEFAULT_ADMIN_ROLE()`, `ADMIN_ROLE()`, `PAUSER_ROLE()`,
//!   `AGENT_ROLE()` → fetch the four canonical role-hash constants from
//!   the deployed contract, never bake them in. The §9 acceptance test
//!   "role membership for ADMIN, PAUSER, AGENT, and any future roles"
//!   relies on the on-chain truth.
//! - `hasRole(role, address)` once per role.
//!
//! The set of roles is locked to the four declared on `RobotMoneyGateway`.
//! Future roles added to the contract become a separate batch (issue +
//! ADR amendment) — silently widening the role list would risk a stale
//! `false` value being trusted.
//!
//! Output is the §9 envelope; per-field failures are recorded against
//! the role name (`role.<name>.has` or `role.<name>.hash`).
//!
//! Exit codes:
//! - 0 — envelope emitted (including `partial: true`).
//! - 3 — config / RPC / address-parse failure before the read could run.

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::{Address, B256};
use alloy_sol_types::SolCall;
use serde::Serialize;

use crate::config::Config;
use crate::gateway::RobotMoneyGateway;
use crate::network_env::NetworkEnv;
use crate::output::emit;
use crate::read_output::{Envelope, PartialBuilder};
use crate::rpc::{CallRequest, FailoverRpcClient};

const EXIT_OK: i32 = 0;
const EXIT_STARTUP_FAIL: i32 = 3;

/// One row in the per-role result list. The role name is the canonical
/// string the contract exposes ("DEFAULT_ADMIN_ROLE", "ADMIN_ROLE",
/// "PAUSER_ROLE", "AGENT_ROLE"); `hash` is the 0x-hex `bytes32` returned
/// by the on-chain getter; `has_role` is `hasRole(hash, address)`.
#[derive(Debug, Default, Serialize)]
pub struct RoleEntry {
    pub name: String,
    pub hash: String,
    pub has_role: bool,
}

/// `data` payload for `rmpc get-roles`. The JSON shape is
/// `{ address, roles: [RoleEntry, …] }` — the envelope adds `chain_id`,
/// `block_number`, `source`, `partial`, `errors`.
#[derive(Debug, Default, Serialize)]
pub struct RolesData {
    pub address: String,
    pub roles: Vec<RoleEntry>,
}

/// Names match the on-chain getters; the order is the order the entries
/// appear in `roles[]`. Snapshot tests assert on it.
const ROLES: &[&str] = &[
    "DEFAULT_ADMIN_ROLE",
    "ADMIN_ROLE",
    "PAUSER_ROLE",
    "AGENT_ROLE",
];

pub fn run(config_path: &Path, address_hex: &str, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-roles: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let target = match Address::from_str(address_hex) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc get-roles: --address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let gateway_addr = match Address::from_str(&cfg.gateway_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc get-roles: gateway_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc get-roles: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-roles: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc get-roles: network environment: {} (chain_id={})",
        network_env.human_label(),
        cfg.chain_id
    );
    let env = match rt.block_on(read_roles(&rpc, gateway_addr, target)) {
        Ok(e) => e,
        Err(e) => {
            log::error!("rmpc get-roles: pre-read setup failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    emit(&env, pretty);
    EXIT_OK
}

async fn read_roles(
    rpc: &FailoverRpcClient,
    gateway: Address,
    target: Address,
) -> crate::errors::Result<Envelope<RolesData>> {
    let chain_id = rpc.chain_id().await?;
    let block_number = rpc.block_number().await?;
    let block_tag = format!("0x{block_number:x}");

    let data = RolesData {
        address: format!("{target:#x}"),
        roles: ROLES
            .iter()
            .map(|n| RoleEntry {
                name: (*n).to_string(),
                hash: String::new(),
                has_role: false,
            })
            .collect(),
    };
    let mut b = PartialBuilder::new(chain_id, block_number, data);

    for (idx, name) in ROLES.iter().enumerate() {
        match read_role_hash(rpc, gateway, &block_tag, name).await {
            Ok(hash) => {
                b.data_mut().roles[idx].hash = format!("{hash:#x}");
                match call_has_role(rpc, gateway, &block_tag, hash, target).await {
                    Ok(v) => b.data_mut().roles[idx].has_role = v,
                    Err(e) => b.record_err(format!("role.{name}.has"), e),
                }
            }
            Err(e) => {
                b.record_err(format!("role.{name}.hash"), e);
            }
        }
    }

    Ok(b.finish())
}

async fn read_role_hash(
    rpc: &FailoverRpcClient,
    gateway: Address,
    block_tag: &str,
    role: &str,
) -> std::result::Result<B256, String> {
    let data = match role {
        "DEFAULT_ADMIN_ROLE" => RobotMoneyGateway::DEFAULT_ADMIN_ROLECall {}.abi_encode(),
        "ADMIN_ROLE" => RobotMoneyGateway::ADMIN_ROLECall {}.abi_encode(),
        "PAUSER_ROLE" => RobotMoneyGateway::PAUSER_ROLECall {}.abi_encode(),
        "AGENT_ROLE" => RobotMoneyGateway::AGENT_ROLECall {}.abi_encode(),
        other => return Err(format!("unknown role getter: {other}")),
    };
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
    // Every role getter returns bytes32. We decode by hand to keep the
    // match-arm above structural — the ABI shape is a single 32-byte word.
    if out.len() != 32 {
        return Err(format!("expected 32 bytes, got {}", out.len()));
    }
    Ok(B256::from_slice(out.as_ref()))
}

async fn call_has_role(
    rpc: &FailoverRpcClient,
    gateway: Address,
    block_tag: &str,
    role: B256,
    account: Address,
) -> std::result::Result<bool, String> {
    let data = RobotMoneyGateway::hasRoleCall { role, account }.abi_encode();
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
    let decoded = RobotMoneyGateway::hasRoleCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(decoded._0)
}
