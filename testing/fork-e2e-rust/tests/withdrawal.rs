//! Canonical: docs/architecture.md §5 — On-Chain Gateway
//! Implements: issue #313
//!
//! Fork e2e scenarios for agent gateway withdrawal via `RobotMoneyGateway.withdraw()`.
//!
//! Covered scenarios:
//!
//! - `agent_withdrawal_happy_path` — authorize an agent with withdrawal caps,
//!   deposit USDC into the vault so the share-receiver holds shares, approve
//!   the gateway to pull the shares, call `gateway.withdraw(orderId, shares,
//!   sourceVault, deadline, idempotencyKey)`, and assert USDC lands in
//!   `assetRecipient` and `AgentWithdrawal` event is emitted.
//!
//! - `agent_withdrawal_redirect_blocked` — attempt to pass a different recipient
//!   address as calldata. The `withdraw(bytes32,uint256,address,uint64,bytes32)` API
//!   does not accept a recipient — the recipient is always the policy-configured
//!   `assetRecipient`. Verify that an agent cannot redirect proceeds by constructing
//!   raw calldata for a hypothetical `withdraw(uint256,address)` overload (which
//!   does not exist). The gateway must revert because the selector is unknown.
//!
//! - `agent_withdrawal_window_cap` — make a first withdrawal that consumes the full
//!   per-window cap, then attempt a second withdrawal in the same window and assert
//!   it reverts with `WithdrawWindowCapExceeded`.
//!
//! Each test boots its own anvil fork backend (per ADR §3.5), providing
//! full state isolation without snapshot/revert coordination.

use std::path::PathBuf;

use alloy_primitives::{keccak256, Address, Bytes, U256};
use alloy_sol_types::sol;
use rmpc_fork_e2e::{skip_if_no_fork, ForkFixture};
use serde_json::Value;

// ── Workspace root helper ────────────────────────────────────────────────────

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // testing/fork-e2e-rust → testing → repo root
    p.pop();
    p.pop();
    p
}

// ── Foundry artifact loader ──────────────────────────────────────────────────

fn load_initcode(sol_file: &str, contract_name: &str) -> Bytes {
    let artifact_path = workspace_root()
        .join("out")
        .join(sol_file)
        .join(format!("{contract_name}.json"));
    let raw = std::fs::read_to_string(&artifact_path).unwrap_or_else(|e| {
        panic!(
            "Cannot read Foundry artefact at {}; run `forge build` first: {e}",
            artifact_path.display()
        )
    });
    let json: Value = serde_json::from_str(&raw).expect("artefact is valid JSON");
    let hex_with_prefix = json
        .get("bytecode")
        .and_then(|v| v.get("object"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("{contract_name}.json missing bytecode.object"));
    let hex = hex_with_prefix.trim_start_matches("0x");
    Bytes::from(
        hex::decode(hex)
            .unwrap_or_else(|e| panic!("{contract_name} bytecode is not valid hex: {e}")),
    )
}

fn encode_address_arg(addr: Address) -> [u8; 32] {
    let mut arg = [0u8; 32];
    arg[12..].copy_from_slice(addr.as_slice());
    arg
}

// ── ABI bindings ─────────────────────────────────────────────────────────────

sol! {
    /// USDC (ERC-20) minimal interface.
    #[allow(missing_docs)]
    interface IUSDC {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }

    /// MockVault minimal interface — 1:1 ERC-4626 used in fork tests.
    #[allow(missing_docs)]
    interface IMockVault {
        function deposit(uint256 assets, address receiver) external returns (uint256 shares);
        function balanceOf(address account) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function redeem(uint256 shares, address receiver, address owner) external returns (uint256 assets);
        function asset() external view returns (address);
    }

    /// Gateway interface — subset used by withdrawal tests.
    #[allow(missing_docs)]
    interface IGateway {
        struct AgentPolicy {
            bool active;
            uint64 validUntil;
            uint256 maxPerPayment;
            uint256 maxPerWindow;
            address shareReceiver;
            address[] allowedDestinations;
            address assetRecipient;
            uint256 maxWithdrawPerPayment;
            uint256 maxWithdrawPerWindow;
            address[] allowedSourceVaults;
        }

        function authorizeAgent(address agent, AgentPolicy calldata p) external;

        /// Full withdraw() signature as merged in issue #311.
        function withdraw(
            bytes32 orderId,
            uint256 shares,
            address sourceVault,
            uint64 deadline,
            bytes32 idempotencyKey
        ) external returns (bytes32 paymentId, uint256 assetsOut);

        function deposit(bytes32 orderId, uint256 amount, uint64 deadline, bytes32 idempotencyKey)
            external returns (bytes32 paymentId, uint256 sharesMinted);

        function depositTo(
            bytes32 orderId,
            uint256 amount,
            uint64 deadline,
            bytes32 idempotencyKey,
            address destination,
            uint256[] calldata minSharesPerLeg
        ) external returns (bytes32 paymentId);

        /// Issue #967: redeem legs are driven by an explicit, caller-supplied
        /// `vaults[]` (identity-bound to `sharesPerLeg`), not the router's live
        /// weight vector. Issue #969 added the real per-leg `minAssetsPerLeg`
        /// slippage floor (parallel to `sharesPerLeg`).
        function withdrawFromRouter(
            bytes32 orderId,
            address[] calldata vaults,
            uint256[] calldata sharesPerLeg,
            uint256[] calldata minAssetsPerLeg,
            uint64 deadline,
            bytes32 idempotencyKey
        ) external returns (bytes32 paymentId, uint256[] memory assetsPerLeg);

        // Issue #449: rolling-window withdrawal accounting.
        function effectiveWithdrawWindowGross(address agent)
            external view returns (uint256);
    }

    /// VaultRegistry minimal interface for router withdrawal tests.
    #[allow(missing_docs)]
    interface IVaultRegistry {
        enum VaultStatus { Active, Paused, Retired }
        struct VaultMetadata { string name; address asset; uint256 registeredAt; }
        function registerVault(address vault, VaultMetadata calldata metadata) external;
        function setRouterEligible(address vault, bool eligible) external;
    }

    /// PortfolioRouter minimal interface for router withdrawal tests.
    #[allow(missing_docs)]
    interface IPortfolioRouter {
        function setWeights(address[] calldata vaults, uint256[] calldata bps) external;
        function depositFor(address receiver, uint256 amount, uint256[] calldata minSharesPerLeg)
            external returns (uint256[] memory sharesPerLeg);
        function getEffectiveWeights()
            external view returns (address[] memory vaults, uint256[] memory bps);
    }
}

// ── Event signature helpers ──────────────────────────────────────────────────

/// `AgentWithdrawal(bytes32,bytes32,address,address,uint256,uint256,address,uint64)` — topic0.
///
/// Event: `AgentWithdrawal(bytes32 indexed paymentId, bytes32 indexed orderId,
///         address indexed agent, address sourceVault, uint256 shares,
///         uint256 assetsOut, address assetRecipient, uint64 windowId)`
fn agent_withdrawal_topic0() -> alloy_primitives::B256 {
    keccak256(b"AgentWithdrawal(bytes32,bytes32,address,address,uint256,uint256,address,uint64)")
}

// ── Contract deployment helpers ──────────────────────────────────────────────

/// Deploy `MockVault` with `asset_` = USDC address.
fn deploy_mock_vault(deployer: &rmpc_fork_e2e::Account<'_>, usdc: Address) -> Address {
    let mut code = load_initcode("MockVault.sol", "MockVault").to_vec();
    code.extend_from_slice(&encode_address_arg(usdc));
    deployer
        .deploy(Bytes::from(code), 2_000_000)
        .expect("deploy MockVault")
}

/// Deploy `RobotMoneyGateway` (no router).
fn deploy_gateway(
    deployer: &rmpc_fork_e2e::Account<'_>,
    usdc: Address,
    vault: Address,
    admin: Address,
    pauser: Address,
) -> Address {
    let mut code = load_initcode("RobotMoneyGateway.sol", "RobotMoneyGateway").to_vec();
    // 5 × address args: usdc, vault, admin, pauser, router=address(0)
    let mut args = [0u8; 160];
    args[12..32].copy_from_slice(usdc.as_slice());
    args[44..64].copy_from_slice(vault.as_slice());
    args[76..96].copy_from_slice(admin.as_slice());
    args[108..128].copy_from_slice(pauser.as_slice());
    // router = address(0) — slot 128..160 stays zero
    code.extend_from_slice(&args);
    // Gas limit bumped from 5_000_000 → 8_000_000 to accommodate the
    // committee routing additions in issue #1049 (deployment cost ~5.15M).
    deployer
        .deploy(Bytes::from(code), 8_000_000)
        .expect("deploy RobotMoneyGateway")
}

/// Approve `spender` to pull `amount` USDC from `account`.
fn approve_usdc(
    account: &rmpc_fork_e2e::Account<'_>,
    usdc: Address,
    spender: Address,
    amount: U256,
) {
    account
        .send(
            usdc,
            &IUSDC::approveCall { spender, amount },
            U256::ZERO,
            100_000,
        )
        .expect("USDC.approve");
}

/// Read `USDC.balanceOf(addr)` from `caller`'s perspective.
fn usdc_balance_of(caller: &rmpc_fork_e2e::Account<'_>, usdc: Address, addr: Address) -> U256 {
    balance_of_with_retry("USDC.balanceOf", || {
        caller.call(usdc, &IUSDC::balanceOfCall { account: addr })
    })
}

/// Read `vault.balanceOf(addr)` from `caller`'s perspective.
fn vault_balance_of(caller: &rmpc_fork_e2e::Account<'_>, vault: Address, addr: Address) -> U256 {
    balance_of_with_retry("vault.balanceOf", || {
        caller.call(vault, &IMockVault::balanceOfCall { account: addr })
    })
}

/// Read a balance after a transaction receipt, tolerating Geth devnet's brief
/// post-receipt `eth_call("latest")` visibility window (blocker #1081).
///
/// A short response is not a zero balance: it is an unreadable response. Keep
/// retrying it as a possible transient visibility result, then fail loudly if
/// the node never returns a complete ABI word.
fn balance_of_with_retry<F>(method: &str, mut call: F) -> U256
where
    F: FnMut() -> Result<alloy_primitives::Bytes, rmpc_fork_e2e::HarnessError>,
{
    let mut last_error = String::new();
    for attempt in 0..5u32 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        match call() {
            Err(error) => {
                last_error = format!("{method} attempt {attempt}: {error}");
                eprintln!("[{method}] {last_error}");
            }
            Ok(raw) if raw.len() >= 32 => return U256::from_be_slice(&raw[..32]),
            Ok(raw) => {
                last_error = format!(
                    "{method} attempt {attempt}: returned {} bytes, expected at least 32",
                    raw.len()
                );
                eprintln!("[{method}] {last_error}");
            }
        }
    }

    panic!("{method} unreadable after 5 attempts: {last_error}");
}

#[test]
#[should_panic(expected = "unreadable after 5 attempts")]
fn short_balance_of_response_is_not_coerced_to_zero() {
    balance_of_with_retry("vault.balanceOf", || Ok(Bytes::new()));
}

/// Approve `spender` to pull `amount` vault shares from `account`.
fn approve_vault_shares(
    account: &rmpc_fork_e2e::Account<'_>,
    vault: Address,
    spender: Address,
    amount: U256,
) {
    account
        .send(
            vault,
            &IMockVault::approveCall { spender, amount },
            U256::ZERO,
            100_000,
        )
        .expect("vault.approve");
}

// ── Scenario 1: happy path ────────────────────────────────────────────────────

/// agent_withdrawal_happy_path
///
/// 1. Deploy MockVault + Gateway.
/// 2. Authorize an agent with withdrawal caps and a designated `assetRecipient`.
/// 3. Deposit USDC into the vault via gateway.deposit() so shareReceiver holds shares.
/// 4. shareReceiver approves gateway to transfer their shares.
/// 5. Agent calls gateway.withdraw(orderId, shares, sourceVault, deadline, idempotencyKey).
/// 6. Assert: USDC lands in `assetRecipient`, `AgentWithdrawal` event emitted with
///    correct fields (topic1=paymentId, topic2=orderId, topic3=agent).
#[test]
fn agent_withdrawal_happy_path() {
    skip_if_no_fork!();
    let fx = ForkFixture::new().expect("boot fork");
    eprintln!("[agent_withdrawal_happy_path] {}", fx.summary_line());

    let usdc = rmpc_fork_e2e::addresses::USDC;
    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    let deposit_amount = U256::from(100_000_000u64); // 100 USDC

    // Three roles: deployer/admin (deploys + authorizes), pauser (holds PAUSER_ROLE),
    // agent (holds AGENT_ROLE — deposits USDC and later withdraws shares).
    let admin = fx
        .ephemeral(one_eth * U256::from(3u64), U256::ZERO)
        .expect("fund admin");
    let pauser = fx.ephemeral(one_eth, U256::ZERO).expect("fund pauser");
    // Agent is funded with USDC; it will deposit and later withdraw.
    let agent = fx
        .ephemeral(one_eth * U256::from(2u64), deposit_amount)
        .expect("fund agent with USDC");
    // assetRecipient — the address that receives USDC on withdrawal.
    let asset_recipient_addr: Address = "0x000000000000000000000000000000000000BEEF"
        .parse()
        .unwrap();

    // Deploy vault and gateway.
    let vault = deploy_mock_vault(&admin, usdc);
    let gateway = deploy_gateway(&admin, usdc, vault, admin.address, pauser.address);

    eprintln!(
        "[agent_withdrawal_happy_path] vault={vault:#x} gateway={gateway:#x} agent={:#x} assetRecipient={:#x}",
        agent.address, asset_recipient_addr
    );

    // Authorize agent: shareReceiver = agent.address (shares land with the agent),
    // assetRecipient = asset_recipient_addr (USDC on withdrawal goes there).
    let now_secs: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let policy = IGateway::AgentPolicy {
        active: true,
        validUntil: now_secs + 3600,
        maxPerPayment: deposit_amount,
        maxPerWindow: deposit_amount,
        shareReceiver: agent.address, // shares land with agent
        allowedDestinations: vec![],
        assetRecipient: asset_recipient_addr,
        maxWithdrawPerPayment: deposit_amount,
        maxWithdrawPerWindow: deposit_amount,
        allowedSourceVaults: vec![],
    };
    admin
        .send(
            gateway,
            &IGateway::authorizeAgentCall {
                agent: agent.address,
                p: policy,
            },
            U256::ZERO,
            500_000,
        )
        .expect("authorizeAgent");

    // Agent deposits USDC via gateway.deposit() → shares land at agent.address.
    approve_usdc(&agent, usdc, gateway, deposit_amount);
    let deadline = now_secs + 300;
    let deposit_receipt = agent
        .send(
            gateway,
            &IGateway::depositCall {
                orderId: alloy_primitives::B256::from([1u8; 32]),
                amount: deposit_amount,
                deadline,
                idempotencyKey: alloy_primitives::B256::from([2u8; 32]),
            },
            U256::ZERO,
            800_000,
        )
        .expect("gateway.deposit");
    assert_eq!(deposit_receipt.status, 1, "deposit must succeed");

    // Agent now holds shares (shareReceiver = agent.address). Approve gateway to pull them.
    let shares_held = vault_balance_of(&agent, vault, agent.address);
    assert!(
        shares_held > U256::ZERO,
        "agent must hold vault shares after deposit"
    );
    approve_vault_shares(&agent, vault, gateway, shares_held);

    // Pre-withdrawal: assetRecipient has zero USDC.
    let pre_bal = usdc_balance_of(&agent, usdc, asset_recipient_addr);
    assert_eq!(pre_bal, U256::ZERO, "assetRecipient must start with 0 USDC");

    // Agent calls gateway.withdraw(orderId, shares_held, vault, deadline, idempotencyKey).
    // The gateway pulls shares from the agent (transferFrom(agent → gateway)) and
    // redeems them, sending USDC to assetRecipient.
    let withdraw_deadline = now_secs + 300;
    let withdraw_receipt = agent
        .send(
            gateway,
            &IGateway::withdrawCall {
                orderId: alloy_primitives::B256::from([10u8; 32]),
                shares: shares_held,
                sourceVault: vault,
                deadline: withdraw_deadline,
                idempotencyKey: alloy_primitives::B256::from([11u8; 32]),
            },
            U256::ZERO,
            800_000,
        )
        .expect("gateway.withdraw");
    assert_eq!(withdraw_receipt.status, 1, "withdraw must succeed");
    eprintln!(
        "[agent_withdrawal_happy_path] withdraw tx {:?} gasUsed={}",
        withdraw_receipt.tx_hash, withdraw_receipt.gas_used
    );

    // Assert USDC landed in assetRecipient (MockVault is 1:1, so assetsOut == shares_held).
    let post_bal = usdc_balance_of(&agent, usdc, asset_recipient_addr);
    assert_eq!(
        post_bal, shares_held,
        "assetRecipient must receive USDC equal to redeemed shares (1:1 MockVault)"
    );

    // Assert AgentWithdrawal event is present in the receipt.
    // Event: AgentWithdrawal(bytes32 indexed paymentId, bytes32 indexed orderId,
    //         address indexed agent, address sourceVault, uint256 shares,
    //         uint256 assetsOut, address assetRecipient, uint64 windowId)
    // topic0 = event sig, topic1 = paymentId, topic2 = orderId, topic3 = agent
    let topic0 = agent_withdrawal_topic0();
    let withdrawal_log = withdraw_receipt
        .logs
        .iter()
        .find(|l| l.topics.first() == Some(&topic0) && l.address == gateway);
    assert!(
        withdrawal_log.is_some(),
        "AgentWithdrawal event not found in receipt logs; logs={:?}",
        withdraw_receipt.logs
    );
    let log = withdrawal_log.unwrap();
    // topic1 = indexed paymentId (bytes32), topic2 = indexed orderId (bytes32),
    // topic3 = indexed agent (address).
    assert!(
        log.topics.len() >= 4,
        "AgentWithdrawal must have at least 4 topics (sig + paymentId + orderId + agent)"
    );
    // topic3 = agent address (right-aligned in 32 bytes)
    let agent_from_log = Address::from_slice(&log.topics[3].as_slice()[12..]);
    assert_eq!(
        agent_from_log, agent.address,
        "AgentWithdrawal topic3 (agent) mismatch"
    );

    // Assert agent's vault shares are now zero.
    let shares_after = vault_balance_of(&agent, vault, agent.address);
    assert_eq!(
        shares_after,
        U256::ZERO,
        "agent must hold 0 shares after withdrawal"
    );

    eprintln!("[agent_withdrawal_happy_path] passed");
}

// ── Scenario 2: redirect blocked ─────────────────────────────────────────────

/// agent_withdrawal_redirect_blocked
///
/// The `withdraw(bytes32,uint256,address,uint64,bytes32)` function does not
/// accept a recipient parameter — the recipient is always the policy-configured
/// `assetRecipient`. This scenario verifies that an attacker-controlled address
/// cannot be passed as a recipient by constructing raw calldata for a hypothetical
/// `withdraw(uint256,address)` overload (which does not exist). The gateway must
/// revert because the selector is unknown.
///
/// This tests the "no redirect path exists" property: the only way to call the
/// gateway's withdrawal path is via the full `withdraw(...)` signature, which
/// routes proceeds exclusively to `assetRecipient`.
#[test]
fn agent_withdrawal_redirect_blocked() {
    skip_if_no_fork!();
    let fx = ForkFixture::new().expect("boot fork");
    eprintln!("[agent_withdrawal_redirect_blocked] {}", fx.summary_line());

    let usdc = rmpc_fork_e2e::addresses::USDC;
    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    let deposit_amount = U256::from(100_000_000u64); // 100 USDC

    let admin = fx
        .ephemeral(one_eth * U256::from(3u64), U256::ZERO)
        .expect("fund admin");
    let pauser = fx.ephemeral(one_eth, U256::ZERO).expect("fund pauser");
    let agent = fx
        .ephemeral(one_eth * U256::from(2u64), deposit_amount)
        .expect("fund agent with USDC");
    let attacker_addr: Address = "0x000000000000000000000000000000000000DEAD"
        .parse()
        .unwrap();
    let asset_recipient_addr: Address = "0x000000000000000000000000000000000000BEEF"
        .parse()
        .unwrap();

    let vault = deploy_mock_vault(&admin, usdc);
    let gateway = deploy_gateway(&admin, usdc, vault, admin.address, pauser.address);

    let now_secs: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let policy = IGateway::AgentPolicy {
        active: true,
        validUntil: now_secs + 3600,
        maxPerPayment: deposit_amount,
        maxPerWindow: deposit_amount,
        shareReceiver: agent.address,
        allowedDestinations: vec![],
        assetRecipient: asset_recipient_addr,
        maxWithdrawPerPayment: deposit_amount,
        maxWithdrawPerWindow: deposit_amount,
        allowedSourceVaults: vec![],
    };
    admin
        .send(
            gateway,
            &IGateway::authorizeAgentCall {
                agent: agent.address,
                p: policy,
            },
            U256::ZERO,
            500_000,
        )
        .expect("authorizeAgent");

    // Deposit so agent holds shares.
    approve_usdc(&agent, usdc, gateway, deposit_amount);
    let deadline = now_secs + 300;
    agent
        .send(
            gateway,
            &IGateway::depositCall {
                orderId: alloy_primitives::B256::from([3u8; 32]),
                amount: deposit_amount,
                deadline,
                idempotencyKey: alloy_primitives::B256::from([4u8; 32]),
            },
            U256::ZERO,
            800_000,
        )
        .expect("gateway.deposit");

    let shares = vault_balance_of(&agent, vault, agent.address);
    approve_vault_shares(&agent, vault, gateway, shares);

    // Construct `withdraw(uint256,address)` calldata — a nonexistent overload.
    // selector = keccak256("withdraw(uint256,address)")[..4]
    let fake_selector = &keccak256(b"withdraw(uint256,address)")[..4];
    let mut calldata = fake_selector.to_vec();
    // shares: 32 bytes
    calldata.extend_from_slice(&shares.to_be_bytes::<32>());
    // attacker's address: padded to 32 bytes
    let mut addr_word = [0u8; 32];
    addr_word[12..].copy_from_slice(attacker_addr.as_slice());
    calldata.extend_from_slice(&addr_word);

    // Agent sends the crafted calldata — must revert (no fallback / no matching selector).
    let result = agent.send_raw(gateway, Bytes::from(calldata), U256::ZERO, 200_000);
    assert!(
        result.is_err(),
        "A call with a non-existent withdraw(uint256,address) selector must revert"
    );
    match result.unwrap_err() {
        rmpc_fork_e2e::HarnessError::Reverted(_) => {
            eprintln!(
                "[agent_withdrawal_redirect_blocked] redirect revert confirmed — no USDC moved"
            );
        }
        e => panic!("expected Reverted, got {e:?}"),
    }

    // Sanity: attacker address still has 0 USDC.
    let attacker_bal = usdc_balance_of(&admin, usdc, attacker_addr);
    assert_eq!(
        attacker_bal,
        U256::ZERO,
        "attacker address must receive 0 USDC"
    );

    eprintln!("[agent_withdrawal_redirect_blocked] passed");
}

// ── Scenario 3: window cap ────────────────────────────────────────────────────

/// agent_withdrawal_window_cap
///
/// 1. Authorize agent with `maxWithdrawPerPayment = half` and `maxWithdrawPerWindow = half`.
/// 2. Deposit `full` USDC so shareReceiver holds `full` shares.
/// 3. First withdrawal of `half` shares — succeeds and consumes the entire window cap.
/// 4. Second withdrawal of even 1 share in the same window — must revert with
///    `WithdrawWindowCapExceeded`.
#[test]
fn agent_withdrawal_window_cap() {
    skip_if_no_fork!();
    let fx = ForkFixture::new().expect("boot fork");
    eprintln!("[agent_withdrawal_window_cap] {}", fx.summary_line());

    let usdc = rmpc_fork_e2e::addresses::USDC;
    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    let full_amount = U256::from(100_000_000u64); // 100 USDC
    let half_amount = U256::from(50_000_000u64); // 50 USDC

    let admin = fx
        .ephemeral(one_eth * U256::from(3u64), U256::ZERO)
        .expect("fund admin");
    let pauser = fx.ephemeral(one_eth, U256::ZERO).expect("fund pauser");
    // Agent is funded with full_amount USDC to deposit.
    let agent = fx
        .ephemeral(one_eth * U256::from(2u64), full_amount)
        .expect("fund agent with USDC");
    let asset_recipient_addr: Address = "0x000000000000000000000000000000000000CAFE"
        .parse()
        .unwrap();

    let vault = deploy_mock_vault(&admin, usdc);
    let gateway = deploy_gateway(&admin, usdc, vault, admin.address, pauser.address);

    let now_secs: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Policy: shareReceiver = agent.address, maxWithdrawPerPayment = half, maxWithdrawPerWindow = half.
    let policy = IGateway::AgentPolicy {
        active: true,
        validUntil: now_secs + 3600,
        maxPerPayment: full_amount,
        maxPerWindow: full_amount,
        shareReceiver: agent.address,
        allowedDestinations: vec![],
        assetRecipient: asset_recipient_addr,
        maxWithdrawPerPayment: half_amount,
        maxWithdrawPerWindow: half_amount,
        allowedSourceVaults: vec![],
    };
    admin
        .send(
            gateway,
            &IGateway::authorizeAgentCall {
                agent: agent.address,
                p: policy,
            },
            U256::ZERO,
            500_000,
        )
        .expect("authorizeAgent");

    // Deposit full_amount so agent holds full_amount shares.
    approve_usdc(&agent, usdc, gateway, full_amount);
    let deadline = now_secs + 300;
    agent
        .send(
            gateway,
            &IGateway::depositCall {
                orderId: alloy_primitives::B256::from([5u8; 32]),
                amount: full_amount,
                deadline,
                idempotencyKey: alloy_primitives::B256::from([6u8; 32]),
            },
            U256::ZERO,
            800_000,
        )
        .expect("gateway.deposit");

    let shares = vault_balance_of(&agent, vault, agent.address);
    assert_eq!(shares, full_amount, "agent must hold full_amount shares");
    // Approve gateway to pull all shares (gateway takes them in batches).
    approve_vault_shares(&agent, vault, gateway, shares);

    // First withdrawal: half_amount shares — should succeed and exhaust the window cap.
    let w1_deadline = now_secs + 300;
    let w1 = agent
        .send(
            gateway,
            &IGateway::withdrawCall {
                orderId: alloy_primitives::B256::from([20u8; 32]),
                shares: half_amount,
                sourceVault: vault,
                deadline: w1_deadline,
                idempotencyKey: alloy_primitives::B256::from([21u8; 32]),
            },
            U256::ZERO,
            800_000,
        )
        .expect("first withdrawal must succeed");
    assert_eq!(w1.status, 1, "first withdrawal must succeed");
    eprintln!(
        "[agent_withdrawal_window_cap] first withdrawal tx {:?} gasUsed={}",
        w1.tx_hash, w1.gas_used
    );

    // Second withdrawal of even 1 share in same window — must revert.
    let w2_deadline = now_secs + 300;
    let result = agent.send(
        gateway,
        &IGateway::withdrawCall {
            orderId: alloy_primitives::B256::from([22u8; 32]),
            shares: U256::from(1u64),
            sourceVault: vault,
            deadline: w2_deadline,
            idempotencyKey: alloy_primitives::B256::from([23u8; 32]),
        },
        U256::ZERO,
        800_000,
    );
    assert!(
        result.is_err(),
        "second withdrawal in same window must revert (WithdrawWindowCapExceeded)"
    );
    match result.unwrap_err() {
        rmpc_fork_e2e::HarnessError::Reverted(_) => {
            eprintln!("[agent_withdrawal_window_cap] window cap revert confirmed");
        }
        e => panic!("expected Reverted, got {e:?}"),
    }

    // Sanity: assetRecipient received only half_amount USDC.
    let recipient_bal = usdc_balance_of(&admin, usdc, asset_recipient_addr);
    assert_eq!(
        recipient_bal, half_amount,
        "assetRecipient must have received exactly half_amount USDC"
    );

    eprintln!("[agent_withdrawal_window_cap] passed");
}

// ── Scenario 4: router withdrawal round-trip ─────────────────────────────────

/// router_withdrawal
///
/// 1. Deploy VaultRegistry + two MockVaults + PortfolioRouter (60/40) + Gateway (wired to router).
/// 2. Authorize an agent whose policy enables withdrawal and includes router as a destination.
/// 3. Agent deposits USDC via gateway.depositTo(router) — shares split 60/40 to shareReceiver.
/// 4. shareReceiver approves gateway for each vault's share token.
/// 5. Agent calls gateway.withdrawFromRouter(orderId, vaults, sharesPerLeg, minAssetsPerLeg, deadline, idempotencyKey).
/// 6. Assert: USDC lands at assetRecipient, gateway holds zero residual vault receipts,
///    AgentWithdrawalRouted event is emitted.
#[test]
fn router_withdrawal() {
    skip_if_no_fork!();
    let fx = ForkFixture::new().expect("boot fork");
    eprintln!("[router_withdrawal] {}", fx.summary_line());

    let usdc = rmpc_fork_e2e::addresses::USDC;
    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    let deposit_amount = U256::from(100_000_000u64); // 100 USDC

    // Admin deploys everything; agent executes.
    let admin = fx
        .ephemeral(one_eth * U256::from(5u64), U256::ZERO)
        .expect("fund admin");
    let pauser = fx.ephemeral(one_eth, U256::ZERO).expect("fund pauser");
    let agent = fx
        .ephemeral(one_eth * U256::from(2u64), deposit_amount)
        .expect("fund agent with USDC");
    // shareReceiver: holds vault receipts after deposit; approves gateway for withdrawal.
    let share_receiver = fx
        .ephemeral(one_eth, U256::ZERO)
        .expect("fund shareReceiver");
    // assetRecipient: receives USDC on withdrawal. Use a fresh ephemeral address
    // (no ETH, no USDC) rather than a hardcoded constant, so the zero-balance
    // precondition holds regardless of which other tests ran against the shared
    // fork (e.g. `agent_withdrawal_window_cap` also routes USDC to a hardcoded
    // recipient).
    let asset_recipient: Address = fx
        .ephemeral(U256::ZERO, U256::ZERO)
        .expect("fresh assetRecipient")
        .address;

    // ── Deploy infrastructure ────────────────────────────────────────────────

    // VaultRegistry
    let registry = {
        let mut code = load_initcode("VaultRegistry.sol", "VaultRegistry").to_vec();
        code.extend_from_slice(&encode_address_arg(admin.address));
        admin
            .deploy(Bytes::from(code), 3_000_000)
            .expect("deploy VaultRegistry")
    };

    // Two 1:1 MockVaults
    let vault_a = {
        let mut code = load_initcode("MockVault.sol", "MockVault").to_vec();
        code.extend_from_slice(&encode_address_arg(usdc));
        admin
            .deploy(Bytes::from(code), 2_000_000)
            .expect("deploy vaultA")
    };
    let vault_b = {
        let mut code = load_initcode("MockVault.sol", "MockVault").to_vec();
        code.extend_from_slice(&encode_address_arg(usdc));
        admin
            .deploy(Bytes::from(code), 2_000_000)
            .expect("deploy vaultB")
    };

    // Register vaults and mark them router-eligible.
    {
        // registerVault(address, VaultMetadata(name, asset, registeredAt))
        // VaultMetadata is a struct: (string, address, uint256)
        // ABI-encode manually: offset to string, asset, registeredAt, string data.
        let register_a = IVaultRegistry::registerVaultCall {
            vault: vault_a,
            metadata: IVaultRegistry::VaultMetadata {
                name: "Vault A".to_string(),
                asset: usdc,
                registeredAt: U256::ZERO,
            },
        };
        admin
            .send(registry, &register_a, U256::ZERO, 300_000)
            .expect("registerVault A");

        let register_b = IVaultRegistry::registerVaultCall {
            vault: vault_b,
            metadata: IVaultRegistry::VaultMetadata {
                name: "Vault B".to_string(),
                asset: usdc,
                registeredAt: U256::ZERO,
            },
        };
        admin
            .send(registry, &register_b, U256::ZERO, 300_000)
            .expect("registerVault B");

        admin
            .send(
                registry,
                &IVaultRegistry::setRouterEligibleCall {
                    vault: vault_a,
                    eligible: true,
                },
                U256::ZERO,
                100_000,
            )
            .expect("setRouterEligible A");
        admin
            .send(
                registry,
                &IVaultRegistry::setRouterEligibleCall {
                    vault: vault_b,
                    eligible: true,
                },
                U256::ZERO,
                100_000,
            )
            .expect("setRouterEligible B");
    }

    // PortfolioRouter (60/40 split)
    let router = {
        let mut code = load_initcode("PortfolioRouter.sol", "PortfolioRouter").to_vec();
        let mut args = [0u8; 96];
        args[12..32].copy_from_slice(usdc.as_slice());
        args[44..64].copy_from_slice(registry.as_slice());
        args[76..96].copy_from_slice(admin.address.as_slice());
        code.extend_from_slice(&args);
        admin
            .deploy(Bytes::from(code), 4_000_000)
            .expect("deploy PortfolioRouter")
    };

    // Set 60/40 weights.
    admin
        .send(
            router,
            &IPortfolioRouter::setWeightsCall {
                vaults: vec![vault_a, vault_b],
                bps: vec![U256::from(6000u64), U256::from(4000u64)],
            },
            U256::ZERO,
            300_000,
        )
        .expect("setWeights");

    // RobotMoneyGateway (with router)
    // Constructor: (usdc, vault, admin, pauser, router) — 5 × address.
    // `vault` arg: use vault_a as the pinned vault (single-vault path still needs one).
    let gateway = {
        let mut code = load_initcode("RobotMoneyGateway.sol", "RobotMoneyGateway").to_vec();
        let mut args = [0u8; 160];
        args[12..32].copy_from_slice(usdc.as_slice());
        args[44..64].copy_from_slice(vault_a.as_slice()); // pinned vault
        args[76..96].copy_from_slice(admin.address.as_slice());
        args[108..128].copy_from_slice(pauser.address.as_slice());
        args[140..160].copy_from_slice(router.as_slice());
        code.extend_from_slice(&args);
        // Gas limit bumped from 5_000_000 → 8_000_000 to accommodate the
        // committee routing additions in issue #1049 (deployment cost ~5.15M).
        admin
            .deploy(Bytes::from(code), 8_000_000)
            .expect("deploy RobotMoneyGateway")
    };

    eprintln!(
        "[router_withdrawal] vaultA={vault_a:#x} vaultB={vault_b:#x} router={router:#x} gateway={gateway:#x}"
    );

    // ── Authorize agent ──────────────────────────────────────────────────────
    let now_secs: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // AZ-GW-3 fix: empty allowedSourceVaults now means pinned-vault-only
    // (vault_a, which is the gateway's vaultContract). Router withdrawal pulls
    // from both vault_a and vault_b, so enumerate both in allowedSourceVaults.
    let policy = IGateway::AgentPolicy {
        active: true,
        validUntil: now_secs + 3600,
        maxPerPayment: deposit_amount,
        maxPerWindow: deposit_amount * U256::from(10u64),
        shareReceiver: share_receiver.address,
        allowedDestinations: vec![router],
        assetRecipient: asset_recipient,
        maxWithdrawPerPayment: deposit_amount,
        maxWithdrawPerWindow: deposit_amount * U256::from(10u64),
        allowedSourceVaults: vec![vault_a, vault_b],
    };
    admin
        .send(
            gateway,
            &IGateway::authorizeAgentCall {
                agent: agent.address,
                p: policy,
            },
            U256::ZERO,
            500_000,
        )
        .expect("authorizeAgent");

    // ── Deposit via router ───────────────────────────────────────────────────
    // Agent approves gateway for USDC.
    agent
        .send(
            usdc,
            &IUSDC::approveCall {
                spender: gateway,
                amount: deposit_amount,
            },
            U256::ZERO,
            100_000,
        )
        .expect("USDC.approve gateway");

    let deadline = now_secs + 300;
    let deposit_receipt = agent
        .send(
            gateway,
            &IGateway::depositToCall {
                orderId: alloy_primitives::B256::from([50u8; 32]),
                amount: deposit_amount,
                deadline,
                idempotencyKey: alloy_primitives::B256::from([51u8; 32]),
                destination: router,
                minSharesPerLeg: vec![],
            },
            U256::ZERO,
            1_500_000,
        )
        .expect("gateway.depositTo(router)");
    assert_eq!(deposit_receipt.status, 1, "depositTo must succeed");

    // Verify shareReceiver holds vault shares (60/40 split).
    let shares_a = vault_balance_of(&admin, vault_a, share_receiver.address);
    let shares_b = vault_balance_of(&admin, vault_b, share_receiver.address);
    let expected_a = deposit_amount * U256::from(6000u64) / U256::from(10000u64);
    let expected_b = deposit_amount * U256::from(4000u64) / U256::from(10000u64);
    assert_eq!(shares_a, expected_a, "shareReceiver vaultA shares");
    assert_eq!(shares_b, expected_b, "shareReceiver vaultB shares");

    eprintln!("[router_withdrawal] shares deposited: vaultA={shares_a} vaultB={shares_b}");

    // ── Approve gateway to pull vault shares ─────────────────────────────────
    approve_vault_shares(&share_receiver, vault_a, gateway, shares_a);
    approve_vault_shares(&share_receiver, vault_b, gateway, shares_b);

    // ── Pre-withdrawal: assetRecipient has zero USDC ─────────────────────────
    let pre_bal = usdc_balance_of(&agent, usdc, asset_recipient);
    assert_eq!(pre_bal, U256::ZERO, "assetRecipient starts with 0 USDC");

    // ── Call withdrawFromRouter ──────────────────────────────────────────────
    let withdraw_deadline = now_secs + 300;
    // Issue #967: redeem legs are driven by the explicit caller-supplied
    // `vaults[]`, identity-bound to `sharesPerLeg` (not the router's live weight
    // vector), so a reweighted-out/retired position stays redeemable.
    let vaults = vec![vault_a, vault_b];
    let shares_per_leg = vec![shares_a, shares_b];
    // GW-5 / F-11 / issue #969: forward a real per-leg slippage floor. On this
    // 1:1 fork fixture a zero floor is sufficient to exercise the happy path,
    // but the vector must be parallel to sharesPerLeg.
    let min_assets_per_leg = vec![U256::ZERO, U256::ZERO];
    let withdraw_receipt = agent
        .send(
            gateway,
            &IGateway::withdrawFromRouterCall {
                orderId: alloy_primitives::B256::from([60u8; 32]),
                vaults,
                sharesPerLeg: shares_per_leg,
                minAssetsPerLeg: min_assets_per_leg,
                deadline: withdraw_deadline,
                idempotencyKey: alloy_primitives::B256::from([61u8; 32]),
            },
            U256::ZERO,
            1_500_000,
        )
        .expect("gateway.withdrawFromRouter");
    assert_eq!(
        withdraw_receipt.status, 1,
        "withdrawFromRouter must succeed"
    );
    eprintln!(
        "[router_withdrawal] withdrawFromRouter tx {:?} gasUsed={}",
        withdraw_receipt.tx_hash, withdraw_receipt.gas_used
    );

    // ── Assert USDC landed at assetRecipient ─────────────────────────────────
    let post_bal = usdc_balance_of(&agent, usdc, asset_recipient);
    assert_eq!(
        post_bal - pre_bal,
        deposit_amount,
        "assetRecipient must receive full deposit_amount USDC (1:1 MockVaults)"
    );

    // ── Assert gateway holds zero vault shares ────────────────────────────────
    let gw_shares_a = vault_balance_of(&agent, vault_a, gateway);
    let gw_shares_b = vault_balance_of(&agent, vault_b, gateway);
    assert_eq!(gw_shares_a, U256::ZERO, "gateway must hold 0 vaultA shares");
    assert_eq!(gw_shares_b, U256::ZERO, "gateway must hold 0 vaultB shares");

    // ── Assert shareReceiver holds zero vault shares ──────────────────────────
    let sr_shares_a = vault_balance_of(&admin, vault_a, share_receiver.address);
    let sr_shares_b = vault_balance_of(&admin, vault_b, share_receiver.address);
    assert_eq!(
        sr_shares_a,
        U256::ZERO,
        "shareReceiver must hold 0 vaultA shares"
    );
    assert_eq!(
        sr_shares_b,
        U256::ZERO,
        "shareReceiver must hold 0 vaultB shares"
    );

    // ── Assert AgentWithdrawalRouted event was emitted ───────────────────────
    let topic0 = keccak256(
        b"AgentWithdrawalRouted(bytes32,bytes32,address,address,address,uint256[],uint256[],address,uint64)",
    );
    let routed_log = withdraw_receipt
        .logs
        .iter()
        .find(|l| l.topics.first() == Some(&topic0) && l.address == gateway);
    assert!(
        routed_log.is_some(),
        "AgentWithdrawalRouted event not found in receipt; logs={:?}",
        withdraw_receipt.logs
    );
    let log = routed_log.unwrap();
    // topic3 = indexed agent address.
    assert!(
        log.topics.len() >= 4,
        "AgentWithdrawalRouted must have at least 4 topics"
    );
    let agent_from_log = Address::from_slice(&log.topics[3].as_slice()[12..]);
    assert_eq!(agent_from_log, agent.address, "topic3 agent mismatch");

    eprintln!("[router_withdrawal] passed");
}
