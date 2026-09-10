//! Canonical: docs/development/smoke-test-design.md
//!
//! Devnet fixture library for Robot Money integration tests.
//!
//! Boot the Geth+Lighthouse devnet and deploy contracts by constructing
//! [`Fixture`]. Drop tears the Docker Compose stack down unconditionally.
//!
//! This crate is chain-level only — no knowledge of any client binary
//! (rmpc, dapp, explorer). Callers that need client helpers import this
//! crate as a dependency and build on top of [`Fixture`].
//!
//! Public surface:
//! - [`Fixture::new`] / [`Fixture::with_deploy_env`] — boot and deploy.
//! - Address accessors: [`Fixture::rpc_url`], [`Fixture::gateway`], etc.
//! - On-chain poke helpers: [`Fixture::pause_gateway`], [`Fixture::fund_usdc`], etc.
//! - [`prerequisites_available`] — check for docker/forge/cast on PATH.
//! - [`fork_manifest::ForkManifest`] — typed view over
//!   `testing/ethereum-testnet/config/fork-block.json` (issue #255).

/// Dev-scout module for Base testnet fixture support (issue #842).
/// Automated account funding seams for Base testnet e2e tests (issue #839).
pub mod base_testnet;
pub mod fork_manifest;
pub mod genesis_alloc;
pub mod logging;
/// Dev-scout map for the real-adapter state injection boundary (issue #739).
pub mod real_adapter_state;

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{keccak256, Address};
use serde::Deserialize;
use tempfile::TempDir;

// -- Genesis account constants ----------------------------------------

/// Genesis-funded deployer. Used as `--from` for `forge script` and is
/// the recorded **agent owner** in the smoke-test fixture (issue #269 —
/// each depositor is the sole authority over her own agent, so the
/// deployer EOA stands in as the depositor for `revoke_agent` and
/// `reauthorize_agent`). Also holds `ADMIN_ROLE`, which is now scoped to
/// protocol-wide kill switches (`unpause`) and no longer gates any
/// agent's lifecycle.
pub const DEPLOYER_PRIVATE_KEY_HEX: &str =
    "0xbcdf20249abf0ed6d944c0288fad489e33f66b3960d9e6229c1cd214ed3bbe31";
pub const DEPLOYER_ADDRESS_HEX: &str = "0x8943545177806ED17B9F23F0a21ee5948eCaa776";

/// Key paired with PAUSER_ROLE. The derived address (`0x6145…`) is
/// granted PAUSER_ROLE at deploy time so [`Fixture::pause_gateway`] can
/// use `cast send` with a real signed transaction.
pub const PAUSER_PRIVATE_KEY_HEX: &str =
    "0x53321db7c1e331d93a11a41d16f004d7ff63972ec8ec7c25db329728ceeb1710";
pub const PAUSER_ADDRESS_HEX: &str = "0x614561D2d143621E126e87831AEF287678B442b8";

/// Genesis-funded EOA registered as the vault share receiver.
pub const SHARE_RECEIVER_ADDRESS_HEX: &str = "0x1CBd3b2770909D4e10f157cABC84C7264073C9Ec";

/// Harness USDC holder — the clean-history EOA that receives a genesis-time
/// USDC balance grant on the smoke-test devnet. See
/// `docs/development/smoke-test-design.md` (USDC faucet section) and issue #255.
///
/// This key MUST NOT be used on any real chain. It is test-only by
/// construction. The genesis ingester writes
/// `usdc.balances[HARNESS_USDC_HOLDER_ADDRESS_HEX] = grant_units` into the
/// devnet's `genesis.json` alloc, and `Fixture::fund_usdc` signs a plain
/// `transfer(address,uint256)` from this key against the canonical Base USDC
/// proxy.
pub const HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX: &str =
    "0xd2dffaf3c3c5e3e2f5cb5cef1a3a2e0e0a8b9d4ae2f6c1d3e8a5b7c9e0f1a2b3";
/// Address derived from [`HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX`]. Verified
/// against `cast wallet address` at definition time. Used by the genesis
/// ingester (for the USDC balance grant + ETH-for-gas alloc) and by
/// `Fixture::fund_usdc` (as the transfer sender).
pub const HARNESS_USDC_HOLDER_ADDRESS_HEX: &str = "0xaE67A1B2A267a124Cf762098E3Cbf6B03329E6d5";

/// Fixed host+container port for the `receipt-fixtures` compose service
/// (issue #1294). Not randomized like [`DappPorts`] — that service's
/// container_name is already fixed (`dapp-receipt-fixtures`), so this
/// compose project is already single-instance-per-host; using a fixed port
/// here matches that existing constraint rather than introducing a new one.
/// See `testing/ethereum-testnet/config/docker-compose.dapp.yaml` and
/// `Fixture::seed_consensus_receipts`.
pub const RECEIPT_FIXTURES_PORT: u16 = 8097;

/// 32-byte secp256k1 private key for the test agent EOA. Test-only —
/// never use on a real chain.
/// Derives `0xf93Ee4Cf8c6c40b329b0c0626F28333c132CF241`.
pub const AGENT_PRIVATE_KEY: [u8; 32] = [
    0xab, 0x63, 0xb2, 0x3e, 0xb7, 0x94, 0x1c, 0x12, 0x51, 0x75, 0x7e, 0x24, 0xb3, 0xd2, 0x35, 0x0d,
    0x2b, 0xc0, 0x5c, 0x3c, 0x38, 0x8d, 0x06, 0xf8, 0xfe, 0x6f, 0xea, 0xfe, 0xfb, 0x1e, 0x8c, 0x70,
];

/// Derive the agent EOA address from [`AGENT_PRIVATE_KEY`].
pub fn agent_address() -> Address {
    derive_address(&AGENT_PRIVATE_KEY)
}

/// Demo router weight (bps) for the primary RobotMoneyVault (PRD §11.1 Stable
/// Yield) after demo seeding. The primary vault carries 8500 bps; the remaining
/// 1500 bps are split equally across rmRWA (§11.4), rmPROTO (§11.2), and rmAGENT
/// (§11.3) at 500 bps each. Issue #621.
pub const DEMO_WEIGHT_PRIMARY_BPS: u64 = 8_500;

// -- Error type -------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("required binary `{0}` not found on PATH")]
    FoundryMissing(&'static str),
    #[error("RPC at {url} did not become healthy within {timeout:?}")]
    RpcTimeout { url: String, timeout: Duration },
    #[error("forge script failed: {0}")]
    DeployFailed(String),
    #[error("deployment JSON {0}: {1}")]
    DeploymentJson(PathBuf, String),
    #[error("docker compose error: {0}")]
    Docker(String),
    #[error("{0}")]
    Other(String),
}

impl HarnessError {
    pub fn other<S: Into<String>>(s: S) -> Self {
        HarnessError::Other(s.into())
    }
}

// -- Deployment JSON --------------------------------------------------

#[derive(Debug, Deserialize)]
struct DeploymentJson {
    chain_id: u64,
    usdc: String,
    vault: String,
    /// Aave V3 strategy adapter address registered with the vault at deploy time.
    /// Absent on legacy deployments (pre-#363).
    #[serde(default)]
    aave_adapter: String,
    /// Compound V3 strategy adapter address registered with the vault at deploy time.
    /// Absent on legacy deployments (pre-#363).
    #[serde(default)]
    compound_adapter: String,
    /// Morpho strategy adapter address registered with the vault at deploy time.
    /// Absent on legacy deployments (pre-#363).
    #[serde(default)]
    morpho_adapter: String,
    gateway: String,
    #[serde(default)]
    #[allow(dead_code)]
    admin: String,
    #[serde(default)]
    #[allow(dead_code)]
    pauser: String,
    agent: String,
    share_receiver: String,
    gateway_runtime_hash: String,
}

/// Typed view over the registry deployment JSON produced by DeployVaultRegistry.s.sol.
#[derive(Debug, Deserialize)]
struct RegistryDeploymentJson {
    registry: String,
    #[serde(default)]
    #[allow(dead_code)]
    chain_id: u64,
    #[serde(default)]
    #[allow(dead_code)]
    vault_registered: bool,
}

/// Typed view over the router deployment JSON produced by DeployPortfolioRouter.s.sol.
#[derive(Debug, Deserialize)]
struct RouterDeploymentJson {
    router: String,
    #[serde(default)]
    #[allow(dead_code)]
    chain_id: u64,
}

/// Typed view over the governance deployment JSON produced by DeployRouterGovernance.s.sol.
#[derive(Debug, Deserialize)]
struct GovernanceDeploymentJson {
    governance: String,
    #[serde(default)]
    #[allow(dead_code)]
    chain_id: u64,
}

/// Typed view over the RM token deployment JSON produced by DeployRmToken.s.sol (issue #365).
#[derive(Debug, Deserialize)]
struct RmTokenDeploymentJson {
    rm_token: String,
    #[serde(default)]
    #[allow(dead_code)]
    chain_id: u64,
}

/// Typed view over the IC policy + consensus receipt deployment JSON
/// produced by DeployInvestmentCommitteePolicy.s.sol. Deploys BOTH
/// `InvestmentCommitteePolicy` and `ConsensusRebalanceReceipt` in one
/// ceremony (issue #1247 AC10) so a single ceremony wires both into the
/// gateway. Issue #1294: consumed by the dapp e2e devnet wiring.
#[derive(Debug, Deserialize)]
struct IcPolicyDeploymentJson {
    policy: String,
    consensus_receipt: String,
    #[serde(default)]
    #[allow(dead_code)]
    chain_id: u64,
}

/// Typed view over the Uniswap V3 stub deployment JSON produced by
/// DeployDemoUniswapV3Stubs.s.sol (issue #531). Four `UniswapV3PoolSlot0Stub`
/// contracts deployed at deterministic CREATE2 addresses (Arachnid factory,
/// fixed salts). Addresses are pre-committed in `config/dex-pools.json::devnet.pools`
/// so the dapp Docker image is built with the correct pool addresses.
#[derive(Debug, Deserialize)]
struct DemoUniswapV3StubsDeploymentJson {
    eth_usd: String,
    weth_usdc: String,
    cbbtc_usdc: String,
    wsol_usdc: String,
    #[serde(default)]
    #[allow(dead_code)]
    chain_id: u64,
}

/// Typed view over the demo-extra-vaults deployment JSON produced by
/// DeployDemoExtraVaults.s.sol. Surfaces the PRD §11 vault catalog seeded into
/// the demo so smoke-test assertions and downstream tooling can resolve every
/// Active vault and the RWA placeholder. The demo seed deploys the canonical
/// `ProtocolAssetVault` (§11.2) and `AgentTokenVault` (§11.3) with devnet
/// basket stubs, plus an RWA placeholder (§11.4) registered Paused. All three
/// extra entries plus the primary `RobotMoneyVault` (§11.1) make up the
/// four-vault catalog. Router-eligibility is restricted to the primary vault
/// per PRD §11.2/§11.3 — basket vaults stay gap-blocked.
#[derive(Debug, Deserialize)]
struct DemoExtraVaultsDeploymentJson {
    protocol_vault: String,
    #[serde(default)]
    #[allow(dead_code)]
    protocol_tokens: Vec<String>,
    agent_token_vault: String,
    #[serde(default)]
    #[allow(dead_code)]
    agent_tokens: Vec<String>,
    /// RWA/Thematic placeholder (PRD §11.4). Registered non-Active (Paused)
    /// and never router-eligible — rounds the deployed set to the four PRD §11
    /// categories without entering the router weight vector.
    rwa_vault: String,
    #[serde(default)]
    #[allow(dead_code)]
    chain_id: u64,
}

#[derive(Debug, Deserialize)]
struct ComposePsEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "State")]
    state: String,
}

// -- Fixture ----------------------------------------------------------

/// A fully-wired devnet fixture. Boot by calling [`Fixture::new`];
/// Drop tears down the Docker Compose stack.
pub struct Fixture {
    compose_dir: PathBuf,
    /// Tempdir for harness artifacts (deployment JSON, etc.).
    /// Exposed via [`Fixture::tempdir`] so callers can write
    /// additional files (keystores, configs) into the same directory.
    tmp: TempDir,
    compose_log_followers: Vec<MonitoredChild>,
    chain_ports: ChainPorts,
    rpc_port: u16,
    rpc_url: String,
    chain_id: u64,
    deployment: DeploymentJson,
    registry_deployment: RegistryDeploymentJson,
    router_deployment: RouterDeploymentJson,
    governance_deployment: GovernanceDeploymentJson,
    rm_token_deployment: RmTokenDeploymentJson,
    /// InvestmentCommitteePolicy + ConsensusRebalanceReceipt (issue #1247
    /// AC10, issue #1294). One ceremony deploys both.
    ic_policy_deployment: IcPolicyDeploymentJson,
    /// Demo-only extra vaults registered alongside the primary RobotMoneyVault
    /// (issue #465). Two passthrough-backed `RobotMoneyVault` stand-ins that
    /// let the smoke-test exercise multi-vault router weights without
    /// depending on the still-ADR-blocked basket vaults.
    demo_extra_vaults: DemoExtraVaultsDeploymentJson,
    /// Demo-only Uniswap V3 stub pool contracts deployed on the devnet so the
    /// landing-page price strip can read slot0 (issue #531). Addresses are
    /// deterministic (CREATE2 via Arachnid factory) and pre-committed in
    /// `config/dex-pools.json::devnet.pools`.
    demo_uniswap_v3_stubs: DemoUniswapV3StubsDeploymentJson,
    repo_root: PathBuf,
    /// Harness-owned nonce source of truth for every EOA this devnet sends
    /// from (issue #1241, extended to the funding path by issue #1374).
    /// Created before the first boot-time funding send and moved in here, so
    /// boot funding, deploy funding and every later [`Fixture::cast_send`]
    /// share one nonce sequence per sender. See [`NonceTracker`].
    nonce_tracker: NonceTracker,
}

#[derive(Debug, Clone, Copy)]
struct ChainPorts {
    rpc_port: u16,
    ws_port: u16,
    authrpc_port: u16,
    beacon_port: u16,
}

#[derive(Debug, Clone, Copy)]
struct DappPorts {
    postgres_port: u16,
    explorer_api_port: u16,
    dapp_port: u16,
}

struct ComposeContainerStatus {
    id: String,
    name: String,
    service: Option<String>,
    state: String,
    health: Option<String>,
    exit_code: Option<i64>,
    oom_killed: bool,
    error: Option<String>,
}

/// Compose services that are one-shot BY DESIGN: they run to completion and
/// exit 0, so the health probe must not read their `exited` state as a stack
/// failure.
///
/// - `setup` — the chain stack's genesis/keystore bootstrap
///   (`testing/ethereum-testnet/config/docker-compose.yaml`).
/// - `explorer-migrate` — the dapp stack's schema migration step
///   (`docker-compose.dapp.yaml`, issue #1359). The explorer schema used to be
///   migrated as a side effect of the indexer's boot; it is now its own
///   container that `explorer-indexer` and `explorer-api` wait on with
///   `service_completed_successfully`.
///
/// Only a ZERO exit is exempted (see [`is_completed_one_shot`]). A failing
/// migration still trips the probe — that is the point of making migration an
/// explicit step.
const ONE_SHOT_COMPOSE_SERVICES: [&str; 2] = ["setup", "explorer-migrate"];

/// True when this container is a [`ONE_SHOT_COMPOSE_SERVICES`] member that has
/// finished successfully, and so must be excluded from the unhealthy set.
fn is_completed_one_shot(status: &ComposeContainerStatus) -> bool {
    status.exit_code == Some(0)
        && status
            .service
            .as_deref()
            .is_some_and(|service| ONE_SHOT_COMPOSE_SERVICES.contains(&service))
}

impl ComposeContainerStatus {
    fn describe(&self) -> String {
        let service = self.service.as_deref().unwrap_or("unknown");
        let health = self.health.as_deref().unwrap_or("n/a");
        let exit_code = self
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "n/a".to_string());
        let error = self.error.as_deref().unwrap_or("");
        format!(
            "id={} container={} service={} state={} health={} exit_code={} oom_killed={} error={}",
            self.id, self.name, service, self.state, health, exit_code, self.oom_killed, error
        )
    }

    fn is_unhealthy(&self) -> bool {
        self.oom_killed
            || matches!(
                self.state.as_str(),
                "exited" | "dead" | "removing" | "restarting"
            )
            || self.exit_code.is_some_and(|code| code != 0)
            || self.error.as_deref().is_some_and(|error| !error.is_empty())
    }
}

struct MonitoredChild {
    label: String,
    child: Arc<Mutex<Child>>,
    terminated: Arc<AtomicBool>,
}

impl MonitoredChild {
    fn new(label: impl Into<String>, child: Child) -> Self {
        let label = label.into();
        let child = Arc::new(Mutex::new(child));
        let terminated = Arc::new(AtomicBool::new(false));
        let watcher_child = Arc::clone(&child);
        let watcher_terminated = Arc::clone(&terminated);
        let watcher_label = label.clone();
        thread::spawn(move || loop {
            if watcher_terminated.load(Ordering::SeqCst) {
                return;
            }
            let status = match watcher_child.lock() {
                Ok(mut child) => match child.try_wait() {
                    Ok(Some(status)) => Some(Ok(status)),
                    Ok(None) => None,
                    Err(err) => Some(Err(err)),
                },
                Err(_) => return,
            };
            match status {
                Some(Ok(status)) => {
                    if !watcher_terminated.load(Ordering::SeqCst) {
                        if status.success() {
                            logging::info(
                                &watcher_label,
                                format!("process exited cleanly: {status}"),
                            );
                        } else {
                            logging::error(
                                &watcher_label,
                                format!("process exited unexpectedly: {status}"),
                            );
                        }
                    }
                    return;
                }
                Some(Err(err)) => {
                    logging::error(
                        &watcher_label,
                        format!("process status check failed: {err}"),
                    );
                    return;
                }
                None => thread::sleep(Duration::from_secs(5)),
            }
        });
        Self {
            label,
            child,
            terminated,
        }
    }

    fn terminate(&self) {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        } else {
            logging::warn(&self.label, "child lock poisoned during shutdown");
        }
    }
}

impl Drop for MonitoredChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn allocate_unique_port(used: &mut HashSet<u16>) -> Result<u16, HarnessError> {
    loop {
        let port = test_utils::pick_free_port()?;
        if used.insert(port) {
            return Ok(port);
        }
    }
}

fn reserve_port(
    used: &mut HashSet<u16>,
    port: u16,
    label: &'static str,
) -> Result<u16, HarnessError> {
    if used.insert(port) {
        Ok(port)
    } else {
        Err(HarnessError::other(format!(
            "{label} port {port} collides with another allocated port"
        )))
    }
}

fn localhost_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn browser_url(port: u16) -> String {
    format!("http://localhost:{port}")
}

impl ChainPorts {
    /// Allocate chain ports. The Geth RPC port may be pinned via the
    /// `SMOKE_TEST_GETH_RPC_PORT` env var so an external reverse proxy
    /// (e.g. a named cloudflared tunnel with a stable hostname) can
    /// target a deterministic local port. WS / authrpc / beacon stay
    /// randomized since nothing outside the host attaches to them.
    fn allocate() -> Result<Self, HarnessError> {
        let mut used = HashSet::new();
        let rpc_port = match std::env::var("SMOKE_TEST_GETH_RPC_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
        {
            Some(p) => reserve_port(&mut used, p, "geth_rpc")?,
            None => allocate_unique_port(&mut used)?,
        };
        Ok(Self {
            rpc_port,
            ws_port: allocate_unique_port(&mut used)?,
            authrpc_port: allocate_unique_port(&mut used)?,
            beacon_port: allocate_unique_port(&mut used)?,
        })
    }

    fn rpc_url(&self) -> String {
        localhost_url(self.rpc_port)
    }
}

impl DappPorts {
    fn allocate(
        dapp_port: Option<u16>,
        explorer_api_port: Option<u16>,
        occupied_ports: &[u16],
    ) -> Result<Self, HarnessError> {
        let mut used = occupied_ports.iter().copied().collect::<HashSet<_>>();
        let dapp_port = match dapp_port {
            Some(port) => reserve_port(&mut used, port, "dapp")?,
            None => allocate_unique_port(&mut used)?,
        };
        let postgres_port = allocate_unique_port(&mut used)?;
        let explorer_api_port = match explorer_api_port {
            Some(port) => reserve_port(&mut used, port, "explorer_api")?,
            None => allocate_unique_port(&mut used)?,
        };

        Ok(Self {
            postgres_port,
            explorer_api_port,
            dapp_port,
        })
    }

    fn dapp_url(&self) -> String {
        browser_url(self.dapp_port)
    }

    fn explorer_api_url(&self) -> String {
        browser_url(self.explorer_api_port)
    }
}

impl Fixture {
    /// Boot the Docker Geth+Lighthouse devnet, run the gateway deploy
    /// script, and fund the test EOAs.
    ///
    /// The Geth devnet boots from a genesis snapshot that carries real Aave V3,
    /// Compound V3, and Morpho storage (produced by `scripts/devnet/snapshot-fork.sh`
    /// with the adapter warming step from issue #685).  Deploy.s.sol deploys the
    /// three real protocol adapters by default.
    pub fn new() -> Result<Self, HarnessError> {
        Self::with_deploy_env(&[])
    }

    /// Like [`Self::new`] but passes extra env vars to `forge script Deploy`.
    /// Used to override deploy-time parameters (e.g. `AGENT_MAX_PER_WINDOW`).
    pub fn with_deploy_env(extra_deploy_env: &[(&str, &str)]) -> Result<Self, HarnessError> {
        if which::which("docker").is_err() {
            return Err(HarnessError::FoundryMissing("docker"));
        }
        if which::which("forge").is_err() {
            return Err(HarnessError::FoundryMissing("forge"));
        }
        if which::which("cast").is_err() {
            return Err(HarnessError::FoundryMissing("cast"));
        }

        let repo_root = locate_repo_root()?;
        let tmp = TempDir::new()?;
        let compose_dir = repo_root.join("testing/ethereum-testnet/config");
        let chain_ports = ChainPorts::allocate()?;
        let rpc_url = chain_ports.rpc_url();
        // One nonce source of truth for this devnet's whole lifetime (issue
        // #1374). Created here, before the first funding send, and moved into
        // the `Fixture` below — so the boot-time deployer/holder funding sends
        // and every later `Fixture::cast_send` from those same keys draw from
        // a single monotonic sequence instead of two independent ones.
        let nonce_tracker = NonceTracker::new(rpc_url.clone());

        // Issue #255: render the genesis alloc overlay before booting compose
        // so the `setup` container can bind-mount + merge it into the EL
        // genesis.json. If rendering fails (missing fixture, malformed
        // manifest), fall back to the legacy clean-room genesis path —
        // verbose-logging the reason so the operator can fix it offline.
        let alloc_overlay_path = match render_genesis_alloc_overlay(&repo_root, tmp.path()) {
            Ok(Some(p)) => Some(p),
            Ok(None) => {
                eprintln!(
                    "smoke-test: skipping genesis alloc overlay (fixture or manifest absent); \
                     booting with clean-room genesis (legacy behaviour)"
                );
                None
            }
            Err(e) => {
                eprintln!(
                    "smoke-test: genesis alloc overlay rendering failed: {e}; \
                     falling back to clean-room genesis"
                );
                None
            }
        };

        let compose_files: Vec<&str> = if alloc_overlay_path.is_some() {
            vec![
                "-f",
                "docker-compose.yaml",
                "-f",
                "docker-compose.alloc.yaml",
            ]
        } else {
            vec!["-f", "docker-compose.yaml"]
        };
        let compose_files_owned: Vec<String> =
            compose_files.iter().map(|s| s.to_string()).collect();
        let mut compose_log_env = vec![
            ("GETH_RPC_PORT", chain_ports.rpc_port.to_string()),
            ("GETH_WS_PORT", chain_ports.ws_port.to_string()),
            ("GETH_AUTHRPC_PORT", chain_ports.authrpc_port.to_string()),
            ("BEACON_PORT", chain_ports.beacon_port.to_string()),
        ];
        if let Some(ref p) = alloc_overlay_path {
            compose_log_env.push(("SMOKE_GENESIS_ALLOC_FILE", p.to_string_lossy().to_string()));
            std::env::set_var("SMOKE_GENESIS_ALLOC_FILE", p);
        }
        let compose_project = "ethereum-testnet";
        let genesis_timestamp =
            std::env::var("GENESIS_TIMESTAMP").unwrap_or_else(|_| "unset".to_string());
        let overlay_mode = if alloc_overlay_path.is_some() {
            "alloc-overlay"
        } else {
            "clean-room"
        };
        logging::info(
            "smoke-test",
            format!(
                "chain startup config: project={compose_project} mode={overlay_mode} genesis_timestamp={genesis_timestamp} rpc_port={} ws_port={} authrpc_port={} beacon_port={} compose_files={}",
                chain_ports.rpc_port,
                chain_ports.ws_port,
                chain_ports.authrpc_port,
                chain_ports.beacon_port,
                compose_files_owned.join(" "),
            ),
        );
        // Stamp this boot with a unique run-id (exported for the compose label
        // interpolation) and reap any containers stranded by a previous run
        // before asserting the project is idle. Replaces the old "error out and
        // make the operator clean up by hand" behaviour on a zombie collision.
        let (run_id, _run_created) = ensure_run_identity();
        logging::info("smoke-test", format!("boot run-id={run_id}"));
        reap_stale_testnet_containers(&run_id);
        ensure_compose_project_idle(&compose_dir, &compose_files_owned)?;
        let cleanup_compose_files = compose_files_owned.clone();
        let cleanup_compose_dir = compose_dir.clone();
        let cleanup_alloc_overlay_path = alloc_overlay_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let cleanup = move || {
            let mut c = Command::new("docker");
            c.arg("compose");
            for f in &cleanup_compose_files {
                c.arg(f);
            }
            c.args(["down", "-v", "--remove-orphans"]);
            if let Some(ref p) = cleanup_alloc_overlay_path {
                c.env("SMOKE_GENESIS_ALLOC_FILE", p);
            }
            c.current_dir(&cleanup_compose_dir);
            let _ = c.status();
        };

        let mut up_cmd = Command::new("docker");
        up_cmd.arg("compose");
        for f in &compose_files_owned {
            up_cmd.arg(f);
        }
        up_cmd
            .arg("up")
            .arg("-d")
            .arg("--build")
            .env("GETH_RPC_PORT", chain_ports.rpc_port.to_string())
            .env("GETH_WS_PORT", chain_ports.ws_port.to_string())
            .env("GETH_AUTHRPC_PORT", chain_ports.authrpc_port.to_string())
            .env("BEACON_PORT", chain_ports.beacon_port.to_string())
            .current_dir(&compose_dir);
        if let Some(ref p) = alloc_overlay_path {
            up_cmd.env("SMOKE_GENESIS_ALLOC_FILE", p);
        }
        logging::info("smoke-test", "bringing up chain compose stack");
        let up_out = up_cmd.output().map_err(HarnessError::from)?;
        logging::log_command_output("compose", &up_out);
        if !up_out.status.success() {
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "compose up failed",
                200,
            );
            cleanup();
            return Err(HarnessError::Docker(format!(
                "compose up devnet failed: {:?}",
                up_out.status
            )));
        }

        let mut compose_log_followers = Vec::new();
        let chain_log_follower = start_compose_log_follower(
            &compose_dir,
            &compose_files_owned,
            &compose_log_env,
            "chain-compose",
        )
        .inspect_err(|err| {
            logging::error(
                "smoke-test",
                format!("chain compose log follower failed: {err}"),
            );
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "log follower startup failure",
                200,
            );
            cleanup();
        })?;
        compose_log_followers.push(chain_log_follower);

        eprintln!("smoke-test: waiting for chain containers to become ready...");
        logging::info("smoke-test", "waiting for chain containers to become ready");
        let chain_probe_dir = compose_dir.clone();
        let mut chain_health_probe = compose_health_probe(
            &chain_probe_dir,
            &compose_files_owned,
            &compose_log_env,
            "chain-compose",
        );
        wait_for_rpc_with_probe(
            &rpc_url,
            Duration::from_secs(180),
            Some(&mut chain_health_probe),
        )
        .inspect_err(|err| {
            logging::error("smoke-test", format!("chain RPC readiness failed: {err}"));
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "RPC readiness timeout",
                200,
            );
            cleanup();
        })?;
        logging::info(
            "smoke-test",
            "chain RPC ready; waiting for EL/CL block production",
        );

        // Wait for real block production: RPC up != consensus up.
        wait_for_block_height_with_probe(
            &rpc_url,
            1,
            Duration::from_secs(240),
            Some(&mut chain_health_probe),
        )
        .inspect_err(|err| {
            logging::error(
                "smoke-test",
                format!("chain block-production readiness failed: {err}"),
            );
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "block-production timeout",
                200,
            );
            cleanup();
        })?;
        logging::info("smoke-test", "chain EL/CL stack ready");
        wait_for_rpc_with_probe(
            &rpc_url,
            Duration::from_secs(60),
            Some(&mut chain_health_probe),
        )
        .inspect_err(|err| {
            logging::error(
                "smoke-test",
                format!("post-readiness RPC stability check failed: {err}"),
            );
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "post-readiness RPC stability failure",
                200,
            );
            cleanup();
        })?;
        logging::info(
            "smoke-test",
            "post-readiness chain RPC stable; starting deployment",
        );

        // Deploy.s.sol run() performs a mandatory 1,000 USDC seed deposit from
        // the broadcaster (issue #656), so the deployer must hold USDC before
        // the forge script runs. Drip from HARNESS_USDC_HOLDER (genesis grant).
        const DEPLOYER_USDC_GRANT: u128 = 10_000 * 1_000_000; // 10k USDC, 6dp
        fund_usdc_to_deployer(&nonce_tracker, DEPLOYER_USDC_GRANT).inspect_err(|err| {
            logging::error("smoke-test", format!("deployer USDC funding failed: {err}"));
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "deployer USDC funding failure",
                200,
            );
            cleanup();
        })?;

        let dep_out = tmp.path().join("deployment.json");
        let agent_hex = format!("{:#x}", agent_address());
        run_forge_deploy_with_env(
            &repo_root,
            &rpc_url,
            &dep_out,
            &agent_hex,
            PAUSER_ADDRESS_HEX,
            extra_deploy_env,
        )
        .inspect_err(|err| {
            logging::error("smoke-test", format!("forge deploy failed: {err}"));
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "deployment failure",
                200,
            );
            cleanup();
        })?;

        let deployment = read_deployment(&dep_out)?;
        let chain_id = deployment.chain_id;

        // Deploy VaultRegistry and register RobotMoneyVault as the first active vault
        // (issue #294). The registry deployment JSON is written to a separate path in
        // the same tempdir so rmpc and downstream tooling can discover the registry
        // address without manual editing.
        let reg_out = tmp.path().join("registry.json");
        run_forge_deploy_registry(
            &repo_root,
            &rpc_url,
            &reg_out,
            &deployment.vault,
            &deployment.usdc,
        )
        .inspect_err(|err| {
            logging::error("smoke-test", format!("forge deploy registry failed: {err}"));
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "registry deployment failure",
                200,
            );
            cleanup();
        })?;

        let registry_deployment = read_registry_deployment(&reg_out)?;

        // Confirm the registry actually reports the vault as registered before
        // pinning the router simulation's fork block. `--slow` waits for the
        // registerVault receipt, but Geth's "latest" can still briefly lag the
        // mined block on a loaded runner, so fetching the head immediately can
        // pin a fork block that predates the registration — intermittently
        // failing the router deploy with NotRegistered() (issue #880).
        wait_for_vault_registered(&rpc_url, &registry_deployment.registry, &deployment.vault)
            .inspect_err(|err| {
                logging::error(
                    "smoke-test",
                    format!("vault registration wait failed: {err}"),
                );
                cleanup();
            })?;

        // Pin the fork block for the router simulation to the current chain
        // head — guarantees the simulation sees the registerVault tx that
        // run_forge_deploy_registry just mined (avoids a race where Geth
        // reports a stale "latest" before the block propagates to the RPC).
        let fork_block = fetch_current_block_number(&rpc_url).inspect_err(|err| {
            logging::error("smoke-test", format!("fetch block number failed: {err}"));
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "block number fetch failure",
                200,
            );
            cleanup();
        })?;

        // Deploy PortfolioRouter and wire initial weights (issue #303).
        // 10 000 bps → RobotMoneyVault as the sole active vault.
        let router_out = tmp.path().join("router.json");
        run_forge_deploy_router(
            &repo_root,
            &rpc_url,
            fork_block,
            &router_out,
            &registry_deployment.registry,
            &deployment.vault,
            &deployment.usdc,
        )
        .inspect_err(|err| {
            logging::error("smoke-test", format!("forge deploy router failed: {err}"));
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "router deployment failure",
                200,
            );
            cleanup();
        })?;

        let router_deployment = read_router_deployment(&router_out)?;

        // Deploy RouterGovernance and wire it to the PortfolioRouter (issue #364).
        // ADMIN_ROLE is held by the deployer; voting power is assigned per test.
        let governance_out = tmp.path().join("governance.json");
        run_forge_deploy_governance(
            &repo_root,
            &rpc_url,
            &governance_out,
            &router_deployment.router,
        )
        .inspect_err(|err| {
            logging::error(
                "smoke-test",
                format!("forge deploy governance failed: {err}"),
            );
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "governance deployment failure",
                200,
            );
            cleanup();
        })?;

        let governance_deployment = read_governance_deployment(&governance_out)?;

        // Deploy RmToken — the ERC-20 governance voting token (issue #365).
        // The entire initial supply is minted to HARNESS_USDC_HOLDER so that
        // `Fixture::fund_rm_token` can drip RM to test accounts using the same
        // signed-transfer pattern as `fund_usdc`. The deployer EOA signs the
        // broadcast; the initial holder receives the supply.
        let rm_token_out = tmp.path().join("rm-token.json");
        run_forge_deploy_rm_token(
            &repo_root,
            &rpc_url,
            &rm_token_out,
            HARNESS_USDC_HOLDER_ADDRESS_HEX,
        )
        .inspect_err(|err| {
            logging::error("smoke-test", format!("forge deploy rm-token failed: {err}"));
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "rm-token deployment failure",
                200,
            );
            cleanup();
        })?;

        let rm_token_deployment = read_rm_token_deployment(&rm_token_out)?;

        // Deploy demo-only extra vaults and reset the router weight vector to a
        // three-way split (issue #465). Two additional RobotMoneyVault
        // instances are registered via DeployDemoExtraVaults.s.sol; the basket
        // vaults ProtocolAssetVault/AgentTokenVault remain ADR-blocked (see
        // `docs/technical/basket-vault-gap-report.md`) so we ship passthrough-
        // backed stand-ins. The weight split mirrors the dapp's Router
        // Governance demo (5000/3000/2000 bps).
        let extra_vaults_out = tmp.path().join("demo-extra-vaults.json");
        run_forge_deploy_demo_extra_vaults(
            &repo_root,
            &rpc_url,
            &extra_vaults_out,
            &registry_deployment.registry,
            &router_deployment.router,
            &deployment.vault,
            &deployment.usdc,
        )
        .inspect_err(|err| {
            logging::error(
                "smoke-test",
                format!("forge deploy demo extra vaults failed: {err}"),
            );
            log_compose_state(
                &compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "demo extra vaults deployment failure",
                200,
            );
            cleanup();
        })?;

        let demo_extra_vaults = read_demo_extra_vaults_deployment(&extra_vaults_out)?;

        // Deploy Uniswap V3 stub pools via the Arachnid CREATE2 factory
        // (issue #531). Four `UniswapV3PoolSlot0Stub` instances are deployed at
        // deterministic addresses pre-committed in `config/dex-pools.json::devnet.pools`.
        // The Arachnid factory is pre-installed in the devnet genesis alloc by
        // `genesis_alloc::ARACHNID_FACTORY_ADDR`. This step makes the
        // landing-page price strip resolve slot0 on the fresh devnet instead of
        // rendering 'unavailable' (Base pool addresses have no bytecode here).
        let stubs_out = tmp.path().join("demo-uniswap-v3-stubs.json");
        run_forge_deploy_demo_uniswap_v3_stubs(&repo_root, &rpc_url, &stubs_out).inspect_err(
            |err| {
                logging::error(
                    "smoke-test",
                    format!("forge deploy demo uniswap v3 stubs failed: {err}"),
                );
                log_compose_state(
                    &compose_dir,
                    &compose_files_owned,
                    &compose_log_env,
                    "chain-compose",
                    "demo uniswap v3 stubs deployment failure",
                    200,
                );
                cleanup();
            },
        )?;

        let demo_uniswap_v3_stubs = read_demo_uniswap_v3_stubs_deployment(&stubs_out)?;

        // Deploy InvestmentCommitteePolicy + ConsensusRebalanceReceipt in one
        // ceremony (issue #1247 AC10, issue #1294). RECEIPT_ADMIN_ADDRESS is
        // left unset so the script defaults it to ADMIN_ADDRESS (the deployer)
        // for this devnet ceremony — in production it is the TimelockController.
        let ic_policy_out = tmp.path().join("ic-policy.json");
        run_forge_deploy_ic_policy(&repo_root, &rpc_url, &ic_policy_out, &deployment.gateway)
            .inspect_err(|err| {
                logging::error(
                    "smoke-test",
                    format!("forge deploy IC policy failed: {err}"),
                );
                log_compose_state(
                    &compose_dir,
                    &compose_files_owned,
                    &compose_log_env,
                    "chain-compose",
                    "IC policy deployment failure",
                    200,
                );
                cleanup();
            })?;

        let ic_policy_deployment = read_ic_policy_deployment(&ic_policy_out)?;

        fund_eth_from_deployer(&nonce_tracker, &agent_hex, "1000000000000000000").inspect_err(
            |err| {
                logging::error("smoke-test", format!("funding agent failed: {err}"));
                log_compose_state(
                    &compose_dir,
                    &compose_files_owned,
                    &compose_log_env,
                    "chain-compose",
                    "agent funding failure",
                    200,
                );
                cleanup();
            },
        )?;
        fund_eth_from_deployer(&nonce_tracker, PAUSER_ADDRESS_HEX, "1000000000000000000")
            .inspect_err(|err| {
                logging::error("smoke-test", format!("funding pauser failed: {err}"));
                log_compose_state(
                    &compose_dir,
                    &compose_files_owned,
                    &compose_log_env,
                    "chain-compose",
                    "funding failure",
                    200,
                );
                cleanup();
            })?;

        let fx = Fixture {
            compose_dir,
            tmp,
            compose_log_followers,
            chain_ports,
            rpc_port: chain_ports.rpc_port,
            rpc_url,
            chain_id,
            deployment,
            registry_deployment,
            router_deployment,
            governance_deployment,
            rm_token_deployment,
            ic_policy_deployment,
            demo_extra_vaults,
            demo_uniswap_v3_stubs,
            repo_root,
            nonce_tracker,
        };

        // Fund the agent's USDC balance. Deploy.s.sol no longer mints (USDC
        // is now real Base USDC seeded into genesis alloc, not MockUSDC), so
        // the harness funds via a real ERC-20 transfer from
        // HARNESS_USDC_HOLDER. Use a generous amount that comfortably
        // exceeds every scenario's deposit (largest is
        // OVER_PAYMENT_CAP_DEPOSIT = 20_000 USDC) but stays well under the
        // genesis grant (1M USDC by default in fork-block.json).
        const AGENT_USDC_GRANT: u128 = 500_000 * 1_000_000; // 500k USDC, 6dp
        fx.fund_usdc(fx.agent(), AGENT_USDC_GRANT)
            .inspect_err(|err| {
                logging::error("smoke-test", format!("funding USDC failed: {err}"));
                log_compose_state(
                    &fx.compose_dir,
                    &compose_files_owned,
                    &compose_log_env,
                    "chain-compose",
                    "USDC funding failure",
                    200,
                );
                cleanup();
            })?;

        // Seed two fixture consensus receipts (issue #1294) so the dapp e2e
        // spec has something to assert on: one verifies + is released, one
        // does not verify + stays unreleased + does not match live router
        // weights. Payload bytes only need to exist by the time the indexer's
        // first tick fetches `payload_uri` — well after `--full-stack` brings
        // up the `receipt-fixtures` compose service — so seeding here (before
        // that service exists) is safe.
        fx.seed_consensus_receipts().inspect_err(|err| {
            logging::error(
                "smoke-test",
                format!("consensus receipt fixture seeding failed: {err}"),
            );
            log_compose_state(
                &fx.compose_dir,
                &compose_files_owned,
                &compose_log_env,
                "chain-compose",
                "consensus receipt fixture seeding failure",
                200,
            );
            cleanup();
        })?;

        Ok(fx)
    }

    // ---- accessors --------------------------------------------------

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }
    pub fn rpc_port(&self) -> u16 {
        self.rpc_port
    }
    fn occupied_ports(&self) -> [u16; 4] {
        [
            self.chain_ports.rpc_port,
            self.chain_ports.ws_port,
            self.chain_ports.authrpc_port,
            self.chain_ports.beacon_port,
        ]
    }
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }
    pub fn gateway(&self) -> Address {
        parse_addr(&self.deployment.gateway)
    }
    pub fn usdc(&self) -> Address {
        parse_addr(&self.deployment.usdc)
    }
    pub fn vault(&self) -> Address {
        parse_addr(&self.deployment.vault)
    }
    /// AaveV3Adapter address registered with the vault at deploy time (issue #363).
    /// Returns `Address::ZERO` for legacy deployments that predate issue #363.
    pub fn aave_adapter(&self) -> Address {
        if self.deployment.aave_adapter.is_empty() {
            Address::ZERO
        } else {
            parse_addr(&self.deployment.aave_adapter)
        }
    }
    /// CompoundV3Adapter address registered with the vault at deploy time (issue #363).
    /// Returns `Address::ZERO` for legacy deployments that predate issue #363.
    pub fn compound_adapter(&self) -> Address {
        if self.deployment.compound_adapter.is_empty() {
            Address::ZERO
        } else {
            parse_addr(&self.deployment.compound_adapter)
        }
    }
    /// MorphoAdapter address registered with the vault at deploy time (issue #363).
    /// Returns `Address::ZERO` for legacy deployments that predate issue #363.
    pub fn morpho_adapter(&self) -> Address {
        if self.deployment.morpho_adapter.is_empty() {
            Address::ZERO
        } else {
            parse_addr(&self.deployment.morpho_adapter)
        }
    }
    pub fn agent(&self) -> Address {
        parse_addr(&self.deployment.agent)
    }
    pub fn share_receiver(&self) -> Address {
        parse_addr(&self.deployment.share_receiver)
    }
    pub fn gateway_runtime_hash(&self) -> &str {
        &self.deployment.gateway_runtime_hash
    }
    /// Raw string form of the gateway address (for TOML/config templating).
    pub fn gateway_hex(&self) -> &str {
        &self.deployment.gateway
    }
    /// Raw string form of the USDC address.
    pub fn usdc_hex(&self) -> &str {
        &self.deployment.usdc
    }
    /// Raw string form of the vault address.
    pub fn vault_hex(&self) -> &str {
        &self.deployment.vault
    }
    /// VaultRegistry address deployed by DeployVaultRegistry.s.sol (issue #294).
    /// RobotMoneyVault is registered as the first active vault in this registry.
    pub fn registry(&self) -> Address {
        parse_addr(&self.registry_deployment.registry)
    }
    /// Raw string form of the VaultRegistry address.
    pub fn registry_hex(&self) -> &str {
        &self.registry_deployment.registry
    }
    /// PortfolioRouter address deployed by DeployPortfolioRouter.s.sol (issue #303).
    /// Initial weights: 10 000 bps to RobotMoneyVault.
    pub fn router(&self) -> Address {
        parse_addr(&self.router_deployment.router)
    }
    /// Raw string form of the PortfolioRouter address.
    pub fn router_hex(&self) -> &str {
        &self.router_deployment.router
    }
    /// RouterGovernance address deployed by DeployRouterGovernance.s.sol (issue #364).
    /// Deployer holds ADMIN_ROLE; voting power is assigned per test via setVotingPower.
    pub fn governance(&self) -> Address {
        parse_addr(&self.governance_deployment.governance)
    }
    /// Raw string form of the RouterGovernance address.
    pub fn governance_hex(&self) -> &str {
        &self.governance_deployment.governance
    }
    /// RmToken ERC-20 address deployed by DeployRmToken.s.sol (issue #365).
    /// The entire initial supply is held by HARNESS_USDC_HOLDER; tests drip
    /// RM to recipients via `Fixture::fund_rm_token`.
    pub fn rm_token(&self) -> Address {
        parse_addr(&self.rm_token_deployment.rm_token)
    }
    /// Raw string form of the RmToken address.
    pub fn rm_token_hex(&self) -> &str {
        &self.rm_token_deployment.rm_token
    }
    /// `InvestmentCommitteePolicy` deployed by
    /// `DeployInvestmentCommitteePolicy.s.sol` (issue #1247/#1294).
    pub fn ic_policy(&self) -> Address {
        parse_addr(&self.ic_policy_deployment.policy)
    }
    /// Raw string form of the InvestmentCommitteePolicy address.
    pub fn ic_policy_hex(&self) -> &str {
        &self.ic_policy_deployment.policy
    }
    /// `ConsensusRebalanceReceipt` deployed in the same ceremony as
    /// [`Fixture::ic_policy`] (issue #1247 AC10, issue #1294).
    pub fn consensus_receipt(&self) -> Address {
        parse_addr(&self.ic_policy_deployment.consensus_receipt)
    }
    /// Raw string form of the ConsensusRebalanceReceipt address.
    pub fn consensus_receipt_hex(&self) -> &str {
        &self.ic_policy_deployment.consensus_receipt
    }

    /// `ProtocolAssetVault` (PRD §11.2). Seeded with devnet stand-in basket
    /// tokens (wETH, cbBTC, wSOL) and made router-eligible by the demo seed
    /// override — see DeployDemoExtraVaults.s.sol header.
    pub fn demo_protocol_vault(&self) -> Address {
        parse_addr(&self.demo_extra_vaults.protocol_vault)
    }
    /// Raw string form of the ProtocolAssetVault address.
    pub fn demo_protocol_vault_hex(&self) -> &str {
        &self.demo_extra_vaults.protocol_vault
    }
    /// `AgentTokenVault` (PRD §11.3). Seeded with the six MVP shortlist
    /// symbols and made router-eligible by the demo seed override.
    pub fn demo_agent_vault(&self) -> Address {
        parse_addr(&self.demo_extra_vaults.agent_token_vault)
    }
    /// Raw string form of the AgentTokenVault address.
    pub fn demo_agent_vault_hex(&self) -> &str {
        &self.demo_extra_vaults.agent_token_vault
    }
    /// Convenience accessor returning the three non-RWA router-eligible Active
    /// vaults in the demo set (primary §11.1, protocol §11.2, agent §11.3). After
    /// issues #559/#560 all three carry router weight and receive router flow
    /// from `seed_demo_depositors`. The §11.4 RWA vault is separately returned by
    /// [`Fixture::rwa_vault`] — after issue #621 it is also router-eligible at
    /// 500 bps (ADR-0006 §1 amended 2026-06-05).
    pub fn all_demo_vaults(&self) -> [Address; 3] {
        [
            self.vault(),
            self.demo_protocol_vault(),
            self.demo_agent_vault(),
        ]
    }
    /// RWA/Thematic vault (deSPXA) registered by `DeployDemoExtraVaults.s.sol`
    /// (PRD §11.4, ADR-0006). Registered **Active** and router-eligible at
    /// 500 bps (issue #621, ADR-0006 §1 amended 2026-06-05 — product owner
    /// confirmed). The router deposit routes 500 bps of each deposit into rmRWA
    /// via the Aerodrome swap + Chronicle NAV oracle path. `seed_demo_depositors`
    /// also issues a direct deposit for belt-and-suspenders TVL seeding.
    pub fn rwa_vault(&self) -> Address {
        parse_addr(&self.demo_extra_vaults.rwa_vault)
    }
    /// Raw string form of the RWA/Thematic placeholder address.
    pub fn rwa_vault_hex(&self) -> &str {
        &self.demo_extra_vaults.rwa_vault
    }
    /// JSON `{rmUSDC,rmPROTO,rmAGENT,rmRWA}` bucket-vault-symbol map matching
    /// `tests/fixtures/consensus-receipt.bucket-vault-map.json`'s
    /// `required_vault_symbols` (issue #1294). Threaded through as
    /// `VITE_VAULT_ADDRESSES` so `consensusReceiptApi.ts::parseVaultAddressMap`
    /// can compute the applied-vs-not-applied consensus receipt state against
    /// live router weights instead of always reporting "cannot determine".
    pub fn vault_address_map_json(&self) -> String {
        serde_json::json!({
            "rmUSDC": format!("{:#x}", self.vault()),
            "rmPROTO": format!("{:#x}", self.demo_protocol_vault()),
            "rmAGENT": format!("{:#x}", self.demo_agent_vault()),
            "rmRWA": format!("{:#x}", self.rwa_vault()),
        })
        .to_string()
    }
    /// Router weight (bps) assigned to the primary vault after demo seeding.
    /// The primary is the only router-eligible vault, so this is always
    /// 10 000.
    pub fn demo_weight_primary_bps(&self) -> u64 {
        DEMO_WEIGHT_PRIMARY_BPS
    }

    /// ETH/USD devnet stub pool address (issue #531). Deployed by
    /// `DeployDemoUniswapV3Stubs.s.sol` via Arachnid CREATE2 factory.
    /// Pre-committed address in `config/dex-pools.json::devnet.pools.eth-usd`.
    pub fn stub_pool_eth_usd(&self) -> Address {
        parse_addr(&self.demo_uniswap_v3_stubs.eth_usd)
    }
    /// wETH/USDC devnet stub pool address (issue #531).
    pub fn stub_pool_weth_usdc(&self) -> Address {
        parse_addr(&self.demo_uniswap_v3_stubs.weth_usdc)
    }
    /// cbBTC/USDC devnet stub pool address (issue #531).
    pub fn stub_pool_cbbtc_usdc(&self) -> Address {
        parse_addr(&self.demo_uniswap_v3_stubs.cbbtc_usdc)
    }
    /// wSOL/USDC devnet stub pool address (issue #531).
    pub fn stub_pool_wsol_usdc(&self) -> Address {
        parse_addr(&self.demo_uniswap_v3_stubs.wsol_usdc)
    }
    /// All four devnet stub pool addresses in `config/dex-pools.json` order:
    /// [eth-usd, weth-usdc, cbbtc-usdc, wsol-usdc] (issue #531).
    pub fn all_stub_pools(&self) -> [Address; 4] {
        [
            self.stub_pool_eth_usd(),
            self.stub_pool_weth_usdc(),
            self.stub_pool_cbbtc_usdc(),
            self.stub_pool_wsol_usdc(),
        ]
    }

    /// Path to the fixture's private tempdir. Callers may write
    /// additional files (keystores, client configs) here.
    pub fn tempdir(&self) -> &Path {
        self.tmp.path()
    }
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    // ---- on-chain poke helpers --------------------------------------

    /// Send a transaction via `cast send` from an arbitrary private key.
    ///
    /// The gas limit is the node's `eth_estimateGas` result scaled by a 1.5x
    /// buffer (see [`Fixture::estimate_gas_buffered`]) rather than the bare
    /// estimate `cast send` would otherwise forward. This mirrors production
    /// wallets (MetaMask et al. always pad) and the dapp-e2e mock wallet fix
    /// from issue #897: Geth under-estimates transactions whose true cost grows
    /// with same-block state — e.g. several depositors routing into the same
    /// strategy adapter in one block, where the later txs cost more than an
    /// estimate taken against the pre-block state — or that earn storage-clear
    /// gas refunds. Without the buffer those txs mine with too low a gas cap and
    /// revert out-of-gas; with `cast send` reporting only the receipt, the
    /// failure was silent. See `docs/testing/geth-gas-estimation.md`.
    ///
    /// Reverted transactions (receipt `status != 0x1`, including out-of-gas) now
    /// surface as an `Err` instead of an `Ok(tx_hash)`, so a failed deposit can
    /// no longer masquerade as a successful one.
    ///
    /// ## Send-layer failure policy (issue #1241)
    ///
    /// This is the fourth instance of the `latest`-pinned read-after-write
    /// class documented in `docs/testing/geth-state-lag.md`: `cast send`
    /// without an explicit `--nonce` derives one itself from
    /// `eth_getTransactionCount(from, "latest")`, and a receipt-confirmed
    /// prior send does not guarantee that read reflects it yet. Instead of
    /// leaning on cast's implicit lookup, [`Fixture::pin_next_nonce`] makes
    /// this harness the nonce source of truth for every sender: it queries
    /// the `pending`-tagged count (not `latest`) and, once it has pinned a
    /// nonce for an address, polls until a later pin reads strictly past it.
    ///
    /// A failed send is then classified (see [`classify_send_failure`])
    /// rather than always retried:
    /// - `replacement transaction underpriced` means the node refused the
    ///   send outright — nothing entered the chain, so re-sending with the
    ///   same pinned nonce is idempotent and safe.
    /// - `already known` / `nonce too low` mean the write may already have
    ///   landed. Blindly re-sending here could double-apply it (e.g. a
    ///   second `deposit`), so these are resolved by looking up the receipt
    ///   for whatever transaction actually consumed the pinned nonce
    ///   ([`Fixture::find_receipt_for_nonce`]) — never by re-sending. See
    ///   [`run_cast_send_retry`] for the shared decision logic.
    pub fn cast_send(
        &self,
        private_key_hex: &str,
        to: Address,
        sig: &str,
        args: &[&str],
    ) -> Result<String, HarnessError> {
        let to_hex = format!("{to:#x}");
        let from = derive_address(&privkey_hex_to_bytes(private_key_hex)?);
        let from_hex = format!("{from:#x}");
        logging::debug(
            "rpc",
            format!("eth_sendRawTransaction via cast send {sig} -> {to_hex}"),
        );
        let gas_limit = self.estimate_gas_buffered(&from_hex, &to_hex, sig, args)?;
        let gas_limit_s = gas_limit.to_string();
        let nonce = self.nonce_tracker.pin_next_nonce(&from_hex)?;
        let nonce_s = nonce.to_string();

        let v = run_cast_send_retry(
            NONCE_RETRY_DELAYS.len() as u32,
            |attempt| {
                if !NONCE_RETRY_DELAYS[attempt as usize].is_zero() {
                    thread::sleep(NONCE_RETRY_DELAYS[attempt as usize]);
                }
                let mut cmd = Command::new("cast");
                cmd.args([
                    "send",
                    "--rpc-url",
                    &self.rpc_url,
                    "--private-key",
                    private_key_hex,
                    "--gas-limit",
                    &gas_limit_s,
                    "--nonce",
                    &nonce_s,
                    &to_hex,
                    sig,
                ]);
                for a in args {
                    cmd.arg(a);
                }
                cmd.arg("--json");
                let out = match cmd.output() {
                    Ok(out) => out,
                    Err(e) => {
                        return SendAttemptOutcome::Failed(
                            SendFailureClass::Hard,
                            format!("cast send {sig} IO error: {e}"),
                        )
                    }
                };
                logging::log_command_output("cast", &out);
                if out.status.success() {
                    return match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                        Ok(v) => SendAttemptOutcome::Mined(v),
                        Err(e) => SendAttemptOutcome::Failed(
                            SendFailureClass::Hard,
                            format!("cast send {sig} json: {e}"),
                        ),
                    };
                }
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let class = classify_send_failure(&stderr);
                SendAttemptOutcome::Failed(
                    class,
                    format!("cast send {sig} failed: stdout={stdout} stderr={stderr}"),
                )
            },
            || self.nonce_tracker.find_receipt_for_nonce(&from_hex, nonce),
        )
        .inspect_err(|_| self.nonce_tracker.release_pin_if_unused(&from_hex, nonce))?;

        let tx_hash = v
            .get("transactionHash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        // `cast send` exits 0 as soon as the tx mines — even when it mines as a
        // REVERTED tx (receipt `status == 0x0`). Gas estimation catches most
        // reverts up front, but a tx whose state changes between estimate and
        // inclusion (or one sent with an explicit `--gas-limit`) can land
        // reverted with a zero process exit. Assert the receipt status so a
        // reverted seeding tx fails loudly instead of being silently accepted.
        let status = v.get("status").and_then(|x| x.as_str()).ok_or_else(|| {
            HarnessError::other(format!("cast send {sig} receipt missing status field: {v}"))
        })?;
        if !receipt_status_succeeded(status) {
            let reason = self.tx_revert_reason(&tx_hash);
            return Err(HarnessError::other(format!(
                "cast send {sig} -> {to_hex} mined as a REVERTED tx (receipt status={status}, hash={tx_hash}){reason}"
            )));
        }
        Ok(tx_hash)
    }
}

// -- NonceTracker -----------------------------------------------------

/// The nonce this harness pins for the next send from an address, given the
/// last nonce it pinned for that address (`None` on the first send) and the
/// node's current `pending` transaction count (issues #1241, #1374).
///
/// The node's count is a *lower* bound, never an authority: geth reports a
/// `pending` count that has not yet absorbed an in-flight send, so two sends
/// issued close together both read the same number. Taking `prev + 1`
/// whenever the node has not moved past the last pin makes the harness — not
/// the node's read timing — the monotonic source of truth for each sender's
/// nonce sequence. That is what makes two same-account sends structurally
/// unable to collide, rather than merely unlikely to.
fn next_pinned_nonce(prev: Option<u64>, pending: u64) -> u64 {
    match prev {
        Some(p) => pending.max(p + 1),
        None => pending,
    }
}

/// Harness-owned nonce source of truth for every EOA the devnet harness
/// sends from (issue #1241, extended by issue #1374).
///
/// One tracker exists per devnet instance and is created before the first
/// funding send in [`Fixture::new`], so boot-time funding, deploy-time
/// funding and every later [`Fixture::cast_send`] share a single nonce
/// sequence per sender. Before #1374 the funding helpers
/// (`fund_eth_from_deployer`, `fund_usdc_to_deployer`,
/// [`Fixture::fund_eth_from_harness`]) bypassed the tracker entirely and let
/// `cast send` derive its own nonce from a `latest`-tagged read — the read
/// that lags a just-sent transaction — so two funding sends from the same
/// account collided and geth rejected the second with `replacement
/// transaction underpriced`, reddening PRs whose diffs could not have caused
/// it. All sends now route through [`NonceTracker::pin_next_nonce`].
pub(crate) struct NonceTracker {
    rpc_url: String,
    /// Per-sender last-pinned nonce, keyed by lowercase `0x`-EOA hex. Scoped
    /// to one devnet instance rather than a process-global map, so nonce
    /// state from one test's devnet never leaks into another test's freshly
    /// reset chain.
    pins: Mutex<HashMap<String, u64>>,
}

impl NonceTracker {
    fn new(rpc_url: impl Into<String>) -> Self {
        Self {
            rpc_url: rpc_url.into(),
            pins: Mutex::new(HashMap::new()),
        }
    }

    fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Pin the nonce for the next send from `from_hex` rather than letting
    /// `cast send` derive it implicitly (issue #1241).
    ///
    /// `cast send` without `--nonce` calls
    /// `eth_getTransactionCount(from, "latest")` itself — exactly the
    /// `latest`-pinned read-after-write class documented in
    /// `docs/testing/geth-state-lag.md`: a receipt-confirmed send does not
    /// guarantee that read reflects it yet, so the next implicit lookup can
    /// return a stale (too-low) nonce and collide with the previous send.
    ///
    /// Instead this queries the `pending`-tagged count (mempool-inclusive,
    /// not mined-only) and hands out `next_pinned_nonce(prev, pending)` — the
    /// node's count only when it has already moved past the last pin, and
    /// `prev + 1` otherwise.
    ///
    /// ## Why the lock spans the read (issue #1374)
    ///
    /// The map lock is held across the `pending` read *and* the insert, so
    /// nonce issuance for a given tracker is serialised. Releasing it around
    /// the RPC — as this did before #1374 — let two concurrent callers both
    /// observe the same `prev`, read the same `pending`, and pin the *same*
    /// nonce; the second send to reach geth was then rejected with
    /// `replacement transaction underpriced`. `seed_demo_depositors` funds
    /// from two shared keys on two scoped threads, so that window was live.
    fn pin_next_nonce(&self, from_hex: &str) -> Result<u64, HarnessError> {
        self.pin_next_nonce_with(from_hex, |addr| {
            self.eth_get_transaction_count(addr, "pending")
        })
    }

    /// [`NonceTracker::pin_next_nonce`] with the `pending`-count read
    /// injected, so the serialisation invariant is exercisable without a
    /// chain (see `tests::concurrent_pins_never_hand_out_a_colliding_nonce`).
    fn pin_next_nonce_with(
        &self,
        from_hex: &str,
        read_pending: impl Fn(&str) -> Result<u64, HarnessError>,
    ) -> Result<u64, HarnessError> {
        let key = from_hex.to_lowercase();
        // Held across the read: see this method's "Why the lock spans the
        // read" note. `unwrap_or_else(into_inner)` keeps a panicking caller
        // from poisoning every later send.
        let mut pins = self.pins.lock().unwrap_or_else(|e| e.into_inner());
        let pending = read_pending(from_hex)?;
        let nonce = next_pinned_nonce(pins.get(&key).copied(), pending);
        pins.insert(key, nonce);
        Ok(nonce)
    }

    /// Hand a pinned nonce back after a send that failed (issue #1374).
    ///
    /// Pins are monotonic, so a nonce pinned for a send that never reached
    /// the chain — an unfunded faucet, a bad RPC URL — would otherwise leave
    /// a permanent gap: every later send from that address would pin past the
    /// unused nonce and sit in the mempool forever. That would turn one loud
    /// funding failure into a silent hang, the opposite of what this issue is
    /// for.
    ///
    /// Releasing is conditional, never blind: it asks the node for the
    /// `pending` count first and keeps the pin if anything at all — mined or
    /// merely queued — already consumed the nonce, so a send whose success
    /// response was merely lost can never be re-issued under a nonce that is
    /// already spoken for. Best-effort by construction: if the node cannot be
    /// reached the pin simply stands, because the caller is already
    /// propagating a hard error.
    fn release_pin_if_unused(&self, from_hex: &str, nonce: u64) {
        let read = |addr: &str| self.eth_get_transaction_count(addr, "pending");
        self.release_pin_if_unused_with(from_hex, nonce, read);
    }

    /// [`NonceTracker::release_pin_if_unused`] with the `pending`-count read
    /// injected, so its "only when genuinely unused" rule is testable without
    /// a chain.
    fn release_pin_if_unused_with(
        &self,
        from_hex: &str,
        nonce: u64,
        read_pending: impl Fn(&str) -> Result<u64, HarnessError>,
    ) {
        let Ok(pending) = read_pending(from_hex) else {
            return;
        };
        if pending > nonce {
            // Something consumed it — keep the pin.
            return;
        }
        let key = from_hex.to_lowercase();
        let mut pins = self.pins.lock().unwrap_or_else(|e| e.into_inner());
        if pins.get(&key).copied() != Some(nonce) {
            // A later send already moved this sender on; not ours to rewind.
            return;
        }
        match nonce.checked_sub(1) {
            Some(prev) => pins.insert(key, prev),
            None => pins.remove(&key),
        };
    }

    /// Query `eth_getTransactionCount(address, tag)` via `cast rpc`. `tag` is
    /// `"pending"` (mempool-inclusive; used to pin the next send's nonce) or
    /// `"latest"` (mined-only; used to detect that a pinned nonce has
    /// actually landed — see [`Fixture::find_receipt_for_nonce`]).
    fn eth_get_transaction_count(&self, address_hex: &str, tag: &str) -> Result<u64, HarnessError> {
        let out = Command::new("cast")
            .args([
                "rpc",
                "--rpc-url",
                &self.rpc_url,
                "eth_getTransactionCount",
                address_hex,
                tag,
            ])
            .output()?;
        if !out.status.success() {
            return Err(HarnessError::other(format!(
                "eth_getTransactionCount({address_hex}, {tag}) failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        parse_hex_rpc_result(
            &out.stdout,
            &format!("eth_getTransactionCount({address_hex}, {tag})"),
        )
    }

    /// Query `eth_blockNumber` via `cast rpc`. Used by
    /// [`Fixture::find_receipt_for_nonce`] to bound the block-scan window.
    fn eth_block_number(&self) -> Result<u64, HarnessError> {
        let out = Command::new("cast")
            .args(["rpc", "--rpc-url", &self.rpc_url, "eth_blockNumber"])
            .output()?;
        if !out.status.success() {
            return Err(HarnessError::other(format!(
                "eth_blockNumber failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        parse_hex_rpc_result(&out.stdout, "eth_blockNumber")
    }

    /// Fetch a full block (with transaction bodies) via `cast rpc
    /// eth_getBlockByNumber`. Used by [`Fixture::find_receipt_for_nonce`] to
    /// scan for the transaction that consumed a given `(from, nonce)`.
    fn eth_get_block_by_number(&self, block_num: u64) -> Result<serde_json::Value, HarnessError> {
        let out = Command::new("cast")
            .args([
                "rpc",
                "--rpc-url",
                &self.rpc_url,
                "eth_getBlockByNumber",
                &format!("{block_num:#x}"),
                "true",
            ])
            .output()?;
        if !out.status.success() {
            return Err(HarnessError::other(format!(
                "eth_getBlockByNumber({block_num}) failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        serde_json::from_slice(&out.stdout).map_err(|e| {
            HarnessError::other(format!("eth_getBlockByNumber({block_num}) json: {e}"))
        })
    }

    /// Fetch a mined transaction's receipt via `cast receipt --json`. Shares
    /// the same top-level shape (`transactionHash`, `status`, ...) as `cast
    /// send --json`'s output, so callers can treat both uniformly.
    fn fetch_receipt(&self, tx_hash: &str) -> Result<serde_json::Value, HarnessError> {
        let out = Command::new("cast")
            .args(["receipt", "--rpc-url", &self.rpc_url, tx_hash, "--json"])
            .output()?;
        if !out.status.success() {
            return Err(HarnessError::other(format!(
                "cast receipt {tx_hash} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|e| HarnessError::other(format!("cast receipt {tx_hash} json: {e}")))
    }

    /// Resolve an ambiguous send outcome (`already known` / `nonce too low`)
    /// by finding the transaction that actually consumed `nonce` and
    /// returning its receipt, instead of re-sending (issue #1241).
    ///
    /// [`Fixture::pin_next_nonce`] makes this harness the sole nonce source
    /// for `from_hex`, so whichever transaction consumed `nonce` — this
    /// attempt or an earlier one whose success response was lost — IS the
    /// write this call intended; re-sending on ambiguity risks double-
    /// applying it (e.g. a double deposit). This waits (bounded, 30s
    /// deadline / 500ms poll — the same shape as
    /// `wait_for_vault_registered`'s registry-visibility poll) for the
    /// mined transaction count to pass `nonce`, then scans back through
    /// recent blocks for the `(from, nonce)` transaction and returns its
    /// receipt.
    fn find_receipt_for_nonce(
        &self,
        from_hex: &str,
        nonce: u64,
    ) -> Result<serde_json::Value, HarnessError> {
        const DEADLINE: Duration = Duration::from_secs(30);
        const POLL_INTERVAL: Duration = Duration::from_millis(500);
        let start = std::time::Instant::now();
        loop {
            let mined = self.eth_get_transaction_count(from_hex, "latest")?;
            if mined > nonce {
                break;
            }
            if start.elapsed() >= DEADLINE {
                return Err(HarnessError::other(format!(
                    "ambiguous send outcome for {from_hex} nonce {nonce} never resolved: mined \
                     transaction count still {mined} after {DEADLINE:?} — refusing to resend a \
                     write whose effect is unknown"
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }

        let from_lower = from_hex.to_lowercase();
        let latest_block = self.eth_block_number()?;
        const SCAN_BLOCKS: u64 = 20;
        let earliest = latest_block.saturating_sub(SCAN_BLOCKS);
        for block_num in (earliest..=latest_block).rev() {
            let block = self.eth_get_block_by_number(block_num)?;
            let Some(txs) = block.get("transactions").and_then(|t| t.as_array()) else {
                continue;
            };
            for tx in txs {
                let tx_from = tx.get("from").and_then(|f| f.as_str()).unwrap_or_default();
                let tx_nonce = tx
                    .get("nonce")
                    .and_then(|n| n.as_str())
                    .and_then(|n| u64::from_str_radix(n.trim_start_matches("0x"), 16).ok());
                if tx_from.eq_ignore_ascii_case(&from_lower) && tx_nonce == Some(nonce) {
                    let hash = tx.get("hash").and_then(|h| h.as_str()).ok_or_else(|| {
                        HarnessError::other(format!(
                            "found tx for {from_hex} nonce {nonce} in block {block_num} with no hash field"
                        ))
                    })?;
                    return self.fetch_receipt(hash);
                }
            }
        }
        Err(HarnessError::other(format!(
            "mined transaction count for {from_hex} passed nonce {nonce}, but no matching \
             transaction was found scanning the last {SCAN_BLOCKS} blocks ({earliest}..={latest_block})"
        )))
    }
}

impl Fixture {
    /// Best-effort decode of a reverted tx's revert reason via `cast run`,
    /// appended to the harness error for fast diagnosis. Returns an empty
    /// string when no reason can be recovered (e.g. `cast run` unavailable),
    /// so the caller's error stays well-formed regardless.
    fn tx_revert_reason(&self, tx_hash: &str) -> String {
        if tx_hash.is_empty() {
            return String::new();
        }
        let out = match Command::new("cast")
            .args(["run", "--rpc-url", &self.rpc_url, tx_hash])
            .output()
        {
            Ok(out) => out,
            Err(_) => return String::new(),
        };
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let reason = text
            .lines()
            .find(|l| {
                let l = l.to_lowercase();
                l.contains("revert") || l.contains("error")
            })
            .map(str::trim)
            .unwrap_or("");
        if reason.is_empty() {
            String::new()
        } else {
            format!("; revert: {reason}")
        }
    }

    /// Read `token.balanceOf(owner)` via a plain `eth_call` (`cast call`).
    /// Used to verify deposits actually minted shares to the recipient. No
    /// signing, no impersonation — a read-only query against the live chain.
    pub fn erc20_balance_of(&self, token: Address, owner: Address) -> Result<u128, HarnessError> {
        // balanceOf(address) selector 0x70a08231, owner left-padded to 32 bytes.
        let data = format!(
            "0x70a08231000000000000000000000000{}",
            format!("{owner:#x}").trim_start_matches("0x")
        );
        let out = Command::new("cast")
            .args([
                "call",
                "--rpc-url",
                &self.rpc_url,
                &format!("{token:#x}"),
                &data,
            ])
            .output()?;
        if !out.status.success() {
            return Err(HarnessError::other(format!(
                "cast call balanceOf({owner:#x}) on {token:#x} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        let s = raw.trim().trim_start_matches("0x");
        if s.is_empty() || s.chars().all(|c| c == '0') {
            return Ok(0);
        }
        // The ABI word is up to 256 bits. The demo seeds share counts well
        // within u128, but a value past 2^128 must not silently read back as
        // zero: if the high half is non-zero, saturate so the non-zero check
        // stays honest; otherwise return the exact low-128-bit value.
        let low = &s[s.len().saturating_sub(32)..];
        let high_nonzero = s.len() > 32 && s[..s.len() - 32].chars().any(|c| c != '0');
        let low_val = u128::from_str_radix(low, 16).unwrap_or(u128::MAX);
        Ok(if high_nonzero { u128::MAX } else { low_val })
    }

    /// Read `token.allowance(owner, spender)` via a plain `eth_call`
    /// (`cast call`). Used to confirm an `approve` is visible on-chain before
    /// the dependent `transferFrom`/`deposit` is sent. No signing — a read-only
    /// query against the live chain.
    ///
    /// This is the read half of the poll-after-write fix for the Geth
    /// read-after-write state-lag class; canonical doc:
    /// `docs/testing/geth-state-lag.md`.
    pub fn erc20_allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<u128, HarnessError> {
        // allowance(address,address) selector 0xdd62ed3e, owner then spender
        // each left-padded to 32 bytes.
        let data = format!(
            "0xdd62ed3e000000000000000000000000{}000000000000000000000000{}",
            format!("{owner:#x}").trim_start_matches("0x"),
            format!("{spender:#x}").trim_start_matches("0x"),
        );
        let out = Command::new("cast")
            .args([
                "call",
                "--rpc-url",
                &self.rpc_url,
                &format!("{token:#x}"),
                &data,
            ])
            .output()?;
        if !out.status.success() {
            return Err(HarnessError::other(format!(
                "cast call allowance({owner:#x},{spender:#x}) on {token:#x} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        let s = raw.trim().trim_start_matches("0x");
        if s.is_empty() || s.chars().all(|c| c == '0') {
            return Ok(0);
        }
        // Same up-to-256-bit decode as `erc20_balance_of`: saturate when the
        // high half is non-zero so the visibility check stays honest.
        let low = &s[s.len().saturating_sub(32)..];
        let high_nonzero = s.len() > 32 && s[..s.len() - 32].chars().any(|c| c != '0');
        let low_val = u128::from_str_radix(low, 16).unwrap_or(u128::MAX);
        Ok(if high_nonzero { u128::MAX } else { low_val })
    }

    /// `approve(spender, amount)` on `token` from `private_key_hex`, then poll
    /// `allowance(owner, spender)` until the on-chain read reflects the
    /// approved amount before returning.
    ///
    /// On the Geth devnet, `cast send` exits as soon as the approve tx mines,
    /// but Geth's state-read can briefly race the state-update so an immediate
    /// `eth_call`/`transferFrom` still sees the pre-approve (zero) allowance —
    /// the same state-lag fixed for the registry harness in issue #1081 / PR
    /// #1094. That surfaced here as an intermittent
    /// `ERC20: transfer amount exceeds allowance` revert on the dependent
    /// deposit. Polling the allowance read (>=5 attempts, 200ms apart) closes
    /// that read-after-write window deterministically — a real race fix, not a
    /// skip. Canonical doc: `docs/testing/geth-state-lag.md`.
    fn approve_and_confirm(
        &self,
        private_key_hex: &str,
        token: Address,
        owner: Address,
        spender: Address,
        amount: u128,
    ) -> Result<String, HarnessError> {
        let spender_hex = format!("{spender:#x}");
        let tx = self.cast_send(
            private_key_hex,
            token,
            "approve(address,uint256)",
            &[&spender_hex, &amount.to_string()],
        )?;

        const ATTEMPTS: u32 = 5;
        const INTERVAL: Duration = Duration::from_millis(200);
        for attempt in 0..ATTEMPTS {
            let allowance = self.erc20_allowance(token, owner, spender)?;
            if allowance >= amount {
                return Ok(tx);
            }
            if attempt + 1 < ATTEMPTS {
                thread::sleep(INTERVAL);
            }
        }
        Err(HarnessError::other(format!(
            "approve({spender_hex}, {amount}) on {token:#x} not visible to {owner:#x} after \
             {ATTEMPTS} attempts ({INTERVAL:?} apart): Geth state-lag did not settle (tx={tx})"
        )))
    }

    /// Estimate gas for a `cast send` and return a 1.5x-buffered limit.
    ///
    /// A failing estimate means the transaction would revert on-chain (the node
    /// rejects the `eth_estimateGas` call); we propagate that as an `Err` so the
    /// caller never sends a doomed transaction. See `cast_send` for why the
    /// buffer is required.
    fn estimate_gas_buffered(
        &self,
        from_hex: &str,
        to_hex: &str,
        sig: &str,
        args: &[&str],
    ) -> Result<u128, HarnessError> {
        let mut cmd = Command::new("cast");
        cmd.args([
            "estimate",
            "--rpc-url",
            &self.rpc_url,
            "--from",
            from_hex,
            to_hex,
            sig,
        ]);
        for a in args {
            cmd.arg(a);
        }
        let out = cmd.output()?;
        logging::log_command_output("cast", &out);
        if !out.status.success() {
            return Err(HarnessError::other(format!(
                "cast estimate {sig} failed (tx would revert): from={from_hex} to={to_hex} stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let est: u128 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .map_err(|e| HarnessError::other(format!("cast estimate {sig} parse: {e}")))?;
        // 1.5x buffer, matching the dapp-e2e mock-wallet policy (issue #897).
        Ok(est.saturating_mul(3) / 2)
    }

    /// Approve `gateway` to pull `amount` USDC from the agent EOA.
    pub fn approve_usdc_from_agent(&self, amount: u128) -> Result<String, HarnessError> {
        let agent_pk_hex = format!("0x{}", hex::encode(AGENT_PRIVATE_KEY));
        self.cast_send(
            &agent_pk_hex,
            self.usdc(),
            "approve(address,uint256)",
            &[&format!("{:#x}", self.gateway()), &amount.to_string()],
        )
    }

    /// Pause the gateway from the PAUSER_ROLE holder.
    pub fn pause_gateway(&self) -> Result<String, HarnessError> {
        self.cast_send(PAUSER_PRIVATE_KEY_HEX, self.gateway(), "pause()", &[])
    }

    /// Unpause the gateway. Unpause is ADMIN_ROLE-only.
    pub fn unpause_gateway(&self) -> Result<String, HarnessError> {
        self.cast_send(DEPLOYER_PRIVATE_KEY_HEX, self.gateway(), "unpause()", &[])
    }

    /// Revoke the agent's `AGENT_ROLE`.
    pub fn revoke_agent(&self) -> Result<String, HarnessError> {
        self.cast_send(
            DEPLOYER_PRIVATE_KEY_HEX,
            self.gateway(),
            "revokeAgent(address)",
            &[&format!("{:#x}", self.agent())],
        )
    }

    /// Re-grant the agent's `AGENT_ROLE` with the given policy caps.
    pub fn reauthorize_agent(
        &self,
        max_per_payment: u128,
        max_per_window: u128,
    ) -> Result<String, HarnessError> {
        let agent = format!("{:#x}", self.agent());
        let share_receiver = format!("{:#x}", self.share_receiver());
        // allowedDestinations left empty ([]) for smoke-test: open policy
        // allows any registered destination (vault or router).
        let policy = format!(
            "(true,18446744073709551615,{max_per_payment},{max_per_window},{share_receiver},[],0x0000000000000000000000000000000000000000,0,0,[])"
        );
        self.cast_send(
            DEPLOYER_PRIVATE_KEY_HEX,
            self.gateway(),
            "authorizeAgent(address,(bool,uint64,uint256,uint256,address,address[],address,uint256,uint256,address[]))",
            &[&agent, &policy],
        )
    }

    /// Assign `power` voting power to `voter` on the RouterGovernance contract.
    /// Callable by the deployer, who holds ADMIN_ROLE on the governance contract.
    pub fn set_voting_power(&self, voter: Address, power: u128) -> Result<String, HarnessError> {
        self.cast_send(
            DEPLOYER_PRIVATE_KEY_HEX,
            self.governance(),
            "setVotingPower(address,uint256)",
            &[&format!("{voter:#x}"), &power.to_string()],
        )
    }

    /// Fund `recipient` with `amount` USDC by signing a real
    /// `transfer(address,uint256)` from [`HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX`].
    ///
    /// This is the canonical USDC faucet for the smoke-test devnet. The
    /// holder EOA receives its USDC balance at genesis (the alloc builder
    /// patches `balances[holder] += grant` and `totalSupply += grant`), so
    /// `fund_usdc` is a vanilla ERC-20 transfer signed by the holder's key
    /// — no `cast send` from the deployer, no Anvil cheats, no whale
    /// impersonation. The signature is recoverable, the Transfer event
    /// fires, and behaviour matches prod.
    pub fn fund_usdc(&self, recipient: Address, amount: u128) -> Result<String, HarnessError> {
        self.cast_send(
            HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX,
            self.usdc(),
            "transfer(address,uint256)",
            &[&format!("{recipient:#x}"), &amount.to_string()],
        )
    }

    /// Seed `count` deterministic simulated-depositor EOAs so that **all four**
    /// PRD §11 vaults report non-zero `totalAssets` and real depositor share
    /// balances the moment the dapp first loads (issues #465, #532, #563, #621).
    ///
    /// Each depositor performs two real on-chain deposits, both denominated in
    /// `per_user_usdc`:
    ///
    /// 1. **Router deposit** — `PortfolioRouter.deposit(uint256,uint256[])`
    ///    splits `per_user_usdc` across all four router-eligible vaults by the
    ///    on-chain 8500/500/500/500 bps weight vector (issue #621): the primary
    ///    `RobotMoneyVault` (§11.1, real Aave/Compound/Morpho adapters), `RwaVault` (§11.4,
    ///    rmRWA, ADR-0006 §1 amended 2026-06-05), `ProtocolAssetVault` (§11.2,
    ///    rmPROTO) and `AgentTokenVault` (§11.3, rmAGENT, issues #559/#560). The
    ///    basket and RWA legs execute real USDC → token swaps through the demo
    ///    swap routers (`DemoV3SwapRouter`, `DemoV4SwapRouter`, `DemoAerodromeRouter`),
    ///    so every router-eligible vault ends boot with non-zero TVL. An empty
    ///    `minSharesPerLeg` array skips per-leg slippage protection — acceptable
    ///    for the seed against the demo stub pools.
    ///
    /// 2. **Direct RWA deposit** — `RwaVault.deposit(uint256,address)` deposits
    ///    `per_user_usdc` straight into the §11.4 RWA vault as a belt-and-suspenders
    ///    guarantee. While the router now funds rmRWA at 500 bps (issue #621), the
    ///    direct deposit ensures the depositor holds rmRWA receipt shares and
    ///    exercises the full Aerodrome/Chronicle deposit path end to end.
    ///
    /// Each depositor is therefore funded with `2 * per_user_usdc` USDC (one
    /// budget per deposit path) plus a small ETH gas grant.
    ///
    /// Returns the list of `(depositor_address, router_deposit_tx_hash)` pairs,
    /// ordered by depositor index. The keys are derived from
    /// `keccak256("rm-demo-depositor-vN")` so the seed is reproducible across
    /// runs and keys never collide with the harness EOAs (deployer / pauser /
    /// agent / share-receiver / USDC holder).
    ///
    /// ## Why this is parallelised
    ///
    /// The smoke-test devnet is a real Geth+Lighthouse PoS chain with a 12s
    /// slot time, and `cast send` blocks until its transaction is mined (one
    /// confirmation). A naive serial seed of N depositors does `6N`
    /// confirmation-bound transactions back to back — at four depositors that
    /// is 24 block-waits (~5–8 min) on top of the multi-minute chain+dapp boot,
    /// which pushed the `smoke-test-devnet` CI job (30-min budget) into a
    /// timeout (issue #563). The work splits into two phases so the wall-clock
    /// collapses to ~8 block-waits without changing the on-chain outcome:
    ///
    /// - **Funding phase** — ETH grants are signed by the shared DEPLOYER key
    ///   and USDC grants by the shared HARNESS_USDC_HOLDER key. Same-key sends
    ///   must stay serial (sequential nonces), but the two streams use
    ///   *different* keys, so they run concurrently in two scoped threads.
    /// - **Deposit phase** — each depositor's four deposit transactions
    ///   (approve+deposit router, approve+deposit RWA) are signed by that
    ///   depositor's unique key, so the per-depositor sequences are fully
    ///   independent and run concurrently, one scoped thread per depositor.
    ///   Transactions from distinct EOAs are mined together in the same block.
    pub fn seed_demo_depositors(
        &self,
        count: u32,
        per_user_usdc: u128,
    ) -> Result<Vec<(Address, String)>, HarnessError> {
        // Each depositor runs the router split plus a direct belt-and-suspenders
        // deposit into ALL FOUR vaults (primary §11.1, Protocol §11.2, Agent
        // §11.3, RWA §11.4). The router's weighted split can intermittently
        // under-fund a vault on a busy chain — leaving it at zero TVL or leaving
        // a depositor with no shares (issue #882) — so the direct deposits
        // guarantee every vault has non-zero TVL and every depositor holds shares
        // in each. Fund five per-path budgets (router + four direct deposits).
        let total_usdc = per_user_usdc.saturating_mul(5);
        let keys: Vec<(String, Address)> = (0..count).map(demo_depositor_key).collect();

        // ── Funding phase ────────────────────────────────────────────────
        // Fund all depositors with gas ETH (DEPLOYER key) and USDC (harness
        // holder key) before any deposits. The two faucet streams use distinct
        // shared keys, so they run concurrently; within each stream the sends
        // stay serial to keep that key's nonce sequence well-ordered.
        thread::scope(|s| -> Result<(), HarnessError> {
            let eth = s.spawn(|| -> Result<(), HarnessError> {
                for (_pk, depositor) in &keys {
                    let depositor_hex = format!("{depositor:#x}");
                    // 0.05 ETH comfortably covers two approves + two deposits
                    // (the basket-vault swap legs are the costliest step).
                    fund_eth_from_deployer(
                        &self.nonce_tracker,
                        &depositor_hex,
                        "50000000000000000",
                    )?;
                }
                Ok(())
            });
            let usdc = s.spawn(|| -> Result<(), HarnessError> {
                for (_pk, depositor) in &keys {
                    self.fund_usdc(*depositor, total_usdc)?;
                }
                Ok(())
            });
            eth.join()
                .map_err(|_| HarnessError::other("eth-funding thread panicked"))??;
            usdc.join()
                .map_err(|_| HarnessError::other("usdc-funding thread panicked"))??;
            Ok(())
        })?;

        // ── Deposit phase ────────────────────────────────────────────────
        // Run each depositor's deposit sequence on its own scoped thread. The
        // sequences are signed by distinct keys, so they neither share nonces
        // nor depend on one another; the chain mines them in parallel.
        // `primary_hex` is interpolated into the zero-shares error message
        // below. Approve targets (router/vaults) are now passed to
        // `approve_and_confirm` as typed `Address`es, so their hex forms are no
        // longer materialised here.
        let primary_hex = format!("{:#x}", self.vault());
        let results = thread::scope(|s| -> Result<Vec<(Address, String)>, HarnessError> {
            let handles: Vec<_> = keys
                .iter()
                .map(|(pk_hex, depositor)| {
                    let primary_hex = &primary_hex;
                    s.spawn(move || -> Result<(Address, String), HarnessError> {
                        let depositor_hex = format!("{depositor:#x}");

                        // Approve the router for the router-leg budget, then
                        // deposit. The router splits the amount across the three
                        // router-eligible vaults by the on-chain weight vector
                        // and mints shares to the depositor (msg.sender). Empty
                        // minSharesPerLeg skips slippage protection (demo stubs).
                        self.approve_and_confirm(
                            pk_hex,
                            self.usdc(),
                            *depositor,
                            self.router(),
                            per_user_usdc,
                        )?;
                        let router_tx = self.cast_send(
                            pk_hex,
                            self.router(),
                            "deposit(uint256,uint256[])",
                            &[&per_user_usdc.to_string(), "[]"],
                        )?;

                        // Direct deposit into the primary vault too. The router
                        // leg already routes ~8500 bps here, but the router
                        // deposit can intermittently leave a depositor with no
                        // primary-vault shares on a busy chain (issue #882); a
                        // direct deposit guarantees every depositor holds primary
                        // shares. NOTE: the primary RobotMoneyVault on the Geth
                        // devnet routes deposits through the real Aave/Compound/
                        // Morpho strategy adapters (NOT a 1:1 no-yield adapter,
                        // an earlier comment claimed that in error). That real-
                        // adapter path can intermittently revert under load, which
                        // — when swallowed — left the depositor with zero primary
                        // shares (issue #904). We now assert the receipt status in
                        // cast_send and re-read the post-deposit share balance so a
                        // mis-minted deposit surfaces the true on-chain failure
                        // here rather than as a downstream per-depositor flake.
                        self.approve_and_confirm(
                            pk_hex,
                            self.usdc(),
                            *depositor,
                            self.vault(),
                            per_user_usdc,
                        )?;
                        self.cast_send(
                            pk_hex,
                            self.vault(),
                            "deposit(uint256,address)",
                            &[&per_user_usdc.to_string(), &depositor_hex],
                        )?;
                        // ERC-4626 `deposit` mints shares to `depositor`; the
                        // primary-share guarantee this seeding exists to provide
                        // only holds if those shares actually landed. Verify a
                        // non-zero primary-vault `balanceOf(depositor)` and fail
                        // loudly (with the on-chain context) otherwise.
                        let primary_shares = self.erc20_balance_of(self.vault(), *depositor)?;
                        if primary_shares == 0 {
                            return Err(HarnessError::other(format!(
                                "primary-vault deposit minted no shares to depositor {depositor_hex}: \
                                 balanceOf(vault={primary_hex}) == 0 after deposit(amount={per_user_usdc}). \
                                 The primary vault's real-adapter routing likely reverted on the Geth devnet."
                            )));
                        }

                        // Approve and deposit directly into the §11.4 RWA vault,
                        // which is Active but not router-eligible. Shares go to
                        // the depositor.
                        self.approve_and_confirm(
                            pk_hex,
                            self.usdc(),
                            *depositor,
                            self.rwa_vault(),
                            per_user_usdc,
                        )?;
                        self.cast_send(
                            pk_hex,
                            self.rwa_vault(),
                            "deposit(uint256,address)",
                            &[&per_user_usdc.to_string(), &depositor_hex],
                        )?;

                        // Direct belt-and-suspenders deposits into the two basket
                        // vaults (Protocol §11.2, Agent §11.3). The router's
                        // weighted split can under-fund a basket leg on a busy
                        // chain (issue #882); a direct deposit guarantees non-zero
                        // TVL. Each swaps USDC -> basket via the demo stub routers.
                        self.approve_and_confirm(
                            pk_hex,
                            self.usdc(),
                            *depositor,
                            self.demo_protocol_vault(),
                            per_user_usdc,
                        )?;
                        self.cast_send(
                            pk_hex,
                            self.demo_protocol_vault(),
                            "deposit(uint256,address)",
                            &[&per_user_usdc.to_string(), &depositor_hex],
                        )?;
                        self.approve_and_confirm(
                            pk_hex,
                            self.usdc(),
                            *depositor,
                            self.demo_agent_vault(),
                            per_user_usdc,
                        )?;
                        self.cast_send(
                            pk_hex,
                            self.demo_agent_vault(),
                            "deposit(uint256,address)",
                            &[&per_user_usdc.to_string(), &depositor_hex],
                        )?;

                        Ok((*depositor, router_tx))
                    })
                })
                .collect();

            let mut out = Vec::with_capacity(handles.len());
            for h in handles {
                out.push(
                    h.join()
                        .map_err(|_| HarnessError::other("depositor-seed thread panicked"))??,
                );
            }
            Ok(out)
        })?;

        Ok(results)
    }

    /// Fund `recipient` with `amount` RM tokens by signing a real
    /// `transfer(address,uint256)` from [`HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX`].
    ///
    /// The harness EOA holds the entire RmToken initial supply (minted at
    /// deploy time by DeployRmToken.s.sol). The transfer is a vanilla ERC-20
    /// call — no Anvil cheats, no impersonation. The signature is recoverable
    /// and the `Transfer` event fires, matching production semantics (issue #365).
    pub fn fund_rm_token(&self, recipient: Address, amount: u128) -> Result<String, HarnessError> {
        self.cast_send(
            HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX,
            self.rm_token(),
            "transfer(address,uint256)",
            &[&format!("{recipient:#x}"), &amount.to_string()],
        )
    }

    /// keccak256(abi.encodePacked(RECEIPT_ID_DOMAIN, sessionId, "\n",
    /// subjectId)), mirroring `ConsensusRebalanceReceipt.computeReceiptId`
    /// exactly (contracts/gateway/ConsensusRebalanceReceipt.sol). `abi.encodePacked`
    /// on `string` params is a plain byte concatenation, so this is
    /// reproducible off-chain without an RPC round trip.
    fn compute_receipt_id(session_id: &str, subject_id: &str) -> [u8; 32] {
        const RECEIPT_ID_DOMAIN: &str = "robotmoney:consensus-receipt-id:v1\n";
        let mut buf =
            Vec::with_capacity(RECEIPT_ID_DOMAIN.len() + session_id.len() + 1 + subject_id.len());
        buf.extend_from_slice(RECEIPT_ID_DOMAIN.as_bytes());
        buf.extend_from_slice(session_id.as_bytes());
        buf.push(b'\n');
        buf.extend_from_slice(subject_id.as_bytes());
        keccak256(&buf).0
    }

    /// Seed two fixture consensus receipts (issue #1294) so the dapp e2e
    /// spec can assert every rendered state distinctly:
    ///
    /// - `receipt-a.json`: submitted with its OWN correct digest and
    ///   released — renders "Verified" and "Released", and its weights
    ///   match the live 8 500/500/500/500 bps router split from
    ///   `DeployDemoExtraVaults.s.sol::_applyFourVaultWeights` — renders
    ///   "Applied".
    /// - `receipt-b.json`: submitted with a deliberately WRONG digest and
    ///   never released — renders "Unverified" and "Recorded, not
    ///   released", and its weights deliberately do not match the live
    ///   split — renders "Not applied".
    ///
    /// Both payload files are served by the `receipt-fixtures` compose
    /// service (issue #1294) at [`RECEIPT_FIXTURES_PORT`] under the
    /// hostname `receipt-fixtures` — reachable from the indexer container
    /// via Docker's embedded DNS on the compose `default` network, and from
    /// the Playwright browser via a `--host-resolver-rules` mapping of that
    /// same hostname straight to 127.0.0.1 (see
    /// `clients/dapp/playwright.config.ts`), which resolves to the SAME
    /// published host port. One literal on-chain `payload_uri` string is
    /// therefore fetchable, byte-identically, from both vantage points.
    fn seed_consensus_receipts(&self) -> Result<(), HarnessError> {
        let fixtures_dir = self
            .repo_root
            .join("testing/ethereum-testnet/config/consensus-receipt-fixtures");
        let receipt_a_bytes = std::fs::read(fixtures_dir.join("receipt-a.json"))
            .map_err(|e| HarnessError::other(format!("read receipt-a.json: {e}")))?;
        // receipt-b.json is served byte-for-byte too, but its ON-CHAIN digest
        // is deliberately wrong (below) — its own bytes are never hashed.

        // Allowlist the agent EOA as a committee member: gateway.committeeRegister
        // requires ADMIN_ROLE on the gateway (the deployer), and forwards to
        // InvestmentCommitteePolicy.registerAgent, which the gateway may call
        // because DeployInvestmentCommitteePolicy.s.sol already granted it the
        // IC contract's ADMIN_ROLE.
        let agent_hex = format!("{:#x}", self.agent());
        self.cast_send(
            DEPLOYER_PRIVATE_KEY_HEX,
            self.gateway(),
            "committeeRegister(address,string)",
            &[&agent_hex, "smoke-test-receipt-agent"],
        )?;

        let agent_pk_hex = format!("0x{}", hex::encode(AGENT_PRIVATE_KEY));
        let uri_a = format!("http://receipt-fixtures:{RECEIPT_FIXTURES_PORT}/receipt-a.json");
        let uri_b = format!("http://receipt-fixtures:{RECEIPT_FIXTURES_PORT}/receipt-b.json");

        let session_a = "smoke-test-fusion-1294-a";
        let subject_a = "treasury-allocation";
        let receipt_id_a = Self::compute_receipt_id(session_a, subject_a);
        let digest_a = keccak256(&receipt_a_bytes).0;
        self.cast_send(
            &agent_pk_hex,
            self.gateway(),
            "consensusRecordReceipt(bytes32,bytes32,string)",
            &[
                &format!("0x{}", hex::encode(receipt_id_a)),
                &format!("0x{}", hex::encode(digest_a)),
                &uri_a,
            ],
        )?;
        // Release receipt A — an admin signalling-only act (no funds move, no
        // router weight changes). RECEIPT_ADMIN_ADDRESS defaults to the
        // deployer for this devnet ceremony.
        self.cast_send(
            DEPLOYER_PRIVATE_KEY_HEX,
            self.consensus_receipt(),
            "releaseReceipt(bytes32)",
            &[&format!("0x{}", hex::encode(receipt_id_a))],
        )?;

        let session_b = "smoke-test-fusion-1294-b";
        let subject_b = "treasury-allocation";
        let receipt_id_b = Self::compute_receipt_id(session_b, subject_b);
        // Deliberately WRONG digest — receipt-b.json is valid JSON the
        // browser can still render, but the indexer's re-fetched keccak256
        // will never match this value, so `verified` stays false.
        let digest_b = keccak256(b"smoke-test-wrong-digest-marker-for-1294").0;
        self.cast_send(
            &agent_pk_hex,
            self.gateway(),
            "consensusRecordReceipt(bytes32,bytes32,string)",
            &[
                &format!("0x{}", hex::encode(receipt_id_b)),
                &format!("0x{}", hex::encode(digest_b)),
                &uri_b,
            ],
        )?;
        // Receipt B is deliberately left unreleased.

        Ok(())
    }

    /// Fund `recipient` with `value_wei` native ETH by signing a plain value
    /// transfer from [`HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX`] (issue #466).
    ///
    /// Mirrors the dapp's `dripEth` faucet client: the holder EOA receives
    /// 1000 ETH at genesis via `genesis_alloc::DEFAULT_HARNESS_ETH_WEI`, so
    /// a vanilla value transfer signed by the holder's key matches the
    /// production faucet code path exactly — no Anvil cheats, no deployer
    /// impersonation. Returns the transaction hash.
    pub fn fund_eth_from_harness(
        &self,
        recipient: Address,
        value_wei: &str,
    ) -> Result<String, HarnessError> {
        let to_hex = format!("{recipient:#x}");
        logging::debug(
            "rpc",
            format!("eth_sendRawTransaction via cast send value={value_wei} -> {to_hex}"),
        );
        let v = pinned_cast_send(
            &self.nonce_tracker,
            "fund_eth_from_harness",
            HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX,
            &["--value", value_wei, &to_hex],
        )?;
        Ok(v.get("transactionHash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        logging::info("chain-compose", "tearing down chain compose stack");
        for child in &mut self.compose_log_followers {
            child.terminate();
        }
        let _ = Command::new("docker")
            .args([
                "compose",
                "-f",
                "docker-compose.yaml",
                "down",
                "-v",
                "--remove-orphans",
            ])
            .current_dir(&self.compose_dir)
            .status();
        logging::info("chain-compose", "chain compose teardown complete");
    }
}

// -- Public helpers ---------------------------------------------------

/// Returns `true` iff `docker`, `forge`, and `cast` are all on PATH.
pub fn prerequisites_available() -> bool {
    which::which("docker").is_ok() && which::which("forge").is_ok() && which::which("cast").is_ok()
}

// -- Internal helpers -------------------------------------------------

/// Issue #255 / #607: copy the pre-built genesis alloc overlay JSON into
/// `out_dir` and return its absolute path. Returns `Ok(None)` when the
/// committed `genesis-alloc.json` fixture is absent — the caller falls back
/// to the legacy clean-room genesis path.
///
/// The pre-built JSON is produced by `smoke-test-genesis-ingester` from
/// Committed at `testing/fixtures/fork-state/genesis-alloc.json`. Regenerate
/// whenever the fork block is bumped:
///
///     cargo run --bin smoke-test-genesis-ingester --release -- \
///         --manifest testing/ethereum-testnet/config/fork-block.json \
///         --snapshot testing/fixtures/fork-state/<BLOCK>.anvil-state \
///         --output   testing/fixtures/fork-state/genesis-alloc.json
fn render_genesis_alloc_overlay(
    repo_root: &Path,
    out_dir: &Path,
) -> Result<Option<PathBuf>, HarnessError> {
    let src = repo_root.join("testing/fixtures/fork-state/genesis-alloc.json");
    if !src.exists() {
        return Ok(None);
    }

    let out_path = out_dir.join("genesis-alloc.json");
    std::fs::copy(&src, &out_path)?;
    // docker requires an absolute path for bind-mount source; the tempdir
    // path already is absolute, but be defensive.
    let absolute = std::fs::canonicalize(&out_path)?;
    eprintln!(
        "smoke-test: using pre-built genesis alloc overlay -> {}",
        absolute.display()
    );
    Ok(Some(absolute))
}

/// Parse a `0x`-prefixed (or bare) 32-byte hex private key into raw bytes.
/// Used to recover the sender address for gas estimation in [`Fixture::cast_send`].
fn privkey_hex_to_bytes(pk_hex: &str) -> Result<[u8; 32], HarnessError> {
    let s = pk_hex.strip_prefix("0x").unwrap_or(pk_hex);
    let bytes =
        hex::decode(s).map_err(|e| HarnessError::other(format!("invalid private key hex: {e}")))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| HarnessError::other("private key must be exactly 32 bytes"))
}

fn derive_address(privkey: &[u8; 32]) -> Address {
    use k256::ecdsa::SigningKey;
    let sk = SigningKey::from_bytes(privkey.into()).expect("static privkey is valid");
    let vk = sk.verifying_key();
    let pubkey = vk.to_encoded_point(false);
    let hash = keccak256(&pubkey.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

/// Derive a deterministic harness EOA from an arbitrary domain-separated
/// seed. Shared by [`demo_depositor_key`] and [`dapp_faucet_key`] so neither
/// needs a hand-picked, hand-verified hex literal: the key is reproducible
/// across runs and, by construction (domain-separated seed input), cannot
/// collide with any other harness key. Returns `(0x-prefixed hex private
/// key, derived address)`.
fn derive_deterministic_key(seed: &[u8]) -> (String, Address) {
    let mut pk = keccak256(seed).0;
    // secp256k1 keys must be in (0, n-1); the chance of `keccak256 >= n` is
    // ~2^-127. If it ever happens (or the all-zero edge case), perturb by
    // hashing again. This loop is bounded; in practice it executes once.
    loop {
        if pk != [0u8; 32] {
            // Try to construct a SigningKey to confirm validity.
            use k256::ecdsa::SigningKey;
            if SigningKey::from_bytes((&pk).into()).is_ok() {
                let addr = derive_address(&pk);
                return (format!("0x{}", hex::encode(pk)), addr);
            }
        }
        pk = keccak256(pk).0;
    }
}

/// Derive a deterministic simulated-depositor key for the demo seeding flow
/// (issues #465, #503). Seed is `keccak256("rm-demo-depositor-v1\0" || index)`
/// so the resulting key is reproducible across runs and cannot collide with the
/// hand-picked harness keys (deployer/pauser/agent/share-receiver/USDC holder).
/// Returns `(0x-prefixed hex private key, derived address)`.
///
/// Public so the `demo-seed-depositors` standalone binary can reuse the same
/// key derivation without depending on the full [`Fixture`] struct (issue #503).
pub fn demo_depositor_key(index: u32) -> (String, Address) {
    let mut seed = Vec::with_capacity(64);
    seed.extend_from_slice(b"rm-demo-depositor-v1\0");
    seed.extend_from_slice(&index.to_be_bytes());
    derive_deterministic_key(&seed)
}

/// Derive the dedicated dapp-faucet EOA (issue #1241).
///
/// `HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX` used to be both the key
/// [`Fixture::fund_usdc`] signs seeding transactions with AND the key baked
/// into the dapp bundle as `VITE_FAUCET_HARNESS_PRIVATE_KEY`. The dapp is
/// live and health-checked *before* `seed_demo_depositors` runs, so a
/// browser-triggered faucet drip during seeding was a genuinely concurrent
/// sender sharing the harness's own nonce sequence for that key — a hazard
/// no amount of nonce-pinning inside `cast_send` can fix, because the
/// browser's `cast`-equivalent (viem) never goes through this harness at
/// all. Giving the faucet its own EOA, funded once at boot (see
/// `DappStack::boot`), removes the shared signer entirely.
///
/// Seed is `keccak256("rm-dapp-faucet-v1\0")`, following the same
/// deterministic derivation as [`demo_depositor_key`] so this key also needs
/// no hand-verified hex literal.
pub fn dapp_faucet_key() -> (String, Address) {
    derive_deterministic_key(b"rm-dapp-faucet-v1\0")
}

fn parse_addr(s: &str) -> Address {
    s.parse::<Address>().unwrap_or(Address::ZERO)
}

/// Sub-second readiness poll cadence for HTTP/RPC probes: a quickly-booting
/// service is detected within a quarter second rather than waiting out a
/// multi-second tick. The per-request timeout (5s) still bounds a hung probe,
/// so tightening the cadence costs nothing but a few extra cheap calls.
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Block-height poll cadence. Slower than [`READINESS_POLL_INTERVAL`] because a
/// new block only arrives once per slot, so polling faster cannot surface one
/// sooner.
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[allow(dead_code)]
fn wait_for_rpc(url: &str, timeout: Duration) -> Result<(), HarnessError> {
    wait_for_rpc_with_probe(url, timeout, None)
}

fn wait_for_rpc_with_probe(
    url: &str,
    timeout: Duration,
    mut health_probe: Option<&mut dyn FnMut() -> Result<(), HarnessError>>,
) -> Result<(), HarnessError> {
    logging::debug("rpc", format!("polling {url} for chain RPC health"));
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| HarnessError::other(format!("reqwest builder: {e}")))?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_chainId",
        "params": []
    });
    let started = std::time::Instant::now();
    let deadline = started + timeout;
    #[allow(unused_assignments)]
    let mut last_error: Option<String> = None;
    let mut unreachable_since: Option<std::time::Instant> = None;
    while std::time::Instant::now() < deadline {
        if let Some(probe) = health_probe.as_deref_mut() {
            probe()?;
        }
        match client.post(url).json(&body).send() {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(json) = resp.json::<serde_json::Value>() {
                    if json.get("result").is_some() {
                        if let Some(since) = unreachable_since.take() {
                            logging::info(
                                "rpc",
                                format!(
                                    "RPC recovered after {}s: {url}",
                                    since.elapsed().as_secs()
                                ),
                            );
                        }
                        logging::info(
                            "rpc",
                            format!(
                                "{url} ready in {}ms (chainId)",
                                started.elapsed().as_millis()
                            ),
                        );
                        return Ok(());
                    }
                    last_error = Some("missing result field".to_string());
                } else {
                    last_error = Some("invalid JSON-RPC response".to_string());
                }
            }
            Ok(resp) => {
                last_error = Some(format!("HTTP {}", resp.status()));
                if unreachable_since.is_none() {
                    unreachable_since = Some(std::time::Instant::now());
                    logging::warn(
                        "rpc",
                        format!(
                            "RPC unreachable at {url}: {}",
                            last_error.as_deref().unwrap_or("unknown error")
                        ),
                    );
                }
            }
            Err(err) => {
                last_error = Some(err.to_string());
                if unreachable_since.is_none() {
                    unreachable_since = Some(std::time::Instant::now());
                    logging::warn(
                        "rpc",
                        format!(
                            "RPC unreachable at {url}: {}",
                            last_error.as_deref().unwrap_or("unknown error")
                        ),
                    );
                }
            }
        }
        if let Some(since) = unreachable_since {
            if since.elapsed() >= Duration::from_secs(30) {
                logging::warn(
                    "rpc",
                    format!(
                        "RPC still unreachable at {url} after {}s: {}",
                        since.elapsed().as_secs(),
                        last_error.as_deref().unwrap_or("unknown error")
                    ),
                );
            }
        }
        std::thread::sleep(READINESS_POLL_INTERVAL);
    }
    Err(HarnessError::RpcTimeout {
        url: url.to_string(),
        timeout,
    })
}

#[allow(dead_code)]
fn wait_for_block_height(url: &str, target: u64, timeout: Duration) -> Result<(), HarnessError> {
    wait_for_block_height_with_probe(url, target, timeout, None)
}

fn wait_for_block_height_with_probe(
    url: &str,
    target: u64,
    timeout: Duration,
    mut health_probe: Option<&mut dyn FnMut() -> Result<(), HarnessError>>,
) -> Result<(), HarnessError> {
    logging::debug(
        "rpc",
        format!("polling {url} for block height {target} readiness"),
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| HarnessError::other(format!("reqwest builder: {e}")))?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_blockNumber",
        "params": []
    });
    let deadline = std::time::Instant::now() + timeout;
    let mut last_error: Option<String> = None;
    let mut last_block: Option<u64> = None;
    let mut last_progress = std::time::Instant::now();
    let mut first_success_logged = false;
    let mut stall_warned = false;
    let mut unreachable_since: Option<std::time::Instant> = None;
    while std::time::Instant::now() < deadline {
        if let Some(probe) = health_probe.as_deref_mut() {
            probe()?;
        }
        match client.post(url).json(&body).send() {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(json) = resp.json::<serde_json::Value>() {
                    if let Some(block_hex) = json.get("result").and_then(|v| v.as_str()) {
                        if let Ok(block) =
                            u64::from_str_radix(block_hex.trim_start_matches("0x"), 16)
                        {
                            let now = chrono::Utc::now()
                                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                            if !first_success_logged {
                                logging::info(
                                    "rpc",
                                    format!(
                                        "first eth_blockNumber response at {now}: block={block} url={url}"
                                    ),
                                );
                                first_success_logged = true;
                            }
                            if let Some(since) = unreachable_since.take() {
                                logging::info(
                                    "rpc",
                                    format!(
                                        "RPC recovered after {}s: block={block} url={url}",
                                        since.elapsed().as_secs()
                                    ),
                                );
                            }
                            if last_block.is_none_or(|prev| block > prev) {
                                last_block = Some(block);
                                last_progress = std::time::Instant::now();
                                stall_warned = false;
                            } else if !stall_warned
                                && last_block.is_some()
                                && last_progress.elapsed() >= Duration::from_secs(30)
                            {
                                logging::warn(
                                    "rpc",
                                    format!(
                                        "block production stalled at block={} for {}s on {url}",
                                        last_block.unwrap_or(block),
                                        last_progress.elapsed().as_secs()
                                    ),
                                );
                                stall_warned = true;
                            }
                            if block >= target {
                                logging::info(
                                    "rpc",
                                    format!(
                                        "block target reached: block={block} target={target} url={url}"
                                    ),
                                );
                                return Ok(());
                            }
                        } else {
                            last_error = Some(format!("invalid block hex {block_hex}"));
                        }
                    } else {
                        last_error = Some("missing result field".to_string());
                    }
                } else {
                    last_error = Some("invalid JSON-RPC response".to_string());
                }
            }
            Ok(resp) => {
                last_error = Some(format!("HTTP {}", resp.status()));
                if unreachable_since.is_none() {
                    unreachable_since = Some(std::time::Instant::now());
                    logging::warn(
                        "rpc",
                        format!(
                            "RPC unreachable at {url}: {}",
                            last_error.as_deref().unwrap_or("unknown error")
                        ),
                    );
                }
            }
            Err(err) => {
                last_error = Some(err.to_string());
                if unreachable_since.is_none() {
                    unreachable_since = Some(std::time::Instant::now());
                    logging::warn(
                        "rpc",
                        format!(
                            "RPC unreachable at {url}: {}",
                            last_error.as_deref().unwrap_or("unknown error")
                        ),
                    );
                }
            }
        }
        if let Some(since) = unreachable_since {
            if since.elapsed() >= Duration::from_secs(30) {
                logging::warn(
                    "rpc",
                    format!(
                        "RPC still unreachable at {url} after {}s: {}",
                        since.elapsed().as_secs(),
                        last_error.as_deref().unwrap_or("unknown error")
                    ),
                );
            }
        }
        std::thread::sleep(BLOCK_POLL_INTERVAL);
    }
    Err(HarnessError::RpcTimeout {
        url: url.to_string(),
        timeout,
    })
}

/// Environment variables the compose files read to stamp run-identity labels
/// onto every container (see docker-compose.yaml / docker-compose.dapp.yaml).
const RUN_ID_ENV: &str = "SMOKE_RUN_ID";
const RUN_CREATED_ENV: &str = "SMOKE_RUN_CREATED";

/// Docker label key carrying the run-id of the boot that created a container.
/// A boot reaps containers in our compose projects whose run-id differs from
/// its own (i.e. stranded by a previous, now-dead run).
const RUN_ID_LABEL: &str = "com.robotmoney.testnet.run-id";

/// Compose projects whose containers belong to a devnet boot. The reaper scans
/// only these so it never touches unrelated containers on the host.
const TESTNET_COMPOSE_PROJECTS: [&str; 2] = ["ethereum-testnet", "robotmoney-dapp"];

/// Mint (once per process) a unique run-id and creation timestamp and export
/// them so child `docker compose` invocations stamp them as container labels.
/// Idempotent: a run-id already set earlier in the same process is reused, so
/// the chain stack and the dapp overlay share one identity and are reaped
/// together. Mirrors the existing SMOKE_GENESIS_ALLOC_FILE env-passing pattern.
fn ensure_run_identity() -> (String, String) {
    if let (Ok(id), Ok(created)) = (std::env::var(RUN_ID_ENV), std::env::var(RUN_CREATED_ENV)) {
        if !id.is_empty() {
            return (id, created);
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let created = now.as_secs().to_string();
    // Sub-second precision plus the pid keeps two boots from the same process
    // (or two processes within the same second) distinct.
    let run_id = format!("{:016x}-{:x}", now.as_nanos() as u64, std::process::id());
    std::env::set_var(RUN_ID_ENV, &run_id);
    std::env::set_var(RUN_CREATED_ENV, &created);
    (run_id, created)
}

/// Parse `docker ps` label output (lines of `<id>\t<run-id>\t<name>`) and return
/// the `(id, name)` pairs whose run-id differs from `current_run_id` — i.e. the
/// containers stranded by a previous run. Pure (no docker), so the
/// stale-vs-current decision is unit-tested without a live daemon.
fn stale_containers_from_listing(listing: &str, current_run_id: &str) -> Vec<(String, String)> {
    let mut stale = Vec::new();
    for line in listing.lines() {
        let mut cols = line.split('\t');
        let id = cols.next().unwrap_or("").trim();
        let run_id = cols.next().unwrap_or("").trim();
        let name = cols.next().unwrap_or("").trim();
        if id.is_empty() || run_id == current_run_id {
            continue;
        }
        stale.push((id.to_string(), name.to_string()));
    }
    stale
}

/// Force-remove devnet containers stranded by a previous run. Scans only our
/// compose projects (via Docker's built-in `com.docker.compose.project` label)
/// and removes any container whose run-id label differs from `current_run_id`.
/// Containers from the current run are protected by the run-id match; legacy
/// containers booted before run-id labels existed carry an empty run-id and so
/// are treated as stale and reaped. Best-effort: docker errors are logged, not
/// propagated, so a transient `docker` hiccup never blocks a boot.
fn reap_stale_testnet_containers(current_run_id: &str) {
    let mut stale: Vec<(String, String)> = Vec::new();
    for project in TESTNET_COMPOSE_PROJECTS {
        let listing = Command::new("docker")
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("label=com.docker.compose.project={project}"),
                "--format",
                &format!("{{{{.ID}}}}\t{{{{.Label \"{RUN_ID_LABEL}\"}}}}\t{{{{.Names}}}}"),
            ])
            .output();
        match listing {
            Ok(out) if out.status.success() => {
                stale.extend(stale_containers_from_listing(
                    &String::from_utf8_lossy(&out.stdout),
                    current_run_id,
                ));
            }
            Ok(out) => logging::warn(
                "smoke-test",
                format!(
                    "reaper: `docker ps` for project {project} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ),
            Err(err) => {
                logging::warn(
                    "smoke-test",
                    format!("reaper: `docker ps` not runnable: {err}"),
                );
                return;
            }
        }
    }
    if stale.is_empty() {
        return;
    }
    let names: Vec<&str> = stale.iter().map(|(_, n)| n.as_str()).collect();
    logging::info(
        "smoke-test",
        format!(
            "reaper: removing {} container(s) stranded by previous run(s): {}",
            stale.len(),
            names.join(", ")
        ),
    );
    let mut rm = Command::new("docker");
    rm.args(["rm", "-f"]);
    for (id, _) in &stale {
        rm.arg(id);
    }
    match rm.output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => logging::warn(
            "smoke-test",
            format!(
                "reaper: `docker rm -f` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ),
        Err(err) => logging::warn(
            "smoke-test",
            format!("reaper: `docker rm -f` not runnable: {err}"),
        ),
    }
}

/// Proactively tear down any stale `robotmoney-dapp` compose state before a
/// fresh `DappStack::boot`. Runs `compose down -v --remove-orphans` on
/// `docker-compose.dapp.yaml` so that containers, volumes, AND the
/// `robotmoney-dapp_default` Docker network are fully removed.
///
/// This addresses the root cause of issue #1085: `reap_stale_testnet_containers`
/// force-removes individual containers by ID but does not remove the Docker
/// network itself. A network stuck in `REMOVING` state (left by a cancelled or
/// crashed CI run) causes the subsequent `compose up --build` to fail in ~36 s
/// because Docker cannot create a new network with the same name. Calling
/// `compose down` before `compose up` removes the network atomically.
///
/// The minimum required env vars for `compose down` are the mandatory `?:`
/// substitutions in `docker-compose.dapp.yaml`; everything else defaults.
/// Best-effort: a non-zero exit means there was nothing to tear down (happy
/// path); the outcome is logged and execution continues regardless.
fn purge_stale_dapp_compose_state(
    compose_dir: &Path,
    gateway_hex: &str,
    vault_hex: &str,
    gateway_runtime_hash: &str,
) {
    logging::info(
        "smoke-test",
        "purging stale dapp compose state before boot (removes stuck networks/volumes)",
    );
    let out = Command::new("docker")
        .args([
            "compose",
            "-f",
            "docker-compose.dapp.yaml",
            "down",
            "-v",
            "--remove-orphans",
        ])
        // Satisfy the mandatory ?:-substitutions in docker-compose.dapp.yaml.
        // compose down does not bind ports, so port env vars are not required.
        .env("VITE_GATEWAY_ADDRESS", gateway_hex)
        .env("VITE_VAULT_ADDRESS", vault_hex)
        .env("VITE_GATEWAY_EXPECTED_CODE_HASH", gateway_runtime_hash)
        .env("INDEXER_GATEWAY", gateway_hex)
        .env("INDEXER_VAULT", vault_hex)
        .current_dir(compose_dir)
        .output();
    match out {
        Ok(out) if out.status.success() => {
            logging::info("smoke-test", "stale dapp compose state purged");
        }
        Ok(out) => {
            // Non-zero exit is normal when the project has no containers/networks
            // to remove; log at debug so CI noise stays low.
            logging::debug(
                "smoke-test",
                format!(
                    "dapp compose down (pre-boot purge) exited {:?}: {} {}",
                    out.status,
                    String::from_utf8_lossy(&out.stdout).trim(),
                    String::from_utf8_lossy(&out.stderr).trim(),
                ),
            );
        }
        Err(err) => {
            logging::warn(
                "smoke-test",
                format!("dapp compose down (pre-boot purge) failed to launch: {err}"),
            );
        }
    }
}

fn ensure_compose_project_idle(
    compose_dir: &Path,
    compose_files: &[String],
) -> Result<(), HarnessError> {
    let running = compose_running_container_names(compose_dir, compose_files)?;
    if running.is_empty() {
        return Ok(());
    }

    Err(HarnessError::Docker(format!(
        "ethereum-testnet compose project already running containers: {}; \
         stop the existing smoke-test instance before starting another",
        running.join(", ")
    )))
}

fn compose_running_container_names(
    compose_dir: &Path,
    compose_files: &[String],
) -> Result<Vec<String>, HarnessError> {
    let mut cmd = Command::new("docker");
    cmd.arg("compose");
    for file in compose_files {
        cmd.arg(file);
    }
    let output = cmd
        .arg("ps")
        .arg("--format")
        .arg("json")
        .current_dir(compose_dir)
        .output()
        .map_err(HarnessError::from)?;

    if !output.status.success() {
        return Err(HarnessError::Docker(format!(
            "docker compose ps failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    parse_compose_ps_stdout(&output.stdout)
}

fn parse_compose_ps_stdout(stdout: &[u8]) -> Result<Vec<String>, HarnessError> {
    let mut running = Vec::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: ComposePsEntry = serde_json::from_str(line).map_err(|e| {
            HarnessError::Docker(format!("docker compose ps parse error: {e}; line={line}"))
        })?;
        if entry.state.eq_ignore_ascii_case("running") {
            running.push(entry.name);
        }
    }
    Ok(running)
}

/// Fund the deployer EOA with USDC from HARNESS_USDC_HOLDER before the forge
/// deploy runs. Deploy.s.sol `run()` executes a mandatory seed deposit of
/// `SEED_DEPOSIT_AMOUNT` (1,000 USDC) from the broadcaster (issue #656), so
/// the deployer must hold USDC *before* deployment — `Fixture::fund_usdc` is
/// not available yet at that point in the boot sequence.
fn fund_usdc_to_deployer(
    tracker: &NonceTracker,
    amount_units: u128,
) -> Result<String, HarnessError> {
    logging::debug(
        "rpc",
        format!(
            "eth_sendRawTransaction via cast send usdc transfer {amount_units} -> {DEPLOYER_ADDRESS_HEX}"
        ),
    );
    let amount_s = amount_units.to_string();
    let v = pinned_cast_send(
        tracker,
        "fund deployer usdc",
        HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX,
        &[
            genesis_alloc::BASE_USDC_ADDR,
            "transfer(address,uint256)",
            DEPLOYER_ADDRESS_HEX,
            &amount_s,
        ],
    )?;
    Ok(v.get("transactionHash")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string())
}

fn fund_eth_from_deployer(
    tracker: &NonceTracker,
    recipient_hex: &str,
    value_wei: &str,
) -> Result<String, HarnessError> {
    logging::debug(
        "rpc",
        format!("eth_sendRawTransaction via cast send value={value_wei} -> {recipient_hex}"),
    );
    let v = pinned_cast_send(
        tracker,
        "fund eth",
        DEPLOYER_PRIVATE_KEY_HEX,
        &["--value", value_wei, recipient_hex],
    )?;
    Ok(v.get("transactionHash")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string())
}

/// Delays before the 2nd, 3rd and 4th attempt of a
/// [`SendFailureClass::SafeRetry`] send. The first attempt is immediate.
const NONCE_RETRY_DELAYS: [Duration; 4] = [
    Duration::ZERO,
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// Run one `cast send` under a harness-pinned nonce and the issue #1241
/// send-failure policy, and return its parsed `--json` receipt.
///
/// This is the funding path's half of what [`Fixture::cast_send`] does for
/// contract pokes (issue #1374). `args` is everything between the pinned
/// flags and `--json`: either `["--value", wei, recipient]` for a plain value
/// transfer or `[to, sig, args...]` for a call. Gas is left to `cast`'s own
/// estimate — unlike [`Fixture::cast_send`] these are fixed-cost transfers
/// with no same-block state growth to under-estimate (issue #897).
///
/// The three properties this buys, none of which the previous bare
/// `Command::new("cast").arg("send")` had:
///
/// 1. **No colliding nonce.** `--nonce` comes from
///    [`NonceTracker::pin_next_nonce`], which is monotonic per sender and
///    serialised across threads, so two funding sends from one account
///    cannot be handed the same nonce. This is the actual defect behind
///    `replacement transaction underpriced` in fixture bring-up.
/// 2. **Scoped retry.** Only [`SendFailureClass::SafeRetry`] (the node
///    refused the send outright, so nothing entered the chain) is retried;
///    [`SendFailureClass::Ambiguous`] is resolved by receipt lookup, never by
///    re-sending.
/// 3. **A genuine failure still fails loudly.** Anything unrecognised —
///    `insufficient funds` from an unfunded faucet, a bad RPC URL, `cast`
///    missing — classifies as [`SendFailureClass::Hard`] and propagates
///    immediately with `{label} failed:` and the full stdout/stderr, so it is
///    never retried into silence and never mistaken for the nonce race.
fn pinned_cast_send(
    tracker: &NonceTracker,
    label: &str,
    private_key_hex: &str,
    args: &[&str],
) -> Result<serde_json::Value, HarnessError> {
    let from = derive_address(&privkey_hex_to_bytes(private_key_hex)?);
    let from_hex = format!("{from:#x}");
    let nonce = tracker.pin_next_nonce(&from_hex)?;
    let nonce_s = nonce.to_string();
    run_cast_send_retry(
        NONCE_RETRY_DELAYS.len() as u32,
        |attempt| {
            if !NONCE_RETRY_DELAYS[attempt as usize].is_zero() {
                thread::sleep(NONCE_RETRY_DELAYS[attempt as usize]);
            }
            let mut cmd = Command::new("cast");
            cmd.args([
                "send",
                "--rpc-url",
                tracker.rpc_url(),
                "--private-key",
                private_key_hex,
                "--nonce",
                &nonce_s,
            ]);
            cmd.args(args);
            cmd.arg("--json");
            let out = match cmd.output() {
                Ok(out) => out,
                Err(e) => {
                    return SendAttemptOutcome::Failed(
                        SendFailureClass::Hard,
                        format!("{label} IO error: {e}"),
                    )
                }
            };
            logging::log_command_output("cast", &out);
            if out.status.success() {
                return match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    Ok(v) => SendAttemptOutcome::Mined(v),
                    Err(e) => SendAttemptOutcome::Failed(
                        SendFailureClass::Hard,
                        format!("{label} json: {e}"),
                    ),
                };
            }
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let class = classify_send_failure(&stderr);
            SendAttemptOutcome::Failed(
                class,
                format!("{label} failed: stdout={stdout} stderr={stderr}"),
            )
        },
        || tracker.find_receipt_for_nonce(&from_hex, nonce),
    )
    .inspect_err(|_| tracker.release_pin_if_unused(&from_hex, nonce))
}

fn run_forge_deploy_with_env(
    repo_root: &Path,
    rpc_url: &str,
    dep_out: &Path,
    agent_address_hex: &str,
    pauser_address_hex: &str,
    extra_env: &[(&str, &str)],
) -> Result<(), HarnessError> {
    let mut cmd = Command::new("forge");
    cmd.args(["script", "contracts/script/Deploy.s.sol:Deploy"])
        .args(["--rpc-url", rpc_url])
        .args(["--private-key", DEPLOYER_PRIVATE_KEY_HEX])
        .arg("--broadcast")
        .arg("--slow")
        .arg("-vvv")
        .env("ADMIN_ADDRESS", DEPLOYER_ADDRESS_HEX)
        .env("PAUSER_ADDRESS", pauser_address_hex)
        .env("AGENT_ADDRESS", agent_address_hex)
        .env("SHARE_RECEIVER_ADDRESS", SHARE_RECEIVER_ADDRESS_HEX)
        // Bind the gateway to the canonical Base USDC seeded into genesis
        // (issue #255). Tells Deploy.s.sol to skip MockUSDC + the
        // permissioned post-deploy mint. The harness funds the agent via
        // `Fixture::fund_usdc` (real ERC-20 transfer from
        // HARNESS_USDC_HOLDER) instead.
        .env("USDC_ADDRESS", genesis_alloc::BASE_USDC_ADDR)
        .env("DEPLOYMENT_OUT", dep_out)
        .current_dir(repo_root);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    logging::log_command_output("forge", &out);
    if !out.status.success() {
        return Err(HarnessError::DeployFailed(format!(
            "forge script exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Interpret a transaction receipt `status` word from `cast send --json`.
///
/// Foundry emits the status as a hex quantity: `"0x1"` for a successful tx and
/// `"0x0"` for a reverted one. A reverted tx (status `0x0`) still mines, so
/// `cast send` exits 0 and the calling harness used to swallow the revert
/// (issue #904). Returns `true` only when the word denotes a non-zero (success)
/// status, treating any non-zero hex word as success to stay robust to future
/// formatting; an empty or all-zero word is a failure.
fn receipt_status_succeeded(status: &str) -> bool {
    let stripped = status.trim_start_matches("0x").trim_start_matches("0X");
    !stripped.is_empty() && stripped.chars().any(|c| c != '0')
}

/// Parse a `cast rpc` result that is a single quoted hex-quantity string
/// (e.g. `"0x1a"`), as returned by `eth_getTransactionCount` /
/// `eth_blockNumber`. `what` names the call in the error message.
fn parse_hex_rpc_result(stdout: &[u8], what: &str) -> Result<u64, HarnessError> {
    let raw = String::from_utf8_lossy(stdout);
    let hex = raw.trim().trim_matches('"').trim_start_matches("0x");
    u64::from_str_radix(hex, 16)
        .map_err(|e| HarnessError::other(format!("{what} returned non-hex result {raw:?}: {e}")))
}

/// Classification of a failed `cast send` attempt (issue #1241), used by
/// [`run_cast_send_retry`] to decide whether re-sending is safe. See
/// `docs/testing/geth-state-lag.md` and [`Fixture::cast_send`]'s doc comment
/// for the full policy this implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendFailureClass {
    /// The node rejected the transaction outright (`replacement transaction
    /// underpriced`): nothing entered the chain, so re-sending is
    /// idempotent and safe.
    SafeRetry,
    /// The transaction may already have landed (`already known` / `nonce
    /// too low`). Re-sending here could double-apply a write (e.g. a
    /// double deposit); the caller MUST resolve this via a receipt lookup,
    /// never by re-sending.
    Ambiguous,
    /// Any other failure is a hard, non-retryable error.
    Hard,
}

/// Classify a `cast send` failure's stderr text into a [`SendFailureClass`].
/// Pure and independent of any live chain so the retry/ambiguity policy in
/// [`run_cast_send_retry`] is unit-testable without a devnet.
fn classify_send_failure(stderr: &str) -> SendFailureClass {
    if stderr.contains("replacement transaction underpriced") {
        SendFailureClass::SafeRetry
    } else if stderr.contains("already known") || stderr.contains("nonce too low") {
        SendFailureClass::Ambiguous
    } else {
        SendFailureClass::Hard
    }
}

/// Outcome of one raw `cast send` attempt, as classified from its process
/// exit status. Threaded through [`run_cast_send_retry`] so the retry
/// policy can be exercised by a real `Command` in production and by a fake
/// closure in tests.
enum SendAttemptOutcome {
    /// `cast send` exited 0 and the transaction mined; carries `cast send
    /// --json`'s parsed receipt.
    Mined(serde_json::Value),
    /// `cast send` failed; carries its classification and a fully-formatted
    /// message for the eventual hard-error text.
    Failed(SendFailureClass, String),
}

/// Drives the send-retry/ambiguity policy for [`Fixture::cast_send`] (issue
/// #1241). `attempt(n)` performs (or, in tests, simulates) the nth raw send;
/// `lookup_receipt` resolves an ambiguous outcome by looking up whatever
/// transaction actually consumed the pinned nonce — it must be a real
/// receipt query, never a re-send.
///
/// - [`SendFailureClass::SafeRetry`]: nothing entered the chain, so `attempt`
///   is called again (up to `max_attempts` total calls).
/// - [`SendFailureClass::Ambiguous`]: `attempt` is NEVER called again — the
///   write may already have landed, so the only safe resolution is
///   `lookup_receipt`.
/// - [`SendFailureClass::Hard`]: propagated immediately.
fn run_cast_send_retry(
    max_attempts: u32,
    mut attempt: impl FnMut(u32) -> SendAttemptOutcome,
    mut lookup_receipt: impl FnMut() -> Result<serde_json::Value, HarnessError>,
) -> Result<serde_json::Value, HarnessError> {
    assert!(
        max_attempts > 0,
        "run_cast_send_retry requires at least one attempt"
    );
    for n in 0..max_attempts {
        match attempt(n) {
            SendAttemptOutcome::Mined(v) => return Ok(v),
            SendAttemptOutcome::Failed(SendFailureClass::SafeRetry, msg) => {
                if n + 1 == max_attempts {
                    return Err(HarnessError::other(msg));
                }
            }
            SendAttemptOutcome::Failed(SendFailureClass::Ambiguous, msg) => {
                return lookup_receipt().map_err(|e| {
                    HarnessError::other(format!(
                        "cast send outcome ambiguous ({msg}) and could not be resolved by \
                         receipt lookup: {e}"
                    ))
                });
            }
            SendAttemptOutcome::Failed(SendFailureClass::Hard, msg) => {
                return Err(HarnessError::other(msg));
            }
        }
    }
    unreachable!("loop above always returns before exhausting max_attempts")
}

fn read_deployment(path: &Path) -> Result<DeploymentJson, HarnessError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))?;
    serde_json::from_str(&raw)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))
}

/// Run the DeployVaultRegistry forge script (issue #294) and write the
/// registry deployment JSON to `reg_out`. The deployer EOA (which holds
/// `ADMIN_ROLE` on the newly-deployed registry) broadcasts via its private
/// key, so `msg.sender` on `registerVault` is the admin — no `vm.prank` is
/// needed inside the script's `run()` entrypoint.
fn run_forge_deploy_registry(
    repo_root: &Path,
    rpc_url: &str,
    reg_out: &Path,
    vault_address: &str,
    usdc_address: &str,
) -> Result<(), HarnessError> {
    let mut cmd = Command::new("forge");
    cmd.args([
        "script",
        "contracts/script/DeployVaultRegistry.s.sol:DeployVaultRegistry",
    ])
    .args(["--rpc-url", rpc_url])
    .args(["--private-key", DEPLOYER_PRIVATE_KEY_HEX])
    .arg("--broadcast")
    .arg("--slow")
    .arg("-vvv")
    .env("ADMIN_ADDRESS", DEPLOYER_ADDRESS_HEX)
    .env("VAULT_ADDRESS", vault_address)
    .env("USDC_ADDRESS", usdc_address)
    .env("VAULT_NAME", "Robot Money USDC")
    .env("DEPLOYMENT_OUT", reg_out)
    .current_dir(repo_root);
    let out = cmd.output()?;
    logging::log_command_output("forge-registry", &out);
    if !out.status.success() {
        return Err(HarnessError::DeployFailed(format!(
            "forge script DeployVaultRegistry exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn read_registry_deployment(path: &Path) -> Result<RegistryDeploymentJson, HarnessError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))?;
    serde_json::from_str(&raw)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))
}

/// Run the DeployPortfolioRouter forge script (issue #303) and write the
/// router deployment JSON to `router_out`. Sets initial weights to 10 000 bps
/// (100%) pointing at RobotMoneyVault — the sole active vault at this phase.
///
/// `fork_block` pins the forge simulation to a specific chain head so that
/// the simulation sees the `registerVault` tx from `run_forge_deploy_registry`
/// regardless of Geth's "latest" propagation timing.
fn run_forge_deploy_router(
    repo_root: &Path,
    rpc_url: &str,
    fork_block: u64,
    router_out: &Path,
    registry_address: &str,
    vault_address: &str,
    usdc_address: &str,
) -> Result<(), HarnessError> {
    let mut cmd = Command::new("forge");
    cmd.args([
        "script",
        "contracts/script/DeployPortfolioRouter.s.sol:DeployPortfolioRouter",
    ])
    .args(["--rpc-url", rpc_url])
    .args(["--fork-block-number", &fork_block.to_string()])
    .args(["--private-key", DEPLOYER_PRIVATE_KEY_HEX])
    .arg("--broadcast")
    .arg("--slow")
    .arg("-vvv")
    .env("ADMIN_ADDRESS", DEPLOYER_ADDRESS_HEX)
    .env("REGISTRY_ADDRESS", registry_address)
    .env("VAULT_ADDRESS", vault_address)
    .env("USDC_ADDRESS", usdc_address)
    .env("DEPLOYMENT_OUT", router_out)
    .current_dir(repo_root);
    let out = cmd.output()?;
    logging::log_command_output("forge-router", &out);
    if !out.status.success() {
        return Err(HarnessError::DeployFailed(format!(
            "forge script DeployPortfolioRouter exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Query the chain head block number via `cast block-number`. Used to pin
/// the forge simulation fork block after `run_forge_deploy_registry` so the
/// router simulation sees the `registerVault` tx regardless of Geth's
/// "latest" propagation timing.
fn fetch_current_block_number(rpc_url: &str) -> Result<u64, HarnessError> {
    let out = Command::new("cast")
        .args(["block-number", "--rpc-url", rpc_url])
        .output()?;
    if !out.status.success() {
        return Err(HarnessError::Other(format!(
            "cast block-number failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.parse::<u64>().map_err(|e| {
        HarnessError::Other(format!("cast block-number returned non-integer {s:?}: {e}"))
    })
}

/// Poll the VaultRegistry until `vault` appears in `listVaults()`, so the
/// PortfolioRouter deploy — whose simulation is pinned to the chain head right
/// after — forks a block that already includes the `registerVault` tx. Avoids
/// the intermittent `setRouterEligible -> NotRegistered()` race where Geth's
/// "latest" lags the just-mined registration on a loaded CI runner (issue #880).
/// This is an instance of the Geth read-after-write state-lag class; canonical
/// doc: `docs/testing/geth-state-lag.md`.
fn wait_for_vault_registered(
    rpc_url: &str,
    registry: &str,
    vault: &str,
) -> Result<(), HarnessError> {
    let needle = vault.to_lowercase();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        match Command::new("cast")
            .args([
                "call",
                registry,
                "listVaults()(address[])",
                "--rpc-url",
                rpc_url,
            ])
            .output()
        {
            Ok(out) if out.status.success() => {
                last = String::from_utf8_lossy(&out.stdout).to_lowercase();
                if last.contains(&needle) {
                    return Ok(());
                }
            }
            Ok(out) => last = String::from_utf8_lossy(&out.stderr).trim().to_string(),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(HarnessError::Other(format!(
        "vault {vault} not visible in registry {registry} listVaults() after 30s; last: {last}"
    )))
}

fn read_router_deployment(path: &Path) -> Result<RouterDeploymentJson, HarnessError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))?;
    serde_json::from_str(&raw)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))
}

/// Deploy RouterGovernance via forge script. The deployer holds ADMIN_ROLE.
/// Voting power is assigned per-test via `setVotingPower` (issue #364).
fn run_forge_deploy_governance(
    repo_root: &Path,
    rpc_url: &str,
    governance_out: &Path,
    router_address: &str,
) -> Result<(), HarnessError> {
    let mut cmd = Command::new("forge");
    cmd.args([
        "script",
        "contracts/script/DeployRouterGovernance.s.sol:DeployRouterGovernance",
    ])
    .args(["--rpc-url", rpc_url])
    .args(["--private-key", DEPLOYER_PRIVATE_KEY_HEX])
    .arg("--broadcast")
    .arg("--slow")
    .arg("-vvv")
    .env("ADMIN_ADDRESS", DEPLOYER_ADDRESS_HEX)
    .env("ROUTER_ADDRESS", router_address)
    .env("DEPLOYMENT_OUT", governance_out)
    .current_dir(repo_root);
    let out = cmd.output()?;
    logging::log_command_output("forge-governance", &out);
    if !out.status.success() {
        return Err(HarnessError::DeployFailed(format!(
            "forge script DeployRouterGovernance exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn read_governance_deployment(path: &Path) -> Result<GovernanceDeploymentJson, HarnessError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))?;
    serde_json::from_str(&raw)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))
}

/// Deploy the RmToken ERC-20 contract and write its address to `rm_token_out`
/// (issue #365). The entire initial supply goes to `initial_holder` so that
/// `Fixture::fund_rm_token` can drip RM without forge/Anvil cheats.
fn run_forge_deploy_rm_token(
    repo_root: &Path,
    rpc_url: &str,
    rm_token_out: &Path,
    initial_holder: &str,
) -> Result<(), HarnessError> {
    let mut cmd = Command::new("forge");
    cmd.args([
        "script",
        "contracts/script/DeployRmToken.s.sol:DeployRmToken",
    ])
    .args(["--rpc-url", rpc_url])
    .args(["--private-key", DEPLOYER_PRIVATE_KEY_HEX])
    .arg("--broadcast")
    .arg("--slow")
    .arg("-vvv")
    .env("INITIAL_HOLDER", initial_holder)
    .env("DEPLOYMENT_OUT", rm_token_out)
    .current_dir(repo_root);
    let out = cmd.output()?;
    logging::log_command_output("forge-rm-token", &out);
    if !out.status.success() {
        return Err(HarnessError::DeployFailed(format!(
            "forge script DeployRmToken exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn read_rm_token_deployment(path: &Path) -> Result<RmTokenDeploymentJson, HarnessError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))?;
    serde_json::from_str(&raw)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))
}

/// Deploy InvestmentCommitteePolicy + ConsensusRebalanceReceipt in one
/// ceremony via `DeployInvestmentCommitteePolicy.s.sol` (issue #1247 AC10,
/// issue #1294). `RECEIPT_ADMIN_ADDRESS` is left unset so the script
/// defaults it to `ADMIN_ADDRESS` (the deployer) for this devnet ceremony —
/// production deployments hold it with the TimelockController.
fn run_forge_deploy_ic_policy(
    repo_root: &Path,
    rpc_url: &str,
    ic_policy_out: &Path,
    gateway_address: &str,
) -> Result<(), HarnessError> {
    let mut cmd = Command::new("forge");
    cmd.args([
        "script",
        "contracts/script/DeployInvestmentCommitteePolicy.s.sol:DeployInvestmentCommitteePolicy",
    ])
    .args(["--rpc-url", rpc_url])
    .args(["--private-key", DEPLOYER_PRIVATE_KEY_HEX])
    .arg("--broadcast")
    .arg("--slow")
    .arg("-vvv")
    .env("ADMIN_ADDRESS", DEPLOYER_ADDRESS_HEX)
    .env("GATEWAY_ADDRESS", gateway_address)
    .env("DEPLOYMENT_OUT", ic_policy_out)
    .current_dir(repo_root);
    let out = cmd.output()?;
    logging::log_command_output("forge-ic-policy", &out);
    if !out.status.success() {
        return Err(HarnessError::DeployFailed(format!(
            "forge script DeployInvestmentCommitteePolicy exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn read_ic_policy_deployment(path: &Path) -> Result<IcPolicyDeploymentJson, HarnessError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))?;
    serde_json::from_str(&raw)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))
}

/// Deploy the PRD §11 demo vault catalog via `DeployDemoExtraVaults.s.sol`:
/// ProtocolAssetVault (§11.2) seeded with wETH/cbBTC/wSOL stand-ins,
/// AgentTokenVault (§11.3) seeded with the six MVP shortlist symbols, and an
/// RWA/Thematic placeholder (§11.4) registered Paused. Resets the router
/// weight vector to a single-leg pointing at the primary vault — the basket
/// vaults stay router-ineligible per PRD §11.2/§11.3.
fn run_forge_deploy_demo_extra_vaults(
    repo_root: &Path,
    rpc_url: &str,
    out: &Path,
    registry_address: &str,
    router_address: &str,
    primary_vault: &str,
    usdc_address: &str,
) -> Result<(), HarnessError> {
    let mut cmd = Command::new("forge");
    cmd.args([
        "script",
        "contracts/script/DeployDemoExtraVaults.s.sol:DeployDemoExtraVaults",
    ])
    .args(["--rpc-url", rpc_url])
    .args(["--private-key", DEPLOYER_PRIVATE_KEY_HEX])
    .arg("--broadcast")
    .arg("--slow")
    .arg("-vvv")
    .env("ADMIN_ADDRESS", DEPLOYER_ADDRESS_HEX)
    // EMERGENCY_RESPONDER_ADDRESS is required by DeployDemoExtraVaults after issue #506.
    // For the devnet demo seed, use the same deployer address as the emergency responder
    // (equal admin/emergency is explicitly allowed by the constructor).
    .env("EMERGENCY_RESPONDER_ADDRESS", DEPLOYER_ADDRESS_HEX)
    .env("REGISTRY_ADDRESS", registry_address)
    .env("ROUTER_ADDRESS", router_address)
    .env("PRIMARY_VAULT", primary_vault)
    .env("USDC_ADDRESS", usdc_address)
    .env("DEPLOYMENT_OUT", out)
    .current_dir(repo_root);
    let output = cmd.output()?;
    logging::log_command_output("forge-demo-extra-vaults", &output);
    if !output.status.success() {
        return Err(HarnessError::DeployFailed(format!(
            "forge script DeployDemoExtraVaults exited {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn read_demo_extra_vaults_deployment(
    path: &Path,
) -> Result<DemoExtraVaultsDeploymentJson, HarnessError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))?;
    serde_json::from_str(&raw)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))
}

/// Deploy the four `UniswapV3PoolSlot0Stub` contracts via the Arachnid
/// CREATE2 factory (issue #531). The factory is pre-installed in the devnet
/// genesis alloc (`genesis_alloc::ARACHNID_FACTORY_ADDR`). Each stub is
/// deployed with a fixed salt producing the same address across devnet resets.
/// These addresses are pre-committed in `config/dex-pools.json::devnet.pools`
/// so the dapp Docker image is built with the correct pool addresses.
fn run_forge_deploy_demo_uniswap_v3_stubs(
    repo_root: &Path,
    rpc_url: &str,
    out: &Path,
) -> Result<(), HarnessError> {
    let mut cmd = Command::new("forge");
    cmd.args([
        "script",
        "contracts/script/DeployDemoUniswapV3Stubs.s.sol:DeployDemoUniswapV3Stubs",
    ])
    .args(["--rpc-url", rpc_url])
    .args(["--private-key", DEPLOYER_PRIVATE_KEY_HEX])
    .arg("--broadcast")
    .arg("--slow")
    .arg("-vvv")
    .env("DEPLOYMENT_OUT", out)
    .current_dir(repo_root);
    let output = cmd.output()?;
    logging::log_command_output("forge-uniswap-v3-stubs", &output);
    if !output.status.success() {
        return Err(HarnessError::DeployFailed(format!(
            "forge script DeployDemoUniswapV3Stubs exited {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn read_demo_uniswap_v3_stubs_deployment(
    path: &Path,
) -> Result<DemoUniswapV3StubsDeploymentJson, HarnessError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))?;
    serde_json::from_str(&raw)
        .map_err(|e| HarnessError::DeploymentJson(path.to_path_buf(), e.to_string()))
}

/// Walk up from the crate manifest dir until we find the repo root
/// (identified by `foundry.toml` + `clients/rust-payment-client`).
pub fn locate_repo_root() -> Result<PathBuf, HarnessError> {
    test_utils::find_workspace_root()
        .ok_or_else(|| HarnessError::other("could not locate repo root from CARGO_MANIFEST_DIR"))
}

fn start_compose_log_follower(
    compose_dir: &Path,
    compose_args: &[String],
    compose_env: &[(&str, String)],
    service_label: &'static str,
) -> Result<MonitoredChild, HarnessError> {
    let mut cmd = Command::new("docker");
    cmd.arg("compose");
    for arg in compose_args {
        cmd.arg(arg);
    }
    for (key, value) in compose_env {
        cmd.env(key, value);
    }
    cmd.args(["logs", "--follow", "--no-color", "--timestamps"])
        .current_dir(compose_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| HarnessError::other(format!("compose logs spawn: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::other("compose logs stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| HarnessError::other("compose logs stderr unavailable"))?;
    let stdout_label = service_label.to_string();
    let stderr_label = service_label.to_string();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some((service, message)) = parse_compose_log_line(&line) {
                logging::info(&service, message);
            } else {
                logging::info(&stdout_label, line);
            }
        }
    });
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            logging::error(&stderr_label, line);
        }
    });
    Ok(MonitoredChild::new(service_label, child))
}

fn log_compose_state(
    compose_dir: &Path,
    compose_args: &[String],
    compose_env: &[(&str, String)],
    service_label: &'static str,
    reason: &str,
    tail_lines: u32,
) {
    logging::warn(
        service_label,
        format!("capturing compose state for {reason}; ps/logs follow"),
    );

    let mut ps = Command::new("docker");
    ps.arg("compose");
    for arg in compose_args {
        ps.arg(arg);
    }
    for (key, value) in compose_env {
        ps.env(key, value);
    }
    ps.args(["ps", "--all", "--no-trunc"])
        .current_dir(compose_dir);
    match ps.output() {
        Ok(out) => logging::log_command_output(service_label, &out),
        Err(err) => logging::error(service_label, format!("compose ps failed: {err}")),
    }
    match compose_container_statuses(compose_dir, compose_args, compose_env) {
        Ok(statuses) => {
            if statuses.is_empty() {
                logging::warn(service_label, "compose ps returned no containers");
            } else {
                for status in statuses {
                    logging::info(service_label, status.describe());
                }
            }
        }
        Err(err) => logging::error(service_label, format!("compose inspect failed: {err}")),
    }

    let mut logs = Command::new("docker");
    logs.arg("compose");
    for arg in compose_args {
        logs.arg(arg);
    }
    for (key, value) in compose_env {
        logs.env(key, value);
    }
    let tail = tail_lines.to_string();
    logs.args(["logs", "--no-color", "--timestamps", "--tail", &tail])
        .current_dir(compose_dir);
    match logs.output() {
        Ok(out) => logging::log_command_output(service_label, &out),
        Err(err) => logging::error(service_label, format!("compose logs failed: {err}")),
    }
}

fn compose_health_probe<'a>(
    compose_dir: &'a Path,
    compose_args: &'a [String],
    compose_env: &'a [(&str, String)],
    service_label: &'static str,
) -> impl FnMut() -> Result<(), HarnessError> + 'a {
    move || {
        let statuses = compose_container_statuses(compose_dir, compose_args, compose_env)?;
        if statuses.is_empty() {
            logging::error(
                service_label,
                "compose health probe found no containers for the stack",
            );
            return Err(HarnessError::Docker(format!(
                "{service_label} container health probe found no running containers"
            )));
        }
        let unhealthy = statuses
            .into_iter()
            .filter(|status| !is_completed_one_shot(status) && status.is_unhealthy())
            .collect::<Vec<_>>();
        if unhealthy.is_empty() {
            return Ok(());
        }
        for status in &unhealthy {
            logging::error(service_label, status.describe());
        }
        Err(HarnessError::Docker(format!(
            "{service_label} container health probe detected {} unhealthy container(s)",
            unhealthy.len()
        )))
    }
}

fn compose_container_statuses(
    compose_dir: &Path,
    compose_args: &[String],
    compose_env: &[(&str, String)],
) -> Result<Vec<ComposeContainerStatus>, HarnessError> {
    let mut ps = Command::new("docker");
    ps.arg("compose");
    for arg in compose_args {
        ps.arg(arg);
    }
    for (key, value) in compose_env {
        ps.env(key, value);
    }
    ps.args(["ps", "-q", "--all"]).current_dir(compose_dir);
    let out = ps.output().map_err(HarnessError::from)?;
    if !out.status.success() {
        return Err(HarnessError::Docker(format!(
            "compose ps -q failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )));
    }

    let ids = String::from_utf8_lossy(&out.stdout);
    let mut statuses = Vec::new();
    for id in ids.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let inspect_out = Command::new("docker")
            .args(["inspect", id])
            .output()
            .map_err(HarnessError::from)?;
        if !inspect_out.status.success() {
            let stderr = String::from_utf8_lossy(&inspect_out.stderr);
            if stderr.contains("no such object")
                || stderr.contains("No such object")
                || stderr.contains("No such container")
            {
                logging::warn(
                    "compose",
                    format!("container {id} disappeared before inspect completed"),
                );
                continue;
            }
            return Err(HarnessError::Docker(format!(
                "docker inspect {id} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&inspect_out.stdout),
                String::from_utf8_lossy(&inspect_out.stderr)
            )));
        }
        let payload: Vec<serde_json::Value> = serde_json::from_slice(&inspect_out.stdout)
            .map_err(|e| HarnessError::Docker(format!("docker inspect {id} json: {e}")))?;
        let Some(container) = payload.into_iter().next() else {
            continue;
        };
        let Some(state) = container.get("State") else {
            continue;
        };
        let name = container
            .get("Name")
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .trim_start_matches('/')
            .to_string();
        let service = container
            .get("Config")
            .and_then(|v| v.get("Labels"))
            .and_then(|v| v.get("com.docker.compose.service"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        statuses.push(ComposeContainerStatus {
            id: id.to_string(),
            name,
            service,
            state: state
                .get("Status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            health: state
                .get("Health")
                .and_then(|v| v.get("Status"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            exit_code: state.get("ExitCode").and_then(|v| v.as_i64()),
            oom_killed: state
                .get("OOMKilled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            error: state
                .get("Error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        });
    }
    Ok(statuses)
}

fn parse_compose_log_line(line: &str) -> Option<(String, String)> {
    let (service, message) = line.split_once('|')?;
    let service = service.trim();
    let message = message.trim_start();
    if service.is_empty() || message.is_empty() {
        return None;
    }
    Some((service.to_string(), message.to_string()))
}

fn public_endpoints_label(public_endpoints: &PublicEndpoints) -> &'static str {
    match public_endpoints {
        PublicEndpoints::Local => "local",
        PublicEndpoints::EphemeralTunnel => "ephemeral-tunnel",
        PublicEndpoints::Named { .. } => "named",
    }
}

// -- DappStack --------------------------------------------------------

/// URLs for the full dapp stack services, printed as the structured
/// endpoint summary after all services pass their health checks.
pub struct DappEndpoints {
    pub rpc_url: String,
    pub dapp_url: String,
    pub explorer_api_url: String,
}

/// Manages the second docker-compose stack (dapp + explorer-api +
/// explorer-indexer + Postgres) started by `--full-stack`. Drop tears
/// the stack down unconditionally.
///
/// Canonical: Plan tracking issue #109 §10.5 — Phase 4.5.
/// Boot via [`DappStack::boot`] after the chain fixture is ready and
/// contracts are deployed.
pub struct DappStack {
    compose_dir: PathBuf,
    gateway_hex: String,
    vault_hex: String,
    gateway_runtime_hash: String,
    compose_log_followers: Vec<MonitoredChild>,
    pub endpoints: DappEndpoints,
    _tunnels: Option<Tunnels>,
    /// Env vars captured at boot time for `rebuild_dapp`.
    rebuild_env: Vec<(String, String)>,
}

/// Where the dapp, RPC, and explorer-api are publicly reachable from a
/// browser. Selected by [`DappStack::boot`]:
///
/// - [`PublicEndpoints::Local`] — bind only to localhost; no public
///   reachability. Default for unit / Playwright tests.
/// - [`PublicEndpoints::EphemeralTunnel`] — open three
///   `trycloudflare.com` quick tunnels and bake the random URLs into
///   the dapp bundle. Demo affordance only.
/// - [`PublicEndpoints::Named`] — caller supplies the three public
///   URLs explicitly. Use when a stable reverse proxy (e.g. a named
///   cloudflared tunnel with fixed hostnames) already fronts the
///   pinned local ports. The bundle is built with those URLs.
pub enum PublicEndpoints {
    Local,
    EphemeralTunnel,
    Named {
        rpc_url: String,
        dapp_url: String,
        explorer_api_url: String,
    },
}

/// Options for [`DappStack::boot`].
pub struct DappStackOptions {
    pub dapp_port: Option<u16>,
    pub explorer_api_port: Option<u16>,
    pub public_endpoints: PublicEndpoints,
}

impl DappStack {
    /// Build and start the dapp compose stack, injecting the deployed
    /// contract addresses as build args. Waits for the dapp and
    /// explorer-api health checks to pass before returning.
    pub fn boot(fixture: &Fixture, opts: DappStackOptions) -> Result<Self, HarnessError> {
        // Share the chain boot's run-id (already set in this process) so the
        // dapp containers carry the same run-identity labels and are reaped
        // together; mint one defensively if a caller boots the dapp stack
        // without first booting the chain fixture.
        ensure_run_identity();

        // Fund the dedicated dapp-faucet EOA (issue #1241) before the dapp
        // container comes up, so the first browser-triggered faucet drip has
        // USDC/RM/ETH to send. This key is distinct from
        // `HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX` (see `dapp_faucet_key`),
        // closing the concurrent-signer hazard where a live faucet drip and
        // `Fixture::seed_demo_depositors`'s own holder-key sends could race
        // on the same nonce — a hazard nonce-pinning inside `cast_send`
        // cannot fix on its own, because a browser wallet signing via viem
        // never goes through this harness's nonce tracker at all.
        let (faucet_private_key_hex, faucet_address) = dapp_faucet_key();
        const DAPP_FAUCET_ETH_WEI: &str = "500000000000000000"; // 0.5 ETH
        const DAPP_FAUCET_USDC_RESERVE: u128 = 50_000 * 1_000_000; // 50k USDC, 6dp
        const DAPP_FAUCET_RM_RESERVE: u128 = 50_000 * 1_000_000_000_000_000_000; // 50k RM, 18dp
        fund_eth_from_deployer(
            &fixture.nonce_tracker,
            &format!("{faucet_address:#x}"),
            DAPP_FAUCET_ETH_WEI,
        )
        .inspect_err(|err| {
            logging::error(
                "smoke-test",
                format!("funding dapp-faucet EOA {faucet_address:#x} with ETH failed: {err}"),
            );
        })?;
        fixture
            .fund_usdc(faucet_address, DAPP_FAUCET_USDC_RESERVE)
            .inspect_err(|err| {
                logging::error(
                    "smoke-test",
                    format!("funding dapp-faucet EOA {faucet_address:#x} with USDC failed: {err}"),
                );
            })?;
        fixture
            .fund_rm_token(faucet_address, DAPP_FAUCET_RM_RESERVE)
            .inspect_err(|err| {
                logging::error(
                    "smoke-test",
                    format!("funding dapp-faucet EOA {faucet_address:#x} with RM failed: {err}"),
                );
            })?;

        let compose_dir = fixture.repo_root().join("testing/ethereum-testnet/config");

        // Proactively purge any stale dapp compose state (containers, networks,
        // volumes) from a previous run before calling compose-up. This is
        // belt-and-suspenders over reap_stale_testnet_containers: that helper
        // force-removes individual containers but does NOT remove the
        // robotmoney-dapp_default Docker network, which can get stuck in
        // REMOVING state after an aborted CI run. A stuck network causes the
        // subsequent compose-up to fail immediately (in ~36 s, well before
        // image builds complete) because Docker cannot create a new network
        // with the same name. Calling compose-down here removes containers,
        // volumes, AND the stuck network atomically before we start fresh.
        // Best-effort: a non-zero exit here means there was nothing to clean
        // up, which is the happy path; we log the outcome and continue.
        purge_stale_dapp_compose_state(
            &compose_dir,
            fixture.gateway_hex(),
            fixture.vault_hex(),
            fixture.gateway_runtime_hash(),
        );
        let gateway_hex = fixture.gateway_hex();
        let vault_hex = fixture.vault_hex();
        let gateway_runtime_hash = fixture.gateway_runtime_hash().to_string();
        let ports = DappPorts::allocate(
            opts.dapp_port,
            opts.explorer_api_port,
            &fixture.occupied_ports(),
        )?;
        let cleanup_gateway_hex = gateway_hex.to_string();
        let cleanup_vault_hex = vault_hex.to_string();
        let cleanup_runtime_hash = gateway_runtime_hash.clone();
        let cleanup_compose_dir = compose_dir.clone();
        let cleanup = move || {
            let _ = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    "docker-compose.dapp.yaml",
                    "down",
                    "-v",
                    "--remove-orphans",
                ])
                .env("VITE_GATEWAY_ADDRESS", &cleanup_gateway_hex)
                .env("VITE_VAULT_ADDRESS", &cleanup_vault_hex)
                .env("VITE_GATEWAY_EXPECTED_CODE_HASH", &cleanup_runtime_hash)
                .env("INDEXER_GATEWAY", &cleanup_gateway_hex)
                .env("INDEXER_VAULT", &cleanup_vault_hex)
                .env("INDEXER_REGISTRY", "")
                .current_dir(&cleanup_compose_dir)
                .status();
        };

        let local_dapp_url = ports.dapp_url();
        let local_explorer_api_url = ports.explorer_api_url();
        let local_rpc_url = fixture.rpc_url().to_string();
        let dapp_compose_files = vec!["-f".to_string(), "docker-compose.dapp.yaml".to_string()];
        let dapp_log_env = vec![
            ("POSTGRES_PORT", ports.postgres_port.to_string()),
            ("EXPLORER_API_PORT", ports.explorer_api_port.to_string()),
            ("DAPP_PORT", ports.dapp_port.to_string()),
            ("VITE_GATEWAY_ADDRESS", gateway_hex.to_string()),
            ("VITE_VAULT_ADDRESS", vault_hex.to_string()),
            (
                "VITE_GATEWAY_EXPECTED_CODE_HASH",
                gateway_runtime_hash.clone(),
            ),
            // Issue #320: surface registry and router addresses so the dapp's
            // DestinationSelector can list registered vaults and offer the
            // Portfolio Router deposit path.
            ("VITE_REGISTRY_ADDRESS", fixture.registry_hex().to_string()),
            ("VITE_ROUTER_ADDRESS", fixture.router_hex().to_string()),
            // Issue #364: RouterGovernance address for the Governance tab.
            (
                "VITE_GOVERNANCE_ADDRESS",
                fixture.governance_hex().to_string(),
            ),
            // Issues #463/#466: surface the deployed RmToken address so the
            // main-page balances panel renders the RM row and the Faucet tab's
            // RM drip + balance reads point at the real ERC-20 contract
            // instead of falling back to the compose 0x0 default.
            ("VITE_RM_TOKEN_ADDRESS", fixture.rm_token_hex().to_string()),
            // Issue #1294: bucket-vault-symbol map so ConsensusReceiptPanel can
            // compute applied vs not-applied against live router weights.
            ("VITE_VAULT_ADDRESSES", fixture.vault_address_map_json()),
            ("INDEXER_GATEWAY", gateway_hex.to_string()),
            ("INDEXER_VAULT", vault_hex.to_string()),
            ("INDEXER_REGISTRY", fixture.registry_hex().to_string()),
            // Index WeightsSet/DefaultWeightsSet and RouterDeposit events from PortfolioRouter
            // (issue #615); router deposits trigger fresh TVL snapshots for all registered vaults.
            ("INDEXER_PORTFOLIO_ROUTER", fixture.router_hex().to_string()),
            // Issue #1294: index ReceiptRecorded/ReceiptReleased events from
            // ConsensusRebalanceReceipt and verify each payload_uri's digest.
            (
                "INDEXER_CONSENSUS_RECEIPT",
                fixture.consensus_receipt_hex().to_string(),
            ),
            // Issue #1294: fixed port for the receipt-fixtures compose service
            // (see Fixture::seed_consensus_receipts and RECEIPT_FIXTURES_PORT).
            ("RECEIPT_FIXTURES_PORT", RECEIPT_FIXTURES_PORT.to_string()),
            // Issue #775: indexer reaches Geth via the chain Docker network
            // (ethereum-testnet_default) using the service name, not via
            // host.docker.internal which is unreachable on some Docker configs.
            ("INDEXER_RPC_URL", "http://geth:8545".to_string()),
            ("VITE_DEVNET_RPC_URL", "".to_string()),
            ("VITE_EXPLORER_API_URL", "".to_string()),
            ("VITE_DAPP_URL", "".to_string()),
            (
                "VITE_FAUCET_HARNESS_PRIVATE_KEY",
                faucet_private_key_hex.clone(),
            ),
            ("INDEXER_CHAIN_ID", "918453".to_string()),
            ("INDEXER_CHAIN_NAME", "devnet".to_string()),
            ("EXPLORER_API_CHAIN_ID", "918453".to_string()),
        ];
        logging::info(
            "smoke-test",
            format!(
                "dapp startup config: project=robotmoney-dapp mode={} dapp_port={} explorer_api_port={} postgres_port={}",
                public_endpoints_label(&opts.public_endpoints),
                ports.dapp_port,
                ports.explorer_api_port,
                ports.postgres_port,
            ),
        );

        let (tunnels, vite_rpc_url, vite_dapp_url, vite_explorer_api_url) = match opts
            .public_endpoints
        {
            PublicEndpoints::Local => (
                None,
                local_rpc_url.clone(),
                local_dapp_url.clone(),
                local_explorer_api_url.clone(),
            ),
            PublicEndpoints::EphemeralTunnel => {
                let t =
                    Tunnels::start(fixture.rpc_port(), ports.dapp_port, ports.explorer_api_port)?;
                let urls = (
                    t.rpc_url.clone(),
                    t.dapp_url.clone(),
                    t.explorer_api_url.clone(),
                );
                (Some(t), urls.0, urls.1, urls.2)
            }
            PublicEndpoints::Named {
                rpc_url,
                dapp_url,
                explorer_api_url,
            } => (None, rpc_url, dapp_url, explorer_api_url),
        };

        let rebuild_env: Vec<(String, String)> = vec![
            ("POSTGRES_PORT".into(), ports.postgres_port.to_string()),
            (
                "EXPLORER_API_PORT".into(),
                ports.explorer_api_port.to_string(),
            ),
            ("DAPP_PORT".into(), ports.dapp_port.to_string()),
            ("VITE_GATEWAY_ADDRESS".into(), gateway_hex.to_string()),
            ("VITE_VAULT_ADDRESS".into(), vault_hex.to_string()),
            (
                "VITE_GATEWAY_EXPECTED_CODE_HASH".into(),
                gateway_runtime_hash.clone(),
            ),
            (
                "VITE_REGISTRY_ADDRESS".into(),
                fixture.registry_hex().to_string(),
            ),
            (
                "VITE_ROUTER_ADDRESS".into(),
                fixture.router_hex().to_string(),
            ),
            // Issue #364: RouterGovernance address for the Governance tab.
            (
                "VITE_GOVERNANCE_ADDRESS".into(),
                fixture.governance_hex().to_string(),
            ),
            // Issues #463/#466: surface the deployed RmToken address so the
            // main-page balances panel renders the RM row and the Faucet tab's
            // RM drip + balance reads point at the real ERC-20 contract
            // instead of falling back to the compose 0x0 default.
            (
                "VITE_RM_TOKEN_ADDRESS".into(),
                fixture.rm_token_hex().to_string(),
            ),
            // Issue #1294: bucket-vault-symbol map so ConsensusReceiptPanel can
            // compute applied vs not-applied against live router weights.
            (
                "VITE_VAULT_ADDRESSES".into(),
                fixture.vault_address_map_json(),
            ),
            ("INDEXER_GATEWAY".into(), gateway_hex.to_string()),
            ("INDEXER_VAULT".into(), vault_hex.to_string()),
            (
                "INDEXER_REGISTRY".into(),
                fixture.registry_hex().to_string(),
            ),
            // Index WeightsSet/DefaultWeightsSet from PortfolioRouter (issue #615).
            (
                "INDEXER_PORTFOLIO_ROUTER".into(),
                fixture.router_hex().to_string(),
            ),
            // Issue #1294: index ReceiptRecorded/ReceiptReleased events from
            // ConsensusRebalanceReceipt and verify each payload_uri's digest.
            (
                "INDEXER_CONSENSUS_RECEIPT".into(),
                fixture.consensus_receipt_hex().to_string(),
            ),
            // Issue #1294: fixed port for the receipt-fixtures compose service.
            (
                "RECEIPT_FIXTURES_PORT".into(),
                RECEIPT_FIXTURES_PORT.to_string(),
            ),
            // Issue #775: see dapp_log_env comment above.
            ("INDEXER_RPC_URL".into(), "http://geth:8545".to_string()),
            ("VITE_DEVNET_RPC_URL".into(), vite_rpc_url.clone()),
            (
                "VITE_EXPLORER_API_URL".into(),
                vite_explorer_api_url.clone(),
            ),
            ("VITE_DAPP_URL".into(), vite_dapp_url.clone()),
            (
                "VITE_FAUCET_HARNESS_PRIVATE_KEY".into(),
                faucet_private_key_hex.clone(),
            ),
            ("INDEXER_CHAIN_ID".into(), "918453".into()),
            ("INDEXER_CHAIN_NAME".into(), "devnet".into()),
            ("EXPLORER_API_CHAIN_ID".into(), "918453".into()),
        ];

        eprintln!("smoke-test: building and starting dapp stack (this may take several minutes for first build)...");

        logging::info("smoke-test", "bringing up full-stack compose services");
        let up_out = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg("docker-compose.dapp.yaml")
            .arg("up")
            .arg("-d")
            .arg("--build")
            .env("POSTGRES_PORT", ports.postgres_port.to_string())
            .env("EXPLORER_API_PORT", ports.explorer_api_port.to_string())
            .env("DAPP_PORT", ports.dapp_port.to_string())
            .env("VITE_GATEWAY_ADDRESS", gateway_hex)
            .env("VITE_VAULT_ADDRESS", vault_hex)
            .env("VITE_GATEWAY_EXPECTED_CODE_HASH", &gateway_runtime_hash)
            // Issue #320: thread registry and router addresses into the dapp
            // build so the DestinationSelector and router deposit flow work.
            .env("VITE_REGISTRY_ADDRESS", fixture.registry_hex())
            .env("VITE_ROUTER_ADDRESS", fixture.router_hex())
            // Issue #364: thread governance address into the dapp build.
            .env("VITE_GOVERNANCE_ADDRESS", fixture.governance_hex())
            // Issues #463/#466: thread RmToken address into the dapp build
            // so the main-page balances panel renders the RM row and the RM
            // drip points at the real ERC-20 contract instead of the compose
            // 0x0 default.
            .env("VITE_RM_TOKEN_ADDRESS", fixture.rm_token_hex())
            // Issue #1294: bucket-vault-symbol map so ConsensusReceiptPanel can
            // compute applied vs not-applied against live router weights.
            .env("VITE_VAULT_ADDRESSES", fixture.vault_address_map_json())
            .env("INDEXER_GATEWAY", gateway_hex)
            .env("INDEXER_VAULT", vault_hex)
            .env("INDEXER_REGISTRY", fixture.registry_hex())
            // Index WeightsSet/DefaultWeightsSet and RouterDeposit events from PortfolioRouter (issue #615).
            .env("INDEXER_PORTFOLIO_ROUTER", fixture.router_hex())
            // Issue #1294: index ReceiptRecorded/ReceiptReleased events from
            // ConsensusRebalanceReceipt and verify each payload_uri's digest.
            .env("INDEXER_CONSENSUS_RECEIPT", fixture.consensus_receipt_hex())
            // Issue #1294: fixed port for the receipt-fixtures compose service.
            .env("RECEIPT_FIXTURES_PORT", RECEIPT_FIXTURES_PORT.to_string())
            // Issue #775: indexer reaches Geth via the chain Docker network
            // (ethereum-testnet_default) using the `geth` service name — no
            // host port needed. The dapp compose connects to that network via
            // the chain-net external network reference in docker-compose.dapp.yaml.
            .env("INDEXER_RPC_URL", "http://geth:8545")
            // VITE_FORK_RPC_URL intentionally NOT set: the dapp routes all
            // chain reads through the user's wallet RPC (see
            // docs/technical/dapp-topology.md §2). VITE_DEVNET_RPC_URL is
            // passed as a *UX hint*: the dapp's Connect Wallet button uses
            // it to call `wallet_addEthereumChain` so MetaMask prefills the
            // RPC URL when prompting the user to add chain 918453. The
            // dapp never fetches from this URL itself.
            .env("VITE_DEVNET_RPC_URL", &vite_rpc_url)
            .env("VITE_EXPLORER_API_URL", &vite_explorer_api_url)
            .env("VITE_DAPP_URL", &vite_dapp_url)
            // Issue #261 (superseded by issue #1241): thread the dedicated
            // dapp-faucet key (`dapp_faucet_key`, funded at the top of
            // `DappStack::boot`) through to the dapp build so the testnet
            // Faucet tab + onboarding seed can drip USDC/RM/ETH via a real
            // signed transfer. This is now a distinct EOA from
            // `HARNESS_USDC_HOLDER_PRIVATE_KEY_HEX` — a live browser drip
            // during `Fixture::seed_demo_depositors` can no longer race the
            // harness's own nonce sequence for the holder key.
            .env("VITE_FAUCET_HARNESS_PRIVATE_KEY", &faucet_private_key_hex)
            .env("INDEXER_CHAIN_ID", "918453")
            .env("INDEXER_CHAIN_NAME", "devnet")
            .env("EXPLORER_API_CHAIN_ID", "918453")
            .current_dir(&compose_dir)
            .output()
            .map_err(HarnessError::from)?;
        logging::log_command_output("compose", &up_out);

        if !up_out.status.success() {
            log_compose_state(
                &compose_dir,
                &dapp_compose_files,
                &dapp_log_env,
                "dapp-compose",
                "compose up failed",
                200,
            );
            cleanup();
            return Err(HarnessError::Docker(
                "compose up dapp stack failed".to_string(),
            ));
        }

        let mut compose_log_followers = Vec::new();
        let dapp_log_env = {
            dapp_log_env
                .into_iter()
                .map(|(key, value)| match key {
                    "VITE_DEVNET_RPC_URL" => (key, vite_rpc_url.clone()),
                    "VITE_EXPLORER_API_URL" => (key, vite_explorer_api_url.clone()),
                    "VITE_DAPP_URL" => (key, vite_dapp_url.clone()),
                    _ => (key, value),
                })
                .collect::<Vec<_>>()
        };
        let dapp_log_follower = start_compose_log_follower(
            &compose_dir,
            &dapp_compose_files,
            &dapp_log_env,
            "dapp-compose",
        )
        .inspect_err(|err| {
            logging::error(
                "smoke-test",
                format!("dapp compose log follower failed: {err}"),
            );
            log_compose_state(
                &compose_dir,
                &dapp_compose_files,
                &dapp_log_env,
                "dapp-compose",
                "log follower startup failure",
                200,
            );
            cleanup();
        })?;
        compose_log_followers.push(dapp_log_follower);

        eprintln!("smoke-test: waiting for dapp containers to become ready...");
        logging::info("smoke-test", "waiting for dapp containers to become ready");
        // Health checks go to the local host ports — the tunnels are
        // user-facing only and need not be up for readiness.
        let dapp_probe_dir = compose_dir.clone();
        let mut dapp_health_probe = compose_health_probe(
            &dapp_probe_dir,
            &dapp_compose_files,
            &dapp_log_env,
            "dapp-compose",
        );
        wait_for_http_ok_with_probe(
            &format!("{local_explorer_api_url}/health"),
            Duration::from_secs(300),
            Some(&mut dapp_health_probe),
        )
        .inspect_err(|err| {
            logging::error(
                "smoke-test",
                format!("explorer-api readiness failed: {err}"),
            );
            log_compose_state(
                &compose_dir,
                &dapp_compose_files,
                &dapp_log_env,
                "dapp-compose",
                "explorer-api readiness timeout",
                200,
            );
            cleanup();
        })?;
        wait_for_http_ok_with_probe(
            &local_dapp_url,
            Duration::from_secs(300),
            Some(&mut dapp_health_probe),
        )
        .inspect_err(|err| {
            logging::error("smoke-test", format!("dapp readiness failed: {err}"));
            log_compose_state(
                &compose_dir,
                &dapp_compose_files,
                &dapp_log_env,
                "dapp-compose",
                "dapp readiness timeout",
                200,
            );
            cleanup();
        })?;

        // Seed demo depositors automatically so vault TVL is non-zero on
        // first load. 4 depositors with 1000 USDC each gives a visible
        // balance on all router-eligible vaults without requiring the
        // operator to run `make demo-seed-depositors` manually (issue #532).
        // The seeding uses the harness USDC holder key (no new secrets).
        const DEMO_SEED_DEPOSITOR_COUNT: u32 = 4;
        const DEMO_SEED_PER_USER_USDC: u128 = 1_000 * 1_000_000; // 1000 USDC (6dp)

        // Surface the seeding phase on stderr. Readiness has already
        // passed by this point, but every step below logs only to the
        // log file, so without this banner the terminal sits on the
        // "waiting for dapp containers to become ready..." line for the
        // ~2-3 minutes seeding takes (one approve+deposit per depositor,
        // each cast send waiting for a receipt) and looks frozen.
        eprintln!(
            "smoke-test: seeding {} demo depositors (this takes ~2-3 min; follow progress with `tail -f artifacts/smoke-test/smoke-test.log`)...",
            DEMO_SEED_DEPOSITOR_COUNT,
        );
        logging::info(
            "smoke-test",
            format!(
                "seeding {} demo depositors ({} USDC each) so vault TVL is non-zero on first load",
                DEMO_SEED_DEPOSITOR_COUNT,
                DEMO_SEED_PER_USER_USDC / 1_000_000,
            ),
        );
        fixture
            .seed_demo_depositors(DEMO_SEED_DEPOSITOR_COUNT, DEMO_SEED_PER_USER_USDC)
            .inspect_err(|err| {
                logging::error(
                    "smoke-test",
                    format!("demo depositor seeding failed: {err}"),
                );
                log_compose_state(
                    &compose_dir,
                    &dapp_compose_files,
                    &dapp_log_env,
                    "dapp-compose",
                    "demo depositor seeding failure",
                    200,
                );
                cleanup();
            })?;

        // Fund the deployer (admin EOA) with USDC so the 3 Playwright e2e
        // specs (vault-deposit-withdraw, router-deposit, multi-vault-withdrawal)
        // can run without their own bespoke fundUsdc() calls (issue #603).
        const ADMIN_SEED_USDC: u128 = 100_000_000; // 100 USDC (6dp)
        let admin_addr: Address = DEPLOYER_ADDRESS_HEX
            .parse()
            .expect("valid deployer address");
        fixture
            .fund_usdc(admin_addr, ADMIN_SEED_USDC)
            .inspect_err(|err| {
                logging::error("smoke-test", format!("admin USDC seeding failed: {err}"));
                cleanup();
            })?;
        logging::info(
            "smoke-test",
            format!("seeded admin EOA with {ADMIN_SEED_USDC} USDC (6dp)"),
        );

        eprintln!(
            "smoke-test: demo depositor seeding complete ({} depositors)",
            DEMO_SEED_DEPOSITOR_COUNT,
        );
        logging::info(
            "smoke-test",
            format!(
                "demo depositor seeding complete ({} depositors)",
                DEMO_SEED_DEPOSITOR_COUNT,
            ),
        );

        Ok(DappStack {
            compose_dir,
            gateway_hex: gateway_hex.to_string(),
            vault_hex: vault_hex.to_string(),
            gateway_runtime_hash,
            compose_log_followers,
            endpoints: DappEndpoints {
                rpc_url: vite_rpc_url,
                dapp_url: vite_dapp_url,
                explorer_api_url: vite_explorer_api_url,
            },
            _tunnels: tunnels,
            rebuild_env,
        })
    }

    /// Rebuild and restart only the `dapp` container in-place, leaving
    /// postgres, explorer-indexer, and explorer-api untouched. All VITE_*
    /// build args are re-injected from the values captured at boot time so
    /// the new bundle points at the same devnet addresses and ports.
    ///
    /// Docker's build output is streamed to the caller's stderr so the
    /// developer can see progress during the (potentially long) bun build.
    pub fn rebuild_dapp(&self) -> Result<(), HarnessError> {
        logging::info("smoke-test", "rebuilding dapp container (--no-deps)");
        let mut cmd = Command::new("docker");
        cmd.args([
            "compose",
            "-f",
            "docker-compose.dapp.yaml",
            "up",
            "--build",
            "--no-deps",
            "-d",
            "dapp",
        ]);
        for (k, v) in &self.rebuild_env {
            cmd.env(k, v);
        }
        let status = cmd
            .current_dir(&self.compose_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(HarnessError::from)?;
        if !status.success() {
            return Err(HarnessError::Docker("dapp rebuild failed".to_string()));
        }
        Ok(())
    }
}

// -- Tunnels ----------------------------------------------------------

/// Owns one `cloudflared tunnel --url` child process per exposed port and
/// stores the public `trycloudflare.com` URL each tunnel announced. Drop
/// kills every child, which is how Cloudflare's ephemeral tunnels close.
struct Tunnels {
    children: Vec<MonitoredChild>,
    rpc_url: String,
    dapp_url: String,
    explorer_api_url: String,
}

impl Tunnels {
    fn start(rpc_port: u16, dapp_port: u16, explorer_api_port: u16) -> Result<Self, HarnessError> {
        let (c_rpc, rpc_url) = spawn_tunnel(rpc_port)?;
        let (c_dapp, dapp_url) = spawn_tunnel(dapp_port)?;
        let (c_expl, explorer_api_url) = spawn_tunnel(explorer_api_port)?;
        Ok(Self {
            children: vec![c_rpc, c_dapp, c_expl],
            rpc_url,
            dapp_url,
            explorer_api_url,
        })
    }
}

impl Drop for Tunnels {
    fn drop(&mut self) {
        for child in &mut self.children {
            child.terminate();
        }
    }
}

fn spawn_tunnel(port: u16) -> Result<(MonitoredChild, String), HarnessError> {
    logging::debug(
        "cloudflared",
        format!("starting ephemeral tunnel for localhost:{port}"),
    );
    // `--config /dev/null` is load-bearing: without it cloudflared loads
    // `/etc/cloudflared/config.yml` if it exists on the host and conflates
    // the quick-tunnel URL with the host's named-tunnel credentials, which
    // makes the announced URL return CF 404. Forcing an empty config keeps
    // every invocation a true ephemeral quick tunnel.
    let mut child = Command::new("cloudflared")
        .args([
            "tunnel",
            "--no-autoupdate",
            "--config",
            "/dev/null",
            "--url",
            &format!("http://127.0.0.1:{port}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| HarnessError::other(format!("cloudflared spawn: {e}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| HarnessError::other("cloudflared stderr unavailable"))?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        let mut sent = false;
        for line in reader.lines().map_while(Result::ok) {
            logging::info("cloudflared", &line);
            if !sent {
                if let Some(url) = extract_trycloudflare_url(&line) {
                    logging::info("cloudflared", format!("announced public url {url}"));
                    let _ = tx.send(url);
                    sent = true;
                }
            }
        }
    });
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(url) => Ok((
            MonitoredChild::new(format!("cloudflared:{port}"), child),
            url,
        )),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(HarnessError::other(format!(
                "cloudflared URL not announced within 60s ({e})"
            )))
        }
    }
}

fn extract_trycloudflare_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let needle = ".trycloudflare.com";
    let end = rest.find(needle)? + needle.len();
    Some(rest[..end].to_string())
}

impl Drop for DappStack {
    fn drop(&mut self) {
        logging::info("dapp-compose", "tearing down dapp compose stack");
        for child in &mut self.compose_log_followers {
            child.terminate();
        }
        let _ = Command::new("docker")
            .args([
                "compose",
                "-f",
                "docker-compose.dapp.yaml",
                "down",
                "-v",
                "--remove-orphans",
            ])
            .env("VITE_GATEWAY_ADDRESS", &self.gateway_hex)
            .env("VITE_VAULT_ADDRESS", &self.vault_hex)
            .env(
                "VITE_GATEWAY_EXPECTED_CODE_HASH",
                &self.gateway_runtime_hash,
            )
            .env("INDEXER_GATEWAY", &self.gateway_hex)
            .env("INDEXER_VAULT", &self.vault_hex)
            .current_dir(&self.compose_dir)
            .status();
        logging::info("dapp-compose", "dapp compose teardown complete");
    }
}

/// Poll `url` with HTTP GET until a 2xx response is received or
/// `timeout` elapses. Used to wait for explorer-api and dapp health.
#[allow(dead_code)]
fn wait_for_http_ok(url: &str, timeout: Duration) -> Result<(), HarnessError> {
    wait_for_http_ok_with_probe(url, timeout, None)
}

fn wait_for_http_ok_with_probe(
    url: &str,
    timeout: Duration,
    mut health_probe: Option<&mut dyn FnMut() -> Result<(), HarnessError>>,
) -> Result<(), HarnessError> {
    logging::debug("http", format!("polling {url} for HTTP health"));
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| HarnessError::other(format!("reqwest builder: {e}")))?;
    let started = std::time::Instant::now();
    let deadline = started + timeout;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        if let Some(probe) = health_probe.as_deref_mut() {
            probe()?;
        }
        match client.get(url).send() {
            Ok(resp) if resp.status().is_success() => {
                logging::info(
                    "http",
                    format!(
                        "{url} ready in {}ms (HTTP {})",
                        started.elapsed().as_millis(),
                        resp.status()
                    ),
                );
                return Ok(());
            }
            Ok(resp) => last = format!("HTTP {}", resp.status()),
            Err(e) => last = format!("{e}"),
        }
        std::thread::sleep(READINESS_POLL_INTERVAL);
    }
    Err(HarnessError::other(format!(
        "service at {url} not healthy after {timeout:?}: {last}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited_status(service: &str, exit_code: i64) -> ComposeContainerStatus {
        ComposeContainerStatus {
            id: "deadbeef".to_string(),
            name: format!("dapp-{service}"),
            service: Some(service.to_string()),
            state: "exited".to_string(),
            health: None,
            exit_code: Some(exit_code),
            oom_killed: false,
            error: None,
        }
    }

    /// Issue #1359: `explorer-migrate` runs the explorer schema migration and
    /// exits. `is_unhealthy` classifies ANY exited container as unhealthy, so
    /// without the one-shot exemption the dapp stack's health probe would abort
    /// every full-stack boot the moment the migration finished successfully.
    #[test]
    fn completed_migrate_one_shot_is_not_treated_as_unhealthy() {
        let migrate = exited_status("explorer-migrate", 0);
        assert!(
            migrate.is_unhealthy(),
            "an exited container is unhealthy in the general case"
        );
        assert!(
            is_completed_one_shot(&migrate),
            "explorer-migrate exiting 0 must be exempted from the unhealthy set"
        );
    }

    /// The exemption is strictly for a SUCCESSFUL run: a failed migration is
    /// exactly the condition the explicit migrate step exists to surface, so it
    /// must still fail the probe.
    #[test]
    fn failed_migrate_one_shot_is_still_unhealthy() {
        let migrate = exited_status("explorer-migrate", 1);
        assert!(
            !is_completed_one_shot(&migrate),
            "a non-zero migrate exit must NOT be exempted"
        );
        assert!(migrate.is_unhealthy());
    }

    /// A long-running service that exits 0 is still a stack failure — the
    /// exemption is keyed on the service name, not on the exit code alone.
    #[test]
    fn long_running_service_exiting_zero_is_still_unhealthy() {
        let indexer = exited_status("explorer-indexer", 0);
        assert!(!is_completed_one_shot(&indexer));
        assert!(indexer.is_unhealthy());
    }

    #[test]
    fn compose_collision_guard_filters_running_containers() {
        let stdout = br#"{"Name":"eth-execution","State":"running"}
{"Name":"eth-beacon","State":"running"}
{"Name":"eth-validator-1","State":"exited"}
{"Name":"eth-validator-2","State":"paused"}
"#;

        let names = parse_compose_ps_stdout(stdout).expect("parse compose ps output");
        assert_eq!(names, vec!["eth-execution", "eth-beacon"]);
    }

    #[test]
    fn parse_compose_ps_stdout_ignores_empty_output() {
        let names = parse_compose_ps_stdout(b"\n\n").expect("parse empty output");
        assert!(names.is_empty());
    }

    #[test]
    fn reaper_keeps_current_run_and_marks_others_stale() {
        // `docker ps` label listing: <id>\t<run-id>\t<name>. CURRENT is the
        // active boot; PREVIOUS is a stranded run; the empty-run-id row is a
        // legacy container booted before run-id labels existed.
        let listing = "\
aaa111\tCURRENT\teth-execution
bbb222\tPREVIOUS\teth-execution
ccc333\t\teth-beacon
";
        let stale = stale_containers_from_listing(listing, "CURRENT");
        assert_eq!(
            stale,
            vec![
                ("bbb222".to_string(), "eth-execution".to_string()),
                ("ccc333".to_string(), "eth-beacon".to_string()),
            ],
            "previous-run and legacy (empty run-id) containers are stale; current run is kept"
        );
    }

    #[test]
    fn reaper_listing_tolerates_blank_lines_and_missing_columns() {
        let listing = "\n\nddd444\tOLD\n";
        // Missing name column -> name parses as empty string, still flagged stale.
        let stale = stale_containers_from_listing(listing, "CURRENT");
        assert_eq!(stale, vec![("ddd444".to_string(), String::new())]);
    }

    #[test]
    fn run_identity_is_stable_and_distinct_per_id() {
        // Two distinct ids never collide on the formatted prefix shape.
        let a = format!("{:016x}-{:x}", 1u64, 2u32);
        let b = format!("{:016x}-{:x}", 3u64, 2u32);
        assert_ne!(a, b);
        assert!(a.contains('-'));
    }

    #[test]
    fn receipt_status_succeeded_accepts_success_word() {
        // `cast send --json` reports a successful tx as status "0x1" (issue #904).
        assert!(receipt_status_succeeded("0x1"));
        assert!(receipt_status_succeeded("0X1"));
        // Robust to a non-canonical non-zero word.
        assert!(receipt_status_succeeded("0x01"));
    }

    #[test]
    fn receipt_status_succeeded_rejects_reverted_receipt() {
        // A reverted tx mines with status "0x0"; cast send still exits 0, so the
        // status word is the only signal that the tx reverted (issue #904).
        assert!(!receipt_status_succeeded("0x0"));
        assert!(!receipt_status_succeeded("0x00"));
        // A malformed/empty word must never be treated as success.
        assert!(!receipt_status_succeeded("0x"));
        assert!(!receipt_status_succeeded(""));
    }

    // -- issue #1241: cast_send nonce-pinning + retry/ambiguity policy ----

    #[test]
    fn classify_send_failure_treats_underpriced_replacement_as_safe_retry() {
        // The node refused the send outright — nothing entered the chain.
        let class = classify_send_failure(
            "Error: server returned an error response: error code -32000: replacement \
             transaction underpriced",
        );
        assert_eq!(class, SendFailureClass::SafeRetry);
    }

    #[test]
    fn classify_send_failure_treats_already_known_and_nonce_too_low_as_ambiguous() {
        // Both mean the write may already have landed; neither is safe to
        // resolve by re-sending (issue #1241 AC).
        assert_eq!(
            classify_send_failure(
                "Error: server returned an error response: error code -32000: already known"
            ),
            SendFailureClass::Ambiguous
        );
        assert_eq!(
            classify_send_failure(
                "Error: server returned an error response: error code -32003: nonce too low"
            ),
            SendFailureClass::Ambiguous
        );
    }

    #[test]
    fn classify_send_failure_treats_unrecognised_errors_as_hard() {
        assert_eq!(
            classify_send_failure("Error: insufficient funds for gas * price + value"),
            SendFailureClass::Hard
        );
    }

    #[test]
    fn retry_policy_retries_a_node_rejected_send_and_never_consults_receipt_lookup() {
        // A `SafeRetry` classification means nothing entered the chain, so
        // the policy must call `attempt` again — and must NOT touch
        // `lookup_receipt` at all, since there is nothing ambiguous to
        // resolve.
        let attempts = std::cell::Cell::new(0u32);
        let lookups = std::cell::Cell::new(0u32);
        let result = run_cast_send_retry(
            4,
            |_n| {
                attempts.set(attempts.get() + 1);
                if attempts.get() < 3 {
                    SendAttemptOutcome::Failed(
                        SendFailureClass::SafeRetry,
                        "replacement transaction underpriced".to_string(),
                    )
                } else {
                    SendAttemptOutcome::Mined(serde_json::json!({
                        "transactionHash": "0xabc",
                        "status": "0x1",
                    }))
                }
            },
            || {
                lookups.set(lookups.get() + 1);
                panic!("lookup_receipt must never be called for a SafeRetry outcome");
            },
        );
        assert_eq!(
            attempts.get(),
            3,
            "the rejected send must be retried until it lands"
        );
        assert_eq!(lookups.get(), 0);
        assert_eq!(
            result.expect("retry should eventually succeed")["transactionHash"],
            "0xabc"
        );
    }

    #[test]
    fn retry_policy_resolves_ambiguous_outcome_by_receipt_lookup_not_resend() {
        // `already known` / `nonce too low` must resolve via a receipt
        // lookup, and `attempt` must be called EXACTLY ONCE — a second call
        // would be a re-send, which for a write like `deposit` would be a
        // double-deposit. This is the double-deposit-impossible-by-
        // construction guarantee from the issue #1241 AC.
        for ambiguous_stderr in ["already known", "nonce too low"] {
            let attempts = std::cell::Cell::new(0u32);
            let lookups = std::cell::Cell::new(0u32);
            let result = run_cast_send_retry(
                4,
                |_n| {
                    attempts.set(attempts.get() + 1);
                    SendAttemptOutcome::Failed(
                        classify_send_failure(ambiguous_stderr),
                        ambiguous_stderr.to_string(),
                    )
                },
                || {
                    lookups.set(lookups.get() + 1);
                    Ok(serde_json::json!({
                        "transactionHash": "0xresolved",
                        "status": "0x1",
                    }))
                },
            );
            assert_eq!(
                attempts.get(),
                1,
                "an ambiguous outcome for {ambiguous_stderr:?} must never trigger a re-send"
            );
            assert_eq!(
                lookups.get(),
                1,
                "exactly one receipt lookup resolves the ambiguity"
            );
            assert_eq!(
                result.expect("ambiguous outcome resolves via the receipt lookup")
                    ["transactionHash"],
                "0xresolved"
            );
        }
    }

    #[test]
    fn retry_policy_propagates_hard_errors_without_retry_or_lookup() {
        let attempts = std::cell::Cell::new(0u32);
        let result = run_cast_send_retry(
            4,
            |_n| {
                attempts.set(attempts.get() + 1);
                SendAttemptOutcome::Failed(SendFailureClass::Hard, "insufficient funds".to_string())
            },
            || panic!("lookup_receipt must never be called for a Hard outcome"),
        );
        assert_eq!(attempts.get(), 1);
        assert!(result.is_err());
    }

    #[test]
    fn retry_policy_errors_when_the_safe_retry_budget_is_exhausted() {
        // Exhausting the retry budget on a persistently rejected send must
        // fail loudly, not silently return a bogus success.
        let result = run_cast_send_retry(
            3,
            |_n| {
                SendAttemptOutcome::Failed(
                    SendFailureClass::SafeRetry,
                    "replacement transaction underpriced".to_string(),
                )
            },
            || panic!("lookup_receipt must never be called for a SafeRetry outcome"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn parse_hex_rpc_result_parses_quoted_hex_quantity() {
        assert_eq!(parse_hex_rpc_result(b"\"0x1a\"", "test").unwrap(), 26);
        assert_eq!(parse_hex_rpc_result(b"\"0x0\"\n", "test").unwrap(), 0);
    }

    #[test]
    fn parse_hex_rpc_result_rejects_non_hex_payload() {
        assert!(parse_hex_rpc_result(b"not-hex", "test").is_err());
    }

    #[test]
    fn dapp_faucet_key_is_distinct_from_every_other_harness_key() {
        // issue #1241 AC: the dapp faucet must sign from an EOA distinct
        // from the harness USDC holder (and, defensively, every other
        // hand-picked harness key and the deterministic depositor keys).
        let (_priv, faucet_addr) = dapp_faucet_key();
        assert_ne!(
            format!("{faucet_addr:#x}"),
            HARNESS_USDC_HOLDER_ADDRESS_HEX.to_lowercase()
        );
        assert_ne!(
            format!("{faucet_addr:#x}"),
            DEPLOYER_ADDRESS_HEX.to_lowercase()
        );
        assert_ne!(
            format!("{faucet_addr:#x}"),
            PAUSER_ADDRESS_HEX.to_lowercase()
        );
        assert_ne!(
            format!("{faucet_addr:#x}"),
            SHARE_RECEIVER_ADDRESS_HEX.to_lowercase()
        );
        for i in 0..8 {
            let (_priv, depositor_addr) = demo_depositor_key(i);
            assert_ne!(faucet_addr, depositor_addr);
        }
    }

    #[test]
    fn dapp_faucet_key_is_deterministic_across_calls() {
        // Reproducible across process restarts, matching the deterministic
        // derivation contract of `demo_depositor_key`.
        assert_eq!(dapp_faucet_key(), dapp_faucet_key());
    }

    // -- issue #1374: the funding-path nonce race ------------------------
    //
    // The production symptom is geth answering a funding `cast send` with
    // `-32000: replacement transaction underpriced` during fixture bring-up,
    // which panics the test before it asserts anything. Its cause is two
    // same-account sends being handed the SAME nonce, and that is what these
    // tests pin down — the nonce-issuance decision and its serialisation,
    // both exercisable without Docker or a chain.

    const FUNDING_SENDER: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    #[test]
    fn next_pinned_nonce_never_repeats_a_nonce_the_node_has_not_absorbed_yet() {
        // THE RACE, reduced to one decision. geth's `pending` count does not
        // include a send it has not seen yet, so a second send issued moments
        // after the first reads the SAME count. Trusting that count verbatim
        // (`nonce = pending`) hands out nonce 7 twice -> the node rejects the
        // second with `replacement transaction underpriced`.
        assert_eq!(next_pinned_nonce(Some(7), 7), 8);
        // Still true when the node has regressed further behind.
        assert_eq!(next_pinned_nonce(Some(9), 7), 10);
    }

    #[test]
    fn next_pinned_nonce_defers_to_the_node_when_it_has_moved_ahead() {
        // First send from this address: the harness has no opinion yet, so
        // the node's count is the only truth available.
        assert_eq!(next_pinned_nonce(None, 4), 4);
        // A send this harness did not issue (a forge script broadcasting from
        // the deployer, say) pushed the count past our last pin — take the
        // node's number, never a stale `prev + 1` that would collide with it.
        assert_eq!(next_pinned_nonce(Some(4), 9), 9);
    }

    #[test]
    fn concurrent_pins_never_hand_out_a_colliding_nonce() {
        // Reproduces the race at its source. `seed_demo_depositors` funds on
        // two scoped threads and `DappStack::boot` funds while the fixture is
        // live, so concurrent pins for one sender are a real shape here.
        //
        // The fake node is a LAGGING one: it always reports the same
        // `pending` count, exactly as geth does for sends it has not absorbed
        // yet, and sleeps to widen the window. Before the fix
        // `pin_next_nonce` released the map lock around this read, so all
        // eight threads observed the same `prev`, read the same count, and
        // pinned the same nonce — seven of the eight sends would have come
        // back `replacement transaction underpriced`. This assertion is red
        // on that code and green once the lock spans the read.
        const THREADS: usize = 8;
        const STALE_PENDING: u64 = 11;
        let tracker = NonceTracker::new("http://unused.invalid");
        let pinned: Vec<u64> = thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let tracker = &tracker;
                    s.spawn(move || {
                        tracker
                            .pin_next_nonce_with(FUNDING_SENDER, |_| {
                                thread::sleep(Duration::from_millis(20));
                                Ok(STALE_PENDING)
                            })
                            .expect("pin_next_nonce_with")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("pin thread panicked"))
                .collect()
        });

        let mut sorted = pinned.clone();
        sorted.sort_unstable();
        let expected: Vec<u64> = (0..THREADS as u64).map(|i| STALE_PENDING + i).collect();
        assert_eq!(
            sorted, expected,
            "concurrent pins for one sender must be distinct and contiguous; got {pinned:?} — a \
             repeated nonce here IS the `replacement transaction underpriced` failure"
        );
    }

    #[test]
    fn pins_are_scoped_per_sender_address() {
        // Two different faucet keys must not consume one another's sequence:
        // the deployer and the USDC holder both fund, concurrently.
        let tracker = NonceTracker::new("http://unused.invalid");
        let other = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
        let a1 = tracker
            .pin_next_nonce_with(FUNDING_SENDER, |_| Ok(3))
            .unwrap();
        let b1 = tracker.pin_next_nonce_with(other, |_| Ok(3)).unwrap();
        let a2 = tracker
            .pin_next_nonce_with(FUNDING_SENDER, |_| Ok(3))
            .unwrap();
        assert_eq!((a1, b1, a2), (3, 3, 4));
        // Address case must not fork a sender into two sequences.
        let a3 = tracker
            .pin_next_nonce_with(&FUNDING_SENDER.to_lowercase(), |_| Ok(3))
            .unwrap();
        assert_eq!(a3, 5);
    }

    #[test]
    fn a_failed_send_hands_its_unused_nonce_back_for_the_next_one() {
        // A hard funding failure (unfunded faucet) leaves its pinned nonce
        // unconsumed. Because pins are monotonic, keeping it would make every
        // later send from that address pin past a gap and sit in the mempool
        // forever — one loud failure turning into a silent hang.
        let tracker = NonceTracker::new("http://unused.invalid");
        let pinned = tracker
            .pin_next_nonce_with(FUNDING_SENDER, |_| Ok(5))
            .unwrap();
        assert_eq!(pinned, 5);
        // Node still reports 5 pending: nothing consumed it.
        tracker.release_pin_if_unused_with(FUNDING_SENDER, pinned, |_| Ok(5));
        let reused = tracker
            .pin_next_nonce_with(FUNDING_SENDER, |_| Ok(5))
            .unwrap();
        assert_eq!(reused, 5, "the next send must reuse the unspent nonce");
    }

    #[test]
    fn a_send_that_did_land_keeps_its_nonce_pinned() {
        // The mirror case: the send failed from cast's point of view but the
        // node already absorbed it. Rewinding here would re-issue a nonce
        // that is already spoken for — which is precisely the `replacement
        // transaction underpriced` collision this issue exists to remove.
        let tracker = NonceTracker::new("http://unused.invalid");
        let pinned = tracker
            .pin_next_nonce_with(FUNDING_SENDER, |_| Ok(5))
            .unwrap();
        tracker.release_pin_if_unused_with(FUNDING_SENDER, pinned, |_| Ok(6));
        let next = tracker
            .pin_next_nonce_with(FUNDING_SENDER, |_| Ok(6))
            .unwrap();
        assert_eq!(next, 6, "a consumed nonce must never be handed out twice");
    }

    #[test]
    fn a_genuine_funding_failure_is_not_retried_and_stays_distinguishable() {
        // AC: an unfunded faucet must still fail LOUDLY, and must not be
        // mistaken for (or retried like) the nonce race. `insufficient funds`
        // classifies Hard, so the first attempt's message propagates verbatim
        // and `attempt` is never called again — the failure cannot be retried
        // into silence.
        let calls = std::cell::Cell::new(0u32);
        let err = run_cast_send_retry(
            4,
            |_| {
                calls.set(calls.get() + 1);
                SendAttemptOutcome::Failed(
                    SendFailureClass::Hard,
                    "fund eth failed: stdout= stderr=server returned an error response: error \
                     code -32000: insufficient funds for gas * price + value"
                        .to_string(),
                )
            },
            || panic!("a hard funding failure must never be resolved by receipt lookup"),
        )
        .expect_err("an unfunded faucet must surface as an error");
        assert_eq!(calls.get(), 1, "a hard funding failure must not be retried");
        let text = err.to_string();
        assert!(
            text.contains("fund eth failed") && text.contains("insufficient funds"),
            "the funding label and the node's reason must both survive: {text}"
        );
        assert!(
            !text.contains("replacement transaction underpriced"),
            "a genuine funding failure must not read as the nonce race: {text}"
        );
    }
}
