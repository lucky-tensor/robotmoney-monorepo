//! Canonical: Plan tracking issue #109 §9 — Phase 3 Direct Chain-Read Query Tooling
//! ADR: docs/technical/rmpc-read-output-contract.md
//!
//! `rmpc get-gateway` — direct on-chain read of the configured gateway's
//! observable state.
//!
//! Sub-reads (all `eth_call` against the gateway, pinned to a single
//! `eth_blockNumber` snapshot per ADR §3.4 block-drift discussion):
//!
//! - `eth_getCode(gateway)` → keccak256 of runtime bytecode (the §9 "code
//!   hash" field). The configured `gateway_runtime_hash` from the operator
//!   TOML is also reported so callers can diff observed-vs-pinned without
//!   a second tool.
//! - `usdc()` and `vault()` → addresses the gateway will route deposits to.
//! - `paused()` → operator-pause flag.
//! - `eth_chainId` → the §9 `chain_id` envelope field.
//!
//! Output is the §9 envelope from `crate::read_output`. Per-field failures
//! are surfaced via `PartialBuilder`: a single sub-read that reverts marks
//! the envelope `partial: true` with a dotted-path error, and the rest of
//! the sub-reads still run so an operator sees as much of the picture as
//! the chain will give them.
//!
//! Exit codes:
//! - 0 — envelope emitted (including `partial: true` envelopes; per ADR
//!   §3.2 a partial response is still a valid read result).
//! - 3 — config / RPC connectivity / address-parse failure (we never even
//!   reached the read stage).

use std::path::Path;
use std::str::FromStr;

use alloy_primitives::{keccak256, Address};
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

/// `data` payload for `rmpc get-gateway`. Field order is the operator-
/// visible JSON order; downstream snapshot tests assert on it. All
/// addresses are lowercase 0x-hex; the runtime hash is 0x-hex of
/// keccak256(eth_getCode(gateway)).
#[derive(Debug, Default, Serialize)]
pub struct GatewayData {
    /// Gateway contract address from operator config.
    pub address: String,
    /// `keccak256(eth_getCode(gateway))` at the pinned block. Empty
    /// string when `eth_getCode` returned `0x` (contract self-destructed
    /// or wrong address).
    pub code_hash: String,
    /// `gateway_runtime_hash` from operator TOML — reported so callers
    /// can compare against `code_hash` without a second tool.
    pub configured_code_hash: String,
    /// `gateway.usdc()` lowercase 0x-hex.
    pub usdc: String,
    /// `gateway.vault()` lowercase 0x-hex.
    pub vault: String,
    /// `gateway.paused()` boolean.
    pub paused: bool,
}

/// Entry point invoked from `main.rs`. Returns the desired process exit code.
pub fn run(config_path: &Path, pretty: bool) -> i32 {
    let cfg = match Config::from_path(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-gateway: failed to load config: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let gateway_addr = match Address::from_str(&cfg.gateway_address) {
        Ok(a) => a,
        Err(e) => {
            log::error!("rmpc get-gateway: gateway_address parse error: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("rmpc get-gateway: tokio runtime build failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };
    let rpc = match cfg.rpc_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("rmpc get-gateway: rpc client init failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    let network_env = NetworkEnv::from_chain_id(cfg.chain_id);
    log::info!(
        "rmpc get-gateway: network environment: {} (chain_id={})",
        network_env.human_label(),
        cfg.chain_id
    );
    let env = match rt.block_on(read_gateway(&rpc, &cfg, gateway_addr)) {
        Ok(env) => env,
        Err(e) => {
            log::error!("rmpc get-gateway: pre-read setup failed: {e}");
            return EXIT_STARTUP_FAIL;
        }
    };

    emit(&env, pretty);
    EXIT_OK
}

/// Drive the four gateway sub-reads against a pinned block. Pre-read
/// setup failures (chain id, block number) abort with `Err`; once those
/// succeed, every individual sub-read failure is captured on the
/// envelope via `record_err`.
async fn read_gateway(
    rpc: &FailoverRpcClient,
    cfg: &Config,
    gateway_addr: Address,
) -> crate::errors::Result<Envelope<GatewayData>> {
    // Pin chain id + block number once. Per ADR §3.4 multi-read commands
    // SHOULD pin to a single block tag and tolerate no drift.
    let chain_id = rpc.chain_id().await?;
    let block_number = rpc.block_number().await?;
    let block_tag = format!("0x{block_number:x}");

    let data = GatewayData {
        address: format!("{gateway_addr:#x}"),
        configured_code_hash: cfg.gateway_runtime_hash.clone(),
        ..Default::default()
    };
    let mut b = PartialBuilder::new(chain_id, block_number, data);

    // code_hash
    match rpc.get_code(gateway_addr, Some(&block_tag)).await {
        Ok(code) if code.is_empty() => {
            b.record_err(
                "code_hash",
                "eth_getCode returned 0x (no contract at address)",
            );
        }
        Ok(code) => {
            b.data_mut().code_hash = format!("0x{}", hex::encode(keccak256(code.as_ref())));
        }
        Err(e) => b.record_err("code_hash", format!("eth_getCode failed: {e}")),
    }

    // usdc()
    match call_view_address(
        rpc,
        gateway_addr,
        &block_tag,
        RobotMoneyGateway::usdcCall {},
    )
    .await
    {
        Ok(addr) => b.data_mut().usdc = format!("{addr:#x}"),
        Err(e) => b.record_err("usdc", e),
    }

    // vault()
    match call_view_address(
        rpc,
        gateway_addr,
        &block_tag,
        RobotMoneyGateway::vaultCall {},
    )
    .await
    {
        Ok(addr) => b.data_mut().vault = format!("{addr:#x}"),
        Err(e) => b.record_err("vault", e),
    }

    // paused()
    match call_view_paused(rpc, gateway_addr, &block_tag).await {
        Ok(p) => b.data_mut().paused = p,
        Err(e) => b.record_err("paused", e),
    }

    Ok(b.finish())
}

/// Decode a `()` → `address` view via the typed bindings. Returns a
/// human-readable message on failure (transport, server, decode) so the
/// builder can surface it as a `FieldError`.
async fn call_view_address<C>(
    rpc: &FailoverRpcClient,
    to: Address,
    block_tag: &str,
    call: C,
) -> std::result::Result<Address, String>
where
    C: SolCall,
    C::Return: ReturnAddress,
{
    let data = call.abi_encode();
    let out = rpc
        .eth_call(
            &CallRequest {
                to,
                from: None,
                data: data.into(),
            },
            Some(block_tag),
        )
        .await
        .map_err(|e| format!("eth_call failed: {e}"))?;
    let decoded = C::abi_decode_returns(&out, true).map_err(|e| format!("abi decode: {e}"))?;
    Ok(decoded.address())
}

/// Trait so we can extract the address out of the alloy-generated
/// `agentsReturn`-style structs without enumerating each by hand.
trait ReturnAddress {
    fn address(self) -> Address;
}
impl ReturnAddress for RobotMoneyGateway::usdcReturn {
    fn address(self) -> Address {
        self._0
    }
}
impl ReturnAddress for RobotMoneyGateway::vaultReturn {
    fn address(self) -> Address {
        self._0
    }
}

async fn call_view_paused(
    rpc: &FailoverRpcClient,
    gateway: Address,
    block_tag: &str,
) -> std::result::Result<bool, String> {
    let data = RobotMoneyGateway::pausedCall {}.abi_encode();
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
    let decoded = RobotMoneyGateway::pausedCall::abi_decode_returns(&out, true)
        .map_err(|e| format!("abi decode: {e}"))?;
    Ok(decoded._0)
}
