//! Canonical: Plan tracking issue #109 — Phase: Vault registry
//! Implements: issue #298
//!
//! Fork e2e scenarios for the on-chain VaultRegistry contract.
//!
//! Covered scenarios:
//!
//! - `registry_register_list` — deploy VaultRegistry, call `registerVault`,
//!   assert the `VaultRegistered` event is present in the receipt logs,
//!   call `listVaults` and verify the vault address appears.
//! - `registry_status_change` — after registration, call `setVaultStatus`
//!   to Paused, assert the `VaultStatusChanged` event is present in the logs,
//!   and verify `getVault` returns the updated status.
//! - `registry_empty_list` — before any registration, `listVaults` returns
//!   an empty array and `rmpc get-vaults` exits 0 with `vaults: []`.
//!
//! All scenarios run against the same per-test anvil-fork backend (fork
//! restart per test, per ADR §3.5). Each test has its own isolated chain
//! state for the full duration of the test function.
//!
//! The rmpc binary is compiled once (via `cargo build --bin rmpc`) and
//! reused for the `get-vaults` round-trip assertion. The VaultRegistry.sol
//! ABI used here is the on-chain contract (contracts/VaultRegistry.sol).
//!
//! Until issue #1362, `rmpc get-vaults` bound to an `abi/VaultRegistry.json`
//! whose `getVault` return type had drifted to a nine-field `VaultRecord` no
//! deployed contract returns, so the per-vault sub-read always failed and the
//! envelope was always partial. This file said so and asserted only on the
//! vault address — a field that comes from `listVaults()` and is populated even
//! when every `getVault` decode fails, so the workaround made these tests
//! insensitive to the very defect they were closest to observing. The
//! `getVault`-derived fields are now asserted here against real registered
//! state, which is the only live-chain check in CI that can catch that drift
//! recurring.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use alloy_primitives::{keccak256, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use rmpc_fork_e2e::{skip_if_no_fork, ForkFixture, IOnchainVaultRegistry};
use serde_json::Value;

// ── Workspace helpers ────────────────────────────────────────────────────────

/// Walk up from `CARGO_MANIFEST_DIR` (= testing/fork-e2e-rust) to the
/// workspace root.
fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // testing/fork-e2e-rust → testing → repo root
    p.pop();
    p.pop();
    p
}

// ── VaultRegistry bytecode ───────────────────────────────────────────────────

/// Load the VaultRegistry creation bytecode from the Foundry build artefact
/// (`out/VaultRegistry.sol/VaultRegistry.json`) and append the ABI-encoded
/// constructor argument `address admin` (left-padded to 32 bytes).
///
/// Requires `forge build` to have run first (the CI workflow does this via
/// the "Install Foundry" and "Build test binaries" steps). In local
/// development, run `forge build` from the workspace root before executing
/// the registry tests.
fn vault_registry_initcode(admin: Address) -> Bytes {
    let artifact_path = workspace_root()
        .join("out")
        .join("VaultRegistry.sol")
        .join("VaultRegistry.json");
    let raw = std::fs::read_to_string(&artifact_path).unwrap_or_else(|e| {
        panic!(
            "Cannot read Foundry build artefact at {}; run `forge build` first: {e}",
            artifact_path.display()
        )
    });
    let json: Value = serde_json::from_str(&raw).expect("VaultRegistry.json is valid JSON");
    let hex_with_prefix = json
        .get("bytecode")
        .and_then(|v| v.get("object"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("VaultRegistry.json missing bytecode.object"));
    let hex = hex_with_prefix.trim_start_matches("0x");
    let mut code =
        hex::decode(hex).unwrap_or_else(|e| panic!("VaultRegistry bytecode is not valid hex: {e}"));
    // ABI-encode `address admin`: left-pad to 32 bytes.
    let mut arg = [0u8; 32];
    arg[12..].copy_from_slice(admin.as_slice());
    code.extend_from_slice(&arg);
    Bytes::from(code)
}

// ── Event signature hashes ───────────────────────────────────────────────────

/// `keccak256("VaultRegistered(address,string,address)")` — the topic0
/// that VaultRegistry emits when a vault is registered.
fn vault_registered_topic0() -> alloy_primitives::B256 {
    keccak256(b"VaultRegistered(address,string,address)")
}

/// `keccak256("VaultStatusChanged(address,uint8,uint256)")` — the topic0
/// that VaultRegistry emits when a vault's status changes.
fn vault_status_changed_topic0() -> alloy_primitives::B256 {
    keccak256(b"VaultStatusChanged(address,uint8,uint256)")
}

// ── rmpc binary helpers ──────────────────────────────────────────────────────

/// Locate (and lazily build) the `rmpc` binary.
fn rmpc_bin() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let manifest = workspace_root().join("clients/rust-payment-client/Cargo.toml");
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "--bin",
                "rmpc",
                "--manifest-path",
                manifest.to_str().expect("manifest path utf-8"),
            ])
            .status()
            .expect("spawn cargo build rmpc");
        assert!(status.success(), "cargo build --bin rmpc failed");
        let bin = workspace_root().join("target/debug/rmpc");
        assert!(bin.exists(), "rmpc binary not at {bin:?} after build");
        bin
    })
}

/// Write a minimal `rmpc.toml` pointing at `rpc_url` with `registry_address`
/// set. The config references a non-existent keystore; read-only commands
/// (`get-vaults`) never open it.
fn write_rmpc_config(
    tmp: &tempfile::TempDir,
    rpc_url: &str,
    chain_id: u64,
    registry_addr: Address,
) -> PathBuf {
    let keystore = tmp.path().join("keystore.json");
    let cfg_path = tmp.path().join("rmpc.toml");
    let toml = format!(
        r#"chain_id              = {chain_id}
rpc_url               = "{rpc_url}"
gateway_address       = "0x000000000000000000000000000000000000dEaD"
usdc_address          = "0x{usdc_zeros}"
vault_address         = "0x{vault_zeros}"
registry_address      = "{registry:#x}"
gateway_runtime_hash  = "0x{zero_hash}"
max_fee_per_gas_cap   = 100000000000

[signer]
allow_software_fallback = true
keystore_path           = "{ks}"
"#,
        chain_id = chain_id,
        usdc_zeros = "00".repeat(20),
        vault_zeros = "00".repeat(20),
        registry = registry_addr,
        zero_hash = "0".repeat(64),
        ks = keystore.display(),
    );
    std::fs::write(&cfg_path, toml).expect("write rmpc.toml");
    cfg_path
}

/// Invoke `rmpc get-vaults --config <cfg>` and parse the JSON stdout.
/// Asserts exit 0.
fn run_get_vaults(cfg: &Path) -> Value {
    let out = Command::new(rmpc_bin())
        .args(["get-vaults", "--config", cfg.to_str().unwrap()])
        .output()
        .expect("spawn rmpc get-vaults");
    assert!(
        out.status.success(),
        "rmpc get-vaults exited {:?}; stderr=\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "rmpc get-vaults stdout is not valid JSON: {e}\nstdout=\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Scenario 1: deploy the registry, register a vault, assert `VaultRegistered`
/// event, and verify `listVaults` returns the vault address.
/// Then run `rmpc get-vaults` and assert the vault appears in the JSON output.
#[test]
fn registry_register_list() {
    skip_if_no_fork!();
    let fx = ForkFixture::new().expect("boot fork");
    eprintln!("[registry_register_list] {}", fx.summary_line());

    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    let deployer = fx
        .ephemeral(one_eth * U256::from(3u64), U256::ZERO)
        .expect("fund deployer");

    // Deploy VaultRegistry with deployer as admin.
    let initcode = vault_registry_initcode(deployer.address);
    let registry_addr = deployer
        .deploy(initcode, 3_000_000)
        .expect("deploy VaultRegistry");
    eprintln!("[registry_register_list] registry deployed at {registry_addr:#x}");

    // Use a stable fake vault address — just a non-zero address. VaultRegistry
    // never calls the vault contract during registration, so no code is needed
    // at this address.
    let fake_vault: Address = "0x0000000000000000000000000000000000000001"
        .parse()
        .unwrap();

    // registerVault(fake_vault, VaultMetadata{name, asset, registeredAt})
    let meta = IOnchainVaultRegistry::VaultMetadata {
        name: "Robot Money USDC".to_string(),
        asset: rmpc_fork_e2e::addresses::USDC,
        registeredAt: U256::ZERO, // overwritten by contract (block.timestamp)
    };
    let register_call = IOnchainVaultRegistry::registerVaultCall {
        vault: fake_vault,
        metadata: meta,
    };
    let receipt = deployer
        .send(registry_addr, &register_call, U256::ZERO, 500_000)
        .expect("registerVault");
    assert_eq!(receipt.status, 1, "registerVault must succeed");
    eprintln!(
        "[registry_register_list] registerVault tx {:?} gasUsed={}",
        receipt.tx_hash, receipt.gas_used
    );

    // Assert VaultRegistered event is present in receipt logs.
    let expected_topic0 = vault_registered_topic0();
    let vault_registered_log = receipt
        .logs
        .iter()
        .find(|log| log.topics.first() == Some(&expected_topic0));
    assert!(
        vault_registered_log.is_some(),
        "VaultRegistered event not found in receipt logs; logs={:?}",
        receipt.logs
    );
    let log = vault_registered_log.unwrap();
    assert_eq!(
        log.address, registry_addr,
        "VaultRegistered emitted from wrong address"
    );
    // topic1 = indexed vault address (padded to 32 bytes, address in low 20 bytes).
    assert!(
        log.topics.len() >= 2,
        "VaultRegistered log must have at least 2 topics (sig + vault)"
    );
    let vault_from_topic = Address::from_slice(&log.topics[1].as_slice()[12..]);
    assert_eq!(
        vault_from_topic, fake_vault,
        "VaultRegistered topic1 (vault address) mismatch"
    );

    // listVaults() — verify the vault is present.
    // Retry loop: on Geth devnet there is a brief window after wait_for_receipt
    // returns where eth_call "latest" may still reflect pre-TX state and return
    // 0 vaults or undecodable data (blocker #1081). Poll up to 5 times with
    // 200 ms backoff before failing.
    let list_call = IOnchainVaultRegistry::listVaultsCall {};
    let vaults = {
        let mut last_err = String::new();
        let mut result = None;
        for attempt in 0..5u32 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            match deployer.call(registry_addr, &list_call) {
                Err(e) => {
                    last_err = format!("listVaults attempt {attempt}: {e}");
                    eprintln!("[registry_register_list] {last_err}");
                }
                Ok(raw) => {
                    match IOnchainVaultRegistry::listVaultsCall::abi_decode_returns(&raw, true) {
                        Ok(decoded) if !decoded._0.is_empty() => {
                            result = Some(decoded._0);
                            break;
                        }
                        Ok(_) => {
                            last_err = format!(
                                "listVaults attempt {attempt}: returned empty (state not yet visible)"
                            );
                            eprintln!("[registry_register_list] {last_err}");
                        }
                        Err(e) => {
                            last_err = format!("listVaults attempt {attempt}: decode: {e}");
                            eprintln!("[registry_register_list] {last_err}");
                        }
                    }
                }
            }
        }
        result.unwrap_or_else(|| {
            panic!("listVaults never returned vault after registerVault: {last_err}")
        })
    };
    assert_eq!(
        vaults.len(),
        1,
        "expected exactly one vault after registration"
    );
    assert_eq!(
        vaults[0], fake_vault,
        "listVaults returned wrong vault address"
    );

    // rmpc get-vaults round-trip: assert exit 0, vault appears in `data.vaults`.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let cfg = write_rmpc_config(&tmp, &fx.rpc_url, fx.chain_id, registry_addr);
    let v = run_get_vaults(&cfg);

    let vaults_json = v["data"]["vaults"]
        .as_array()
        .expect("data.vaults must be a JSON array");
    let entry = vaults_json
        .iter()
        .find(|e| {
            e["address"]
                .as_str()
                .map(|s| s.eq_ignore_ascii_case(&format!("{fake_vault:#x}")))
                .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            panic!(
                "rmpc get-vaults: vault {fake_vault:#x} not found in data.vaults; envelope:\n{v:#}"
            )
        });

    // These four come from decoding the live `getVault` response, so they are
    // what discriminates a working ABI binding from a drifted one (issue
    // #1362). `address` alone does not: it comes from `listVaults()` and is
    // filled in even when the `getVault` decode fails outright.
    assert_eq!(
        entry["name"], "Robot Money USDC",
        "rmpc get-vaults did not decode VaultMetadata.name from the live \
         registry — abi/VaultRegistry.json has drifted from VaultRegistry.sol \
         again (issue #1362); envelope:\n{v:#}"
    );
    assert_eq!(
        entry["asset"]
            .as_str()
            .expect("asset must be a string")
            .to_lowercase(),
        format!("{:#x}", rmpc_fork_e2e::addresses::USDC),
        "rmpc get-vaults decoded the wrong VaultMetadata.asset; envelope:\n{v:#}"
    );
    assert_eq!(
        entry["status"], "active",
        "a freshly registered vault must decode as active; envelope:\n{v:#}"
    );
    assert!(
        entry["registered_at"].as_u64().unwrap_or(0) > 0,
        "VaultMetadata.registeredAt must decode to the registration block \
         timestamp, not 0; envelope:\n{v:#}"
    );
    eprintln!("[registry_register_list] rmpc get-vaults passed");
}

/// Scenario 2: register a vault then change its status to Paused.
/// Assert the `VaultStatusChanged` event and verify `getVault` returns
/// the updated status. Then verify `rmpc get-vaults` still exits 0 with
/// the vault present in the output.
#[test]
fn registry_status_change() {
    skip_if_no_fork!();
    let fx = ForkFixture::new().expect("boot fork");
    eprintln!("[registry_status_change] {}", fx.summary_line());

    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    let deployer = fx
        .ephemeral(one_eth * U256::from(3u64), U256::ZERO)
        .expect("fund deployer");

    // Deploy and register.
    let initcode = vault_registry_initcode(deployer.address);
    let registry_addr = deployer
        .deploy(initcode, 3_000_000)
        .expect("deploy VaultRegistry");

    let fake_vault: Address = "0x0000000000000000000000000000000000000002"
        .parse()
        .unwrap();
    let meta = IOnchainVaultRegistry::VaultMetadata {
        name: "Test Vault".to_string(),
        asset: rmpc_fork_e2e::addresses::USDC,
        registeredAt: U256::ZERO,
    };
    deployer
        .send(
            registry_addr,
            &IOnchainVaultRegistry::registerVaultCall {
                vault: fake_vault,
                metadata: meta,
            },
            U256::ZERO,
            500_000,
        )
        .expect("registerVault");

    // Verify initial status via getVault — status 0 = Active.
    // Retry loop: on Geth devnet there is a brief window after wait_for_receipt
    // returns where eth_call "latest" may still reflect pre-TX state, causing
    // getVault to revert with NotRegistered() (same class of race as blocker
    // #1081, closed for listVaults in registry_register_list). Poll up to 5
    // times with 200 ms backoff before failing.
    let get_call = IOnchainVaultRegistry::getVaultCall { vault: fake_vault };
    let decoded = {
        let mut last_err = String::new();
        let mut result = None;
        for attempt in 0..5u32 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            match deployer.call(registry_addr, &get_call) {
                Err(e) => {
                    last_err = format!("getVault before pause attempt {attempt}: {e}");
                    eprintln!("[registry_status_change] {last_err}");
                }
                Ok(raw) => {
                    match IOnchainVaultRegistry::getVaultCall::abi_decode_returns(&raw, true) {
                        Ok(decoded) => {
                            result = Some(decoded);
                            break;
                        }
                        Err(e) => {
                            last_err =
                                format!("getVault before pause attempt {attempt}: decode: {e}");
                            eprintln!("[registry_status_change] {last_err}");
                        }
                    }
                }
            }
        }
        result.unwrap_or_else(|| {
            panic!("getVault before pause never succeeded after registerVault: {last_err}")
        })
    };
    assert_eq!(
        decoded.status as u8, 0u8,
        "initial status must be Active (0)"
    );

    // setVaultStatus(fake_vault, Paused=1).
    let pause_call = IOnchainVaultRegistry::setVaultStatusCall {
        vault: fake_vault,
        newStatus: IOnchainVaultRegistry::VaultStatus::Paused,
    };
    let pause_receipt = deployer
        .send(registry_addr, &pause_call, U256::ZERO, 200_000)
        .expect("setVaultStatus Paused");
    assert_eq!(pause_receipt.status, 1, "setVaultStatus must succeed");

    // Assert VaultStatusChanged event.
    let expected_topic0 = vault_status_changed_topic0();
    let status_log = pause_receipt
        .logs
        .iter()
        .find(|log| log.topics.first() == Some(&expected_topic0));
    assert!(
        status_log.is_some(),
        "VaultStatusChanged event not found in receipt logs; logs={:?}",
        pause_receipt.logs
    );
    let log = status_log.unwrap();
    assert_eq!(
        log.address, registry_addr,
        "VaultStatusChanged emitted from wrong address"
    );
    // topic1 = indexed vault, topic2 = indexed newStatus (uint8 padded to 32 bytes).
    assert!(
        log.topics.len() >= 3,
        "VaultStatusChanged log must have 3 topics (sig + vault + newStatus)"
    );
    let vault_from_topic = Address::from_slice(&log.topics[1].as_slice()[12..]);
    assert_eq!(
        vault_from_topic, fake_vault,
        "VaultStatusChanged topic1 vault mismatch"
    );
    // newStatus=1 (Paused) is in topic2.
    let status_from_topic = log.topics[2].as_slice()[31];
    assert_eq!(
        status_from_topic, 1u8,
        "VaultStatusChanged topic2 newStatus must be 1 (Paused)"
    );

    // Verify getVault now returns Paused (status=1).
    // Same Geth eth_call/"latest" visibility race as the pre-pause read above:
    // retry until the decoded status reflects the just-mined setVaultStatus TX
    // rather than failing on a stale-but-decodable pre-TX read.
    let decoded2 = {
        let mut last_err = String::new();
        let mut result = None;
        for attempt in 0..5u32 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            match deployer.call(registry_addr, &get_call) {
                Err(e) => {
                    last_err = format!("getVault after pause attempt {attempt}: {e}");
                    eprintln!("[registry_status_change] {last_err}");
                }
                Ok(raw) => {
                    match IOnchainVaultRegistry::getVaultCall::abi_decode_returns(&raw, true) {
                        Ok(decoded) if decoded.status as u8 == 1u8 => {
                            result = Some(decoded);
                            break;
                        }
                        Ok(decoded) => {
                            last_err = format!(
                                "getVault after pause attempt {attempt}: status={} (not yet Paused)",
                                decoded.status as u8
                            );
                            eprintln!("[registry_status_change] {last_err}");
                        }
                        Err(e) => {
                            last_err =
                                format!("getVault after pause attempt {attempt}: decode: {e}");
                            eprintln!("[registry_status_change] {last_err}");
                        }
                    }
                }
            }
        }
        result.unwrap_or_else(|| {
            panic!("getVault after pause never reflected Paused status after setVaultStatus: {last_err}")
        })
    };
    assert_eq!(
        decoded2.status as u8, 1u8,
        "status must be Paused (1) after setVaultStatus"
    );

    // rmpc get-vaults still exits 0 and the vault appears in output.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let cfg = write_rmpc_config(&tmp, &fx.rpc_url, fx.chain_id, registry_addr);
    let v = run_get_vaults(&cfg);
    let vaults_json = v["data"]["vaults"]
        .as_array()
        .expect("data.vaults must be a JSON array");
    assert_eq!(
        vaults_json.len(),
        1,
        "rmpc get-vaults must report one vault after status change; envelope:\n{v:#}"
    );
    let vault_addr_in_output = vaults_json[0]["address"]
        .as_str()
        .unwrap_or("")
        .to_lowercase();
    assert_eq!(
        vault_addr_in_output,
        format!("{fake_vault:#x}"),
        "rmpc get-vaults vault address mismatch after status change"
    );
    // The status the CLI reports must be the one the registry now holds — this
    // is the second live-chain assertion on a `getVault`-decoded field (issue
    // #1362), and the only one that also covers the status output, which is
    // `getVault`'s *second* top-level return value rather than a struct member.
    assert_eq!(
        vaults_json[0]["status"], "paused",
        "rmpc get-vaults must report the paused status it just read from the \
         live registry; envelope:\n{v:#}"
    );
    assert_eq!(
        vaults_json[0]["name"], "Test Vault",
        "rmpc get-vaults must decode VaultMetadata.name after a status change; \
         envelope:\n{v:#}"
    );

    eprintln!("[registry_status_change] passed");
}

/// Scenario 3: empty registry — before any registration, `listVaults` returns
/// an empty array, and `rmpc get-vaults` exits 0 with `vaults: []`.
/// This validates the zero-vault edge case described in the acceptance criteria.
#[test]
fn registry_empty_list() {
    skip_if_no_fork!();
    let fx = ForkFixture::new().expect("boot fork");
    eprintln!("[registry_empty_list] {}", fx.summary_line());

    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    let deployer = fx
        .ephemeral(one_eth * U256::from(3u64), U256::ZERO)
        .expect("fund deployer");

    // Deploy a fresh registry — no vaults registered.
    let initcode = vault_registry_initcode(deployer.address);
    let registry_addr = deployer
        .deploy(initcode, 3_000_000)
        .expect("deploy VaultRegistry");
    eprintln!("[registry_empty_list] registry deployed at {registry_addr:#x}");

    // listVaults() must return an empty array.
    // Retry loop: on Geth devnet there is a brief window after wait_for_receipt
    // returns where eth_call "latest" may still reflect pre-deploy state and
    // return undecodable/empty data ("Overrun"). Poll up to 5 times with 200 ms
    // backoff before failing.
    let list_call = IOnchainVaultRegistry::listVaultsCall {};
    let decoded = {
        let mut last_err = String::new();
        let mut result = None;
        for attempt in 0..5u32 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            match deployer.call(registry_addr, &list_call) {
                Err(e) => {
                    last_err = format!("eth_call attempt {attempt}: {e}");
                    eprintln!("[registry_empty_list] {last_err}");
                }
                Ok(raw) => {
                    match IOnchainVaultRegistry::listVaultsCall::abi_decode_returns(&raw, true) {
                        Ok(decoded) => {
                            result = Some(decoded);
                            break;
                        }
                        Err(e) => {
                            last_err =
                                format!("decode attempt {attempt}: {e} (raw={} bytes)", raw.len());
                            eprintln!("[registry_empty_list] {last_err}");
                        }
                    }
                }
            }
        }
        result.unwrap_or_else(|| panic!("decode listVaults returns: {last_err}"))
    };
    assert!(
        decoded._0.is_empty(),
        "listVaults on empty registry must return []"
    );

    // rmpc get-vaults must exit 0 with data.vaults = [].
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let cfg = write_rmpc_config(&tmp, &fx.rpc_url, fx.chain_id, registry_addr);
    let v = run_get_vaults(&cfg);

    assert_eq!(v["source"], "json_rpc", "source must be json_rpc: {v}");
    assert_eq!(
        v["partial"], false,
        "empty registry must yield partial=false: {v}"
    );
    let vaults_json = v["data"]["vaults"]
        .as_array()
        .expect("data.vaults must be a JSON array");
    assert!(
        vaults_json.is_empty(),
        "rmpc get-vaults on empty registry must return vaults=[]; got {v:#}"
    );
    eprintln!("[registry_empty_list] rmpc get-vaults vaults=[] confirmed");
}
