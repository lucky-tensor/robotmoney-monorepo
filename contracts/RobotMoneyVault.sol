// SPDX-License-Identifier: MIT
// Canonical: docs/architecture.md §4.1 — Vault Family (RobotMoneyVault)
// (See also: docs/prd.md §11.1 — Stable Yield Vault;
//  docs/architecture.md §6 — Data and Trust Boundaries)
pragma solidity ^0.8.24;

import {ERC4626} from "@openzeppelin/contracts/token/ERC20/extensions/ERC4626.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Math} from "@openzeppelin/contracts/utils/math/Math.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {IStrategyAdapter} from "./interfaces/IStrategyAdapter.sol";
import {BpsMath} from "./lib/BpsMath.sol";
import {ForeignTokenQuarantine} from "./lib/ForeignTokenQuarantine.sol";
import {AdminFloorAccessControlCounter} from "./lib/AdminFloorAccessControlCounter.sol";

/// @title RobotMoneyVault
/// @notice Multi-adapter ERC-4626 USDC vault on Base. Dynamic equal-weight target across active
///         adapters. On-chain trustless pricing. Atomic deposit-to-yield AND withdraw — both
///         single-transaction, standard ERC-4626. Exit fee applied on withdrawal.
///         Yearn V3-inspired security: 2 roles + hardcoded floors.
///
/// Deployed: 0x4f835c9f54bcf17daf9040f60cb72951ccbb49dd (Base mainnet)
/// Compiler: v0.8.24+commit.e11b9ed9, optimized 200 runs, EVM Cancun
contract RobotMoneyVault is ERC4626, AdminFloorAccessControlCounter, ReentrancyGuard {
    using SafeERC20 for IERC20;
    using Math for uint256;

    // ─── Roles ─────────────────────────────────────────────────────────

    /// @notice Role that can manage adapters, set parameters, and rebalance.
    ///         Self-administered, and floor-protected: the shared
    ///         `AdminFloorAccessControlCounter` base forbids revoking or
    ///         renouncing the final holder (ACL-3 / F-06), so admin functions
    ///         (including this role's own grant path) can never be
    ///         permanently bricked.
    bytes32 public constant ADMIN_ROLE = keccak256("ADMIN_ROLE");
    /// @notice Role that can pause and perform emergency withdrawals.
    ///         Asymmetric with unpause by design: a compromised emergency key can
    ///         only halt the vault (DoS), not restart it. Unpause is restricted to
    ///         `ADMIN_ROLE` so that resuming operations is deliberate and requires
    ///         the higher-trust role — mirroring the gateway's `PAUSER_ROLE` /
    ///         `ADMIN_ROLE` asymmetry documented in `AccessRoles.sol`.
    bytes32 public constant EMERGENCY_ROLE = keccak256("EMERGENCY_ROLE");
    /// @notice Role for automated keeper rebalancing (not granted at launch).
    bytes32 public constant KEEPER_ROLE = keccak256("KEEPER_ROLE");

    // ─── Immutable bytecode constants — no role can change ─────────────

    /// @notice Absolute ceiling on exit fee (100 bps = 1%).
    uint256 public constant MAX_EXIT_FEE_BPS = 100;
    /// @notice Maximum number of strategy adapters the vault can hold.
    uint256 public constant MAX_ADAPTERS = 20;
    /// @notice Basis-points denominator (10 000 = 100%). Narrowed to `uint16`
    ///         from the shared `BpsMath.BPS_DENOMINATOR` to preserve this
    ///         constant's existing public type and call-site arithmetic.
    uint16 public constant MAX_BPS = uint16(BpsMath.BPS_DENOMINATOR);
    /// @notice Keeper can never move more than 50% of TVL in a single rebalance call.
    uint16 public constant MAX_REBALANCE_BPS_CEILING = 5000;
    /// @notice Minimum enforced interval between rebalance calls (1 hour).
    uint256 public constant MIN_REBALANCE_INTERVAL_FLOOR = 1 hours;

    // ─── Adapter registry ──────────────────────────────────────────────

    struct AdapterInfo {
        IStrategyAdapter adapter;
        uint16 capBps; // max allocation % out of MAX_BPS — also acts as ramp control
        bool active;
    }
    /// @notice Ordered registry of all strategy adapters (active and inactive).
    AdapterInfo[] public adapters;
    /// @notice Exact adapter instances approved by vault governance to receive this vault's USDC.
    mapping(address adapter => bool allowed) public adapterAllowed;
    /// @notice Runtime bytecode hashes approved by vault governance for adapter onboarding.
    mapping(bytes32 codeHash => bool allowed) public adapterCodeHashAllowed;

    // ─── Configurable params ──────────────────────────────────────────

    /// @notice Maximum total assets under management; deposits revert above this.
    uint256 public tvlCap;
    /// @notice Maximum USDC that a single deposit may contribute.
    uint256 public perDepositCap;
    /// @notice Exit fee in basis points charged on withdrawals.
    uint256 public exitFeeBps;
    /// @notice Recipient of collected exit fees.
    address public feeRecipient;
    /// @notice Destination for permissionless foreign-token sweeps (INV-1/INV-2).
    ///         Defaults to `ForeignTokenQuarantine.QUARANTINE`; settable only via
    ///         the TimelockController (ADMIN_ROLE, INV-3). The timelock-settable
    ///         model (vs. a pure compile-time constant) allows governance to
    ///         redirect sweeps to an on-chain multisig that can empty the trash.
    address public quarantineAddress;

    /// @notice Whether the vault has been permanently shut down. Irreversible.
    bool public shutdown;

    /// @notice Whether the vault has been retired by the unified governance
    ///         `retire()` lifecycle action (DI-2; docs/architecture.md §4.7).
    ///         When true, direct deposits/mints are hard-stopped at the vault.
    ///         Distinct from the emergency `shutdown` flag so the two
    ///         enforcement paths never alias: `shutdown` is an EMERGENCY_ROLE
    ///         vault-only overlay that makes no lifecycle decision, whereas
    ///         `retired` is set only by the governance retire action flipping
    ///         the registry to `Retired` in the same call. Recovery is the
    ///         deliberate governance abort `VaultRegistry.setVaultStatus(vault,
    ///         Active)` reflected back via `unretire()`.
    bool public retired;

    /// @notice Linked `VaultRegistry`. Set once by `ADMIN_ROLE` after both
    ///         contracts are deployed. The registry is the only address allowed
    ///         to drive the vault's `retire()` / `unretire()` deposit-halt legs,
    ///         so the unified governance retire action (registry status flip +
    ///         vault deposit halt) lands atomically in a single timelock call to
    ///         `VaultRegistry.retire(vault)` without granting the registry full
    ///         `ADMIN_ROLE` over the vault.
    address public registry;

    // ─── Split pause semantics ─────────────────────────────────────────
    // Deposits and withdrawals are gated independently so that emergencyWithdraw()
    // can block new capital inflows while preserving user exit rights.

    /// @notice When true, new deposits and mints are blocked.
    bool public depositsPaused;
    /// @notice When true, withdrawals and redeems are blocked.
    bool public withdrawalsPaused;

    // ─── Rebalance throttling ──

    /// @notice Maximum fraction of TVL a keeper may move in one rebalance call (bps).
    uint16 public maxRebalanceBpsPerCall;
    /// @notice Minimum time between consecutive rebalance calls (seconds).
    uint256 public minRebalanceInterval;
    /// @notice Timestamp of the most recent completed rebalance.
    uint256 public lastRebalanceAt;

    // ─── Events ────────────────────────────────────────────────────────

    /// @notice Emitted when a new strategy adapter is registered.
    /// @param index   Registry index of the new adapter.
    /// @param adapter Address of the registered adapter contract.
    /// @param capBps  Maximum allocation cap in basis points.
    event AdapterAdded(uint256 indexed index, address indexed adapter, uint16 capBps);
    /// @notice Emitted when governance approves or revokes an exact adapter instance.
    /// @param adapter Adapter address whose eligibility changed.
    /// @param allowed True when the adapter may be onboarded and receive allocations.
    event AdapterAllowedSet(address indexed adapter, bool allowed);
    /// @notice Emitted when governance approves or revokes an adapter runtime code hash.
    /// @param codeHash Runtime bytecode hash whose eligibility changed.
    /// @param allowed  True when adapters with this runtime bytecode may be onboarded.
    event AdapterCodeHashAllowedSet(bytes32 indexed codeHash, bool allowed);
    /// @notice Emitted when an adapter is deactivated (normal removal).
    /// @param index   Registry index of the removed adapter.
    /// @param adapter Address of the deactivated adapter contract.
    event AdapterRemoved(uint256 indexed index, address indexed adapter);
    /// @notice Emitted when an adapter's allocation cap is updated.
    /// @param index  Registry index of the adapter.
    /// @param oldBps Previous cap in basis points.
    /// @param newBps New cap in basis points.
    event AdapterCapUpdated(uint256 indexed index, uint16 oldBps, uint16 newBps);
    /// @notice Emitted when an adapter is force-removed without withdrawing assets (emergency).
    /// @param index      Registry index of the force-removed adapter.
    /// @param adapter    Address of the adapter contract.
    /// @param lossAmount Estimated assets lost due to force removal.
    event AdapterForceRemoved(uint256 indexed index, address indexed adapter, uint256 lossAmount);
    /// @notice Emitted when USDC is allocated from the vault into an adapter.
    /// @param index   Registry index of the target adapter.
    /// @param adapter Address of the target adapter contract.
    /// @param amount  Amount of USDC allocated (6-decimal units).
    event Allocated(uint256 indexed index, address indexed adapter, uint256 amount);
    /// @notice Emitted when USDC is pulled from an adapter back to the vault.
    /// @param index   Registry index of the source adapter.
    /// @param adapter Address of the source adapter contract.
    /// @param amount  Amount of USDC pulled (6-decimal units).
    event Pulled(uint256 indexed index, address indexed adapter, uint256 amount);
    /// @notice Emitted at the end of a successful rebalance call.
    /// @param totalMoved Total USDC redistributed across adapters (6-decimal units).
    event Rebalanced(uint256 totalMoved);
    /// @notice Emitted when the per-call rebalance cap is updated.
    /// @param oldBps Previous cap in basis points.
    /// @param newBps New cap in basis points.
    event MaxRebalanceBpsUpdated(uint16 oldBps, uint16 newBps);
    /// @notice Emitted when the minimum rebalance interval is updated.
    /// @param oldInterval Previous minimum interval in seconds.
    /// @param newInterval New minimum interval in seconds.
    event MinRebalanceIntervalUpdated(uint256 oldInterval, uint256 newInterval);
    /// @notice Emitted when an exit fee is charged on a withdrawal.
    /// @param owner      Share owner who initiated the withdrawal.
    /// @param receiver   Address that received the net USDC.
    /// @param grossAssets Gross USDC value of redeemed shares.
    /// @param fee        Exit fee charged (grossAssets × exitFeeBps / MAX_BPS).
    /// @param netAssets  Net USDC transferred to receiver (grossAssets − fee).
    event ExitFeeCharged(
        address indexed owner,
        address indexed receiver,
        uint256 grossAssets,
        uint256 fee,
        uint256 netAssets
    );
    /// @notice Emitted when the TVL cap is updated.
    /// @param oldCap Previous TVL cap (6-decimal USDC units).
    /// @param newCap New TVL cap (6-decimal USDC units).
    event TvlCapUpdated(uint256 oldCap, uint256 newCap);
    /// @notice Emitted when the per-deposit cap is updated.
    /// @param oldCap Previous per-deposit cap (6-decimal USDC units).
    /// @param newCap New per-deposit cap (6-decimal USDC units).
    event PerDepositCapUpdated(uint256 oldCap, uint256 newCap);
    /// @notice Emitted when the exit fee is updated.
    /// @param oldBps Previous exit fee in basis points.
    /// @param newBps New exit fee in basis points.
    event ExitFeeUpdated(uint256 oldBps, uint256 newBps);
    /// @notice Emitted when the fee recipient address is updated.
    /// @param oldRecipient Previous fee recipient address.
    /// @param newRecipient New fee recipient address.
    event FeeRecipientUpdated(address indexed oldRecipient, address indexed newRecipient);
    /// @notice Emitted when the quarantine address for foreign-token sweeps is updated.
    /// @param oldAddr Previous quarantine address.
    /// @param newAddr New quarantine address.
    event QuarantineAddressUpdated(address indexed oldAddr, address indexed newAddr);
    /// @notice Emitted when the emergency withdrawal flow is triggered (all adapters).
    event EmergencyWithdrawCalled();
    /// @notice Emitted per-adapter during an emergency withdrawal.
    /// @param index   Registry index of the adapter.
    /// @param adapter Address of the adapter contract.
    /// @param amount  Amount withdrawn (0 on failure or empty balance).
    /// @param success Whether the adapter's withdraw call succeeded.
    event EmergencyWithdrawAdapterCalled(
        uint256 indexed index, address indexed adapter, uint256 amount, bool success
    );
    /// @notice Emitted when the vault is shut down.
    event Shutdown();
    /// @notice Emitted when a shut-down vault is restored and deposits re-open.
    /// @param newTvlCap The fresh TVL cap set on restore.
    event VaultRestored(uint256 newTvlCap);
    /// @notice Emitted when deposit pause state changes.
    /// @param paused True when deposits are blocked, false when unblocked.
    event DepositsPausedChanged(bool paused);
    /// @notice Emitted when withdrawal pause state changes.
    /// @param paused True when withdrawals are blocked, false when unblocked.
    event WithdrawalsPausedChanged(bool paused);
    /// @notice Emitted when a deposit cannot be fully routed into adapters (e.g. all caps are full).
    /// @param amount USDC that remains idle in the vault after both routing passes.
    event UnroutedDeposit(uint256 amount);
    /// @notice Emitted when the linked `VaultRegistry` reference is set.
    /// @param oldRegistry Previous registry address (0 = unset).
    /// @param newRegistry New registry address.
    event RegistrySet(address indexed oldRegistry, address indexed newRegistry);
    /// @notice Emitted when the vault is retired by the governance `retire` action.
    event Retired();
    /// @notice Emitted when a retired vault is reactivated (governance abort).
    event Unretired();

    // ─── Errors ────────────────────────────────────────────────────────

    /// @notice Deposit would push total managed assets above `tvlCap`.
    error TVLCapExceeded();
    /// @notice A single deposit exceeds the per-deposit cap.
    error PerDepositCapExceeded();
    /// @notice Constructor or admin call passed `address(0)` where a real address is required.
    error ZeroAddress();
    /// @notice Operation rejected because the vault has been shut down.
    error VaultShutdown();
    /// @notice Deposit/mint rejected because the vault has been retired.
    error VaultRetired();
    /// @notice `retire()` / `unretire()` caller is not the linked registry.
    error OnlyRegistry();
    /// @notice `setRegistry` called more than once (registry is set-once).
    error RegistryAlreadySet();
    /// @notice `restoreVault` called while the vault is not in a shut-down state.
    error NotShutdown();
    /// @notice Exit-fee bps argument exceeds `MAX_EXIT_FEE_BPS` (1%).
    error InvalidFee();
    /// @notice Generic admin parameter validation failure (zero/out-of-range value).
    error InvalidParam();
    /// @notice Adapter cap bps is zero or above `MAX_BPS`.
    error InvalidCap();
    /// @notice Allocation to a single adapter would exceed its configured `capBps`.
    error ExceedsAdapterCap();
    /// @notice Adapter registry already holds `MAX_ADAPTERS`; cannot add another.
    error MaxAdaptersReached();
    /// @notice Provided adapter index is out of range or refers to an inactive entry.
    error AdapterNotFound();
    /// @notice Cannot remove an adapter while it still custodies assets — withdraw first.
    error AdapterNotEmpty();
    /// @notice Deposit/rebalance attempted while no adapter is active.
    error NoActiveAdapters();
    /// @notice Keeper called `rebalance()` before `minRebalanceInterval` elapsed since `lastRebalanceAt`.
    error RebalanceTooSoon();
    /// @notice Caller lacks `KEEPER_ROLE` (or `ADMIN_ROLE` where the rebalancer path also accepts it).
    error UnauthorizedRebalancer();
    /// @notice Deposit attempted while deposits are paused.
    error DepositsPaused();
    /// @notice Withdrawal attempted while withdrawals are paused.
    error WithdrawalsPaused();
    /// @notice Adapter address has not been approved by vault governance.
    /// @param adapter Adapter address that failed the address allowlist check.
    error AdapterNotAllowed(address adapter);
    /// @notice Adapter runtime bytecode hash has not been approved by vault governance.
    /// @param adapter  Adapter address that failed the code-hash allowlist check.
    /// @param codeHash Runtime bytecode hash observed on the adapter.
    error AdapterCodeHashNotAllowed(address adapter, bytes32 codeHash);
    /// @notice Adapter does not expose the expected `USDC()` or `VAULT()` compatibility views.
    /// @param adapter Adapter address that could not be compatibility-checked.
    error AdapterCompatibilityCheckFailed(address adapter);
    /// @notice Adapter reports a USDC token that differs from this vault's ERC-4626 asset.
    /// @param adapter  Adapter address that reported the wrong asset.
    /// @param expected This vault's ERC-4626 asset.
    /// @param actual   Asset reported by the adapter's `USDC()` view.
    error AdapterAssetMismatch(address adapter, address expected, address actual);
    /// @notice Adapter reports an owning vault other than this vault.
    /// @param adapter  Adapter address that reported the wrong vault.
    /// @param expected This vault address.
    /// @param actual   Vault reported by the adapter's `VAULT()` view.
    error AdapterVaultMismatch(address adapter, address expected, address actual);
    /// @notice Active adapters cannot deliver the USDC required for this withdrawal.
    ///         Raised early (before any transfer) so callers see a clear error instead
    ///         of an opaque downstream ERC-20 balance revert. (Audit 2026-06-09, L-2.)
    /// @param requested USDC needed from adapters (after idle balance is applied).
    /// @param available Total USDC the active adapters actually delivered or report holding.
    error InsufficientAdapterLiquidity(uint256 requested, uint256 available);

    // ─── Constructor ──────────────────────────────────────────────────

    constructor(
        IERC20 _asset,
        uint256 _tvlCap,
        uint256 _perDepositCap,
        uint256 _exitFeeBps,
        address _feeRecipient,
        address _admin,
        address _emergencyResponder
    ) ERC4626(_asset) ERC20("Robot Money USDC", "rmUSDC") {
        if (
            _feeRecipient == address(0) || _admin == address(0) || _emergencyResponder == address(0)
        ) {
            revert ZeroAddress();
        }
        if (_exitFeeBps > MAX_EXIT_FEE_BPS) revert InvalidFee();
        if (_tvlCap > 0 && _perDepositCap > _tvlCap) revert InvalidParam();

        tvlCap = _tvlCap;
        perDepositCap = _perDepositCap;
        exitFeeBps = _exitFeeBps;
        feeRecipient = _feeRecipient;
        quarantineAddress = ForeignTokenQuarantine.QUARANTINE;

        maxRebalanceBpsPerCall = 2500; // 25%
        minRebalanceInterval = 12 hours;

        _setRoleAdmin(ADMIN_ROLE, ADMIN_ROLE);
        _setRoleAdmin(EMERGENCY_ROLE, ADMIN_ROLE);
        _setRoleAdmin(KEEPER_ROLE, ADMIN_ROLE);

        _grantRole(ADMIN_ROLE, _admin);
        _grantRole(EMERGENCY_ROLE, _emergencyResponder);
        // KEEPER_ROLE intentionally NOT granted
    }

    /// @notice Returns the decimal precision used by this vault's share token (6, matching USDC).
    ///
    /// @dev Share token precision is fixed at 6 so that external tools (wallets, explorers,
    ///      integrators) always see a consistent denomination regardless of the internal
    ///      virtual-share scale chosen for inflation protection.
    ///
    ///      Raw-share scale note (for integrators):
    ///      The ERC-4626 virtual-share offset is 18 (see `_decimalsOffset`).  OpenZeppelin's
    ///      `_convertToShares` formula is:
    ///        shares = assets × (totalSupply + 10^18) / (totalAssets + 1)
    ///      For a fresh vault this yields `1e6 USDC → 1e24 raw shares`.  Because `decimals()`
    ///      returns 6, a user interface rendering `balanceOf(user) / 1e6` would display
    ///      `1e18` rmUSDC for a 1 USDC seed deposit.  This is intentional: the inflated share
    ///      count is what makes donation-based price manipulation economically infeasible.
    ///      Once the vault accumulates real TVL the share price converges to 1 rmUSDC ≈ 1 USDC
    ///      (in 6-decimal terms) and the raw count no longer dominates the display.
    function decimals() public pure override(ERC4626) returns (uint8) {
        return 6;
    }

    /// @notice Returns the ERC-4626 virtual-share decimal offset used to resist first-depositor
    ///         share-price inflation attacks.
    ///
    /// @dev Returning 18 configures OpenZeppelin's ERC-4626 virtual shares to `10^18` and
    ///      virtual assets to `1`.  With this setting the economic cost of a donation-based
    ///      inflation attack scales as `10^18` — orders of magnitude beyond any realistic
    ///      attacker budget — while legitimate depositors receive economically fair shares at
    ///      all TVL levels.
    ///
    ///      Raw-share scale (fresh vault, decimals() == 6, _decimalsOffset() == 18):
    ///        previewDeposit(1e6)  → 1e24 raw shares  (= 1e18 rmUSDC in 6-decimal display)
    ///        previewMint(1e24)    → 1e6 USDC
    ///        previewRedeem(1e24)  → ~1e6 USDC (minus exit fee if any)
    ///        previewWithdraw(1e6) → ~1e24 raw shares
    ///
    ///      Integrators MUST NOT assume raw shares equal asset amounts.  Always use
    ///      `convertToShares` / `convertToAssets` for on-chain math, or read `decimals()` and
    ///      divide accordingly in off-chain display logic.
    ///
    ///      See: docs/technical/security-model.md — ERC-4626 Inflation Attack Mitigation
    function _decimalsOffset() internal pure override returns (uint8) {
        return 18;
    }

    // ─── totalAssets ──────────────────────────────────────────────────

    /// @notice Sum of USDC held directly in the vault (idle) plus all eligible-and-active
    ///         adapter balances.
    /// @dev Idle USDC can accumulate via direct transfers or when `_routeDeposit` cannot place
    ///      all assets (e.g. all adapter caps are exhausted). Including it here prevents NAV
    ///      understatement and the associated TVL-cap bypass / share-price dilution described
    ///      in docs/code-review/20260508-code-review-internal-claude-sonnet-4-6.md — Finding 2.
    ///
    ///      ADP-2 (F-14): an adapter whose eligibility was revoked while still registered as
    ///      `active` (allowlist withdrawn or codehash de-listed) is EXCLUDED from NAV. A
    ///      revoked adapter is no longer trusted to price its holdings, so continuing to count
    ///      its self-reported balance would let a compromised/lying adapter inflate the share
    ///      price (and, on the withdrawal side, drain honest holders' idle USDC). Eligibility
    ///      is restorable via `setAdapterAllowed`, and the funds remain drainable by EMERGENCY
    ///      via `emergencyWithdrawAdapter`, so this is an exclusion-not-confiscation.
    function totalAssets() public view override returns (uint256) {
        uint256 sum = IERC20(asset()).balanceOf(address(this)); // include idle vault balance
        uint256 len = adapters.length;
        for (uint256 i = 0; i < len; i++) {
            if (_isAdapterCounted(i)) sum += adapters[i].adapter.totalAssets();
        }
        return sum;
    }

    /// @dev An adapter contributes to NAV / receives proportional withdrawals only when it is
    ///      both registered-active AND currently eligible (allowlisted + codehash-pinned +
    ///      identity-bound). Centralises the ADP-2 NAV-side check so `totalAssets` and
    ///      `_pullProportional` can never drift apart.
    function _isAdapterCounted(uint256 i) internal view returns (bool) {
        return adapters[i].active && _isAdapterEligible(address(adapters[i].adapter));
    }

    // ─── Deposit (atomic deposit-to-yield) ────────────────────────────

    function _deposit(address caller, address receiver, uint256 assets, uint256 shares)
        internal
        override
        nonReentrant
    {
        if (depositsPaused) revert DepositsPaused();
        if (shutdown) revert VaultShutdown();
        if (retired) revert VaultRetired();
        if (assets > perDepositCap) revert PerDepositCapExceeded();
        if (totalAssets() + assets > tvlCap) revert TVLCapExceeded();
        if (_activeAdapterCount() == 0) revert NoActiveAdapters();

        super._deposit(caller, receiver, assets, shares);
        _routeDeposit(assets);
    }

    function _routeDeposit(uint256 amount) internal {
        // slither-disable-next-line incorrect-equality
        // Justification: `amount == 0` is a safe early-return guard, not a
        // balance-sensitive strict equality that reentrancy could manipulate.
        if (amount == 0) return;

        // `totalAssets()` now includes the idle vault balance (the deposited USDC already sits
        // in the vault at this point), so it already accounts for `amount`. Do NOT add `amount`
        // again — that would double-count it.
        uint256 totalAfter = totalAssets();
        uint256 activeCount = _activeAdapterCount();
        uint256 remaining = amount;
        uint256 len = adapters.length;

        // Pass-1 `totalAssets()` reads, held as `balance + 1` so 0 means "not read"
        // without shadowing a genuine zero balance. Reused by pass 2 ONLY when
        // pass 1 allocated nothing at all (`remaining == amount` below): in that
        // case nothing but staticcalls happened in between, so the cached reads
        // are exact by construction. The moment pass 1 allocates, every later
        // read is taken fresh — a cached value would be a MODELLED balance, and
        // pass 2's `capBps` headroom check must never be computed from one.
        uint256[] memory cachedBalance = new uint256[](len);

        // Rank of the current adapter among the active ones, in registry order.
        uint256 rank;

        // Absolute `capBps` headroom pass 1 leaves behind, summed over the
        // adapters it visited. Pass 2 has nothing to do when this is zero.
        uint256 capHeadroom;

        // Pass 1: fill toward min(equal target, capBps)
        for (uint256 i = 0; i < len && remaining > 0; i++) {
            if (!adapters[i].active) continue;
            uint256 shareBalance = _equalWeightBalance(totalAfter, rank, activeCount);
            rank++;
            // Skip adapters whose allowlist / codehash eligibility was revoked while
            // still active in the registry — depositing must not brick when governance
            // quarantines one adapter (audit 2026-06-09, L-4). `addAdapter` and
            // `adminRebalance` keep the hard `_requireAdapterEligible` revert.
            if (!_isAdapterEligible(address(adapters[i].adapter))) continue;
            // Still `min(cap, equal target)`, taken on the balances the two bps
            // figures denote so the equal-weight slice keeps its exact partition
            // of NAV. `capBalance` is the same expression pass 2 uses, so an
            // adapter can never be filled past its cap here.
            uint256 capBalance = (totalAfter * adapters[i].capBps) / MAX_BPS;
            uint256 targetBalance = shareBalance < capBalance ? shareBalance : capBalance;
            uint256 currentBalance = adapters[i].adapter.totalAssets();
            if (currentBalance >= targetBalance) {
                cachedBalance[i] = currentBalance + 1;
                if (capBalance > currentBalance) capHeadroom += capBalance - currentBalance;
                continue;
            }
            uint256 deficit = targetBalance - currentBalance;
            uint256 allocation = deficit < remaining ? deficit : remaining;
            _allocateTo(i, allocation);
            remaining -= allocation;
            uint256 filled = currentBalance + allocation;
            if (capBalance > filled) capHeadroom += capBalance - filled;
        }

        // slither-disable-next-line incorrect-equality
        // Justification: `remaining == amount` asks whether pass 1 allocated
        // anything at all — both are local accumulators, not balances an
        // attacker can nudge, and the guard only ever makes pass 2 read MORE.
        bool passOneAllocatedNothing = remaining == amount;

        // Pass 2: spread leftover into adapters with absolute cap headroom.
        //
        // `capHeadroom == 0` means pass 1 already saw every eligible adapter at
        // its `capBps` share, so pass 2 could only re-read them and skip. That is
        // not a corner case: a cap set that sums to exactly `MAX_BPS` (the devnet
        // 3334/3333/3333) floors to slightly LESS than NAV, so a few wei of every
        // deposit are permanently unplaceable — and re-reading every adapter to
        // rediscover that was the second round of `totalAssets()` calls #1391 is
        // about. Skipping pass 2 here leaves exactly the USDC pass 2 would have
        // left, and still emits `UnroutedDeposit`.
        //
        // `capHeadroom` models an allocated adapter's new balance as
        // `currentBalance + allocation`, exact for the 1:1 lending adapters this
        // vault family routes to (`deploy` moves USDC at par, with no min-out
        // leg). It only ever gates whether pass 2 is ATTEMPTED — pass 2 re-reads
        // and re-checks `capBps` against a real balance before placing anything,
        // so an inexact model can never widen a cap, only leave USDC idle.
        if (remaining > 0 && capHeadroom > 0) {
            for (uint256 i = 0; i < len && remaining > 0; i++) {
                if (!adapters[i].active) continue;
                if (!_isAdapterEligible(address(adapters[i].adapter))) continue;
                uint256 cached = passOneAllocatedNothing ? cachedBalance[i] : 0;
                uint256 currentBalance =
                    cached == 0 ? adapters[i].adapter.totalAssets() : cached - 1;
                uint256 capBalance = (totalAfter * adapters[i].capBps) / MAX_BPS;
                if (currentBalance >= capBalance) continue;
                uint256 headroom = capBalance - currentBalance;
                uint256 allocation = headroom < remaining ? headroom : remaining;
                _allocateTo(i, allocation);
                remaining -= allocation;
            }
        }

        // Any USDC still unrouted (e.g. all adapter caps exhausted) stays idle in the vault.
        // Emit an event so off-chain monitors can detect and react (e.g. trigger rebalance).
        if (remaining > 0) emit UnroutedDeposit(remaining);
    }

    // slither-disable-start reentrancy-events
    // `_routeDeposit()` and `rebalance()` are `nonReentrant`; the emitted
    // event happens after the guarded external adapter calls and the warning is
    // a false positive.
    function _allocateTo(uint256 i, uint256 amount) internal {
        IStrategyAdapter adpt = adapters[i].adapter;
        address adptAddr = address(adpt);
        _requireAdapterEligible(adptAddr);
        IERC20(asset()).safeTransfer(adptAddr, amount);
        adpt.deploy(amount);
        emit Allocated(i, adptAddr, amount);
    }

    // slither-disable-end reentrancy-events

    // ─── Synchronous withdraw / redeem ────────────────────────────────

    /// @notice Estimate net USDC returned when redeeming `shares` (after exit fee).
    /// @param shares Number of rmUSDC shares to simulate redeeming.
    function previewRedeem(uint256 shares) public view override returns (uint256) {
        uint256 grossAssets = _convertToAssets(shares, Math.Rounding.Floor);
        return _grossToNet(grossAssets);
    }

    /// @notice Estimate shares required to receive exactly `assets` USDC net (after exit fee).
    /// @param assets Target net USDC to receive.
    function previewWithdraw(uint256 assets) public view override returns (uint256) {
        uint256 grossAssets = _netToGross(assets);
        return _convertToShares(grossAssets, Math.Rounding.Ceil);
    }

    /// @notice Maximum USDC a user can withdraw in a single call (net of exit fee).
    ///         Overrides the OZ default to satisfy ERC-4626: withdraw(maxWithdraw(owner)) MUST NOT revert.
    ///         Uses floor rounding on the gross→net conversion so that
    ///         `_netToGross(maxWithdraw(owner))` never exceeds `_convertToAssets(balanceOf(owner), Floor)`,
    ///         guaranteeing `previewWithdraw(maxWithdraw(owner)) <= balanceOf(owner)` even when `exitFeeBps > 0`.
    ///         Returns 0 while withdrawals are paused, mirroring the deposit-side views
    ///         (ERC-4626: withdraw(maxWithdraw(owner)) MUST NOT revert; audit 2026-06-09, L-1).
    /// @param owner The address whose share balance determines the withdrawal cap.
    function maxWithdraw(address owner) public view override returns (uint256) {
        if (withdrawalsPaused) return 0;
        uint256 shares = balanceOf(owner);
        uint256 grossAssets = _convertToAssets(shares, Math.Rounding.Floor);
        return grossAssets.mulDiv(MAX_BPS - exitFeeBps, MAX_BPS, Math.Rounding.Floor);
    }

    /// @notice Maximum shares a user can redeem in a single call.
    ///         Returns 0 while withdrawals are paused so that `redeem(maxRedeem(owner))`
    ///         never reverts, per ERC-4626 (audit 2026-06-09, L-1).
    /// @param owner The address whose share balance determines the redemption cap.
    function maxRedeem(address owner) public view override returns (uint256) {
        if (withdrawalsPaused) return 0;
        return balanceOf(owner);
    }

    /// @notice Maximum assets that can be deposited for `receiver` given current vault state.
    ///         Returns 0 when deposits are paused, the vault is shutdown, retired,
    ///         no adapters are active, or the TVL cap has been reached.
    function maxDeposit(address) public view override returns (uint256) {
        if (depositsPaused || shutdown || retired) return 0;
        if (_activeAdapterCount() == 0) return 0;
        if (tvlCap == type(uint256).max && perDepositCap == type(uint256).max) {
            return type(uint256).max;
        }
        uint256 current = totalAssets();
        if (current >= tvlCap) return 0;
        uint256 headroom = tvlCap - current;
        return perDepositCap < headroom ? perDepositCap : headroom;
    }

    /// @notice Maximum shares that can be minted for `receiver` given current vault state.
    /// @param receiver The address that would receive the minted shares.
    function maxMint(address receiver) public view override returns (uint256) {
        uint256 assets = maxDeposit(receiver);
        if (assets == type(uint256).max) return type(uint256).max;
        return _convertToShares(assets, Math.Rounding.Floor);
    }

    function _grossToNet(uint256 gross) internal view returns (uint256) {
        return gross - gross.mulDiv(exitFeeBps, MAX_BPS);
    }

    function _netToGross(uint256 net) internal view returns (uint256) {
        if (exitFeeBps == 0) return net;
        return net.mulDiv(MAX_BPS, MAX_BPS - exitFeeBps, Math.Rounding.Ceil);
    }

    function _withdraw(
        address caller,
        address receiver,
        address owner,
        uint256 assets,
        uint256 shares
    ) internal override nonReentrant {
        if (withdrawalsPaused) revert WithdrawalsPaused();
        if (caller != owner) {
            _spendAllowance(owner, caller, shares);
        }

        uint256 grossAssets = _convertToAssets(shares, Math.Rounding.Floor);
        uint256 fee = grossAssets - assets;

        // FEE-2 / NC-11: the exit fee is only ever paid out of proceeds this withdrawal
        // ACTUALLY realises, never from a share-implied gross that an over-reporting adapter
        // could otherwise have other holders' idle USDC silently cover. `_pullProportional`
        // returns the realised USDC (idle applied + genuinely pulled) and reverts via
        // `InsufficientAdapterLiquidity` if it cannot source the full `grossAssets`, so a
        // withdrawal can never disburse `assets + fee` it did not source. (NAV — and hence
        // `grossAssets` — already excludes revoked adapters, ADP-2 / F-14.) The realised
        // figure must cover `grossAssets == assets + fee`; the require pins that the fee was
        // funded by realised proceeds, not socialized.
        uint256 realizedGross = _pullProportional(grossAssets);
        require(realizedGross >= grossAssets, "fee not covered by realised proceeds");

        // slither-disable-next-line reentrancy-no-eth
        // Justification: `_withdraw` is `nonReentrant`; the `_burn` after
        // external adapter calls is safe because reentry is blocked by the OZ guard.
        _burn(owner, shares);

        // ERC-4626 parity: the receiver gets exactly `assets`; the fee (charged on the
        // realised gross, since `grossAssets` was fully realised above) goes to the recipient.
        if (fee > 0) {
            IERC20(asset()).safeTransfer(feeRecipient, fee);
            emit ExitFeeCharged(owner, receiver, grossAssets, fee, assets);
        }
        IERC20(asset()).safeTransfer(receiver, assets);

        emit Withdraw(caller, receiver, owner, assets, shares);
    }

    // slither-disable-start reentrancy-balance
    // The public entrypoints that reach this helper are `nonReentrant`; the
    // pre-call balance reads are therefore protected and the stale-balance
    // warning is a false positive.
    /// @dev Source `assetsNeeded` USDC into the vault, returning the amount ACTUALLY realised
    ///      (idle USDC applied + USDC genuinely withdrawn from adapters). Under honest adapters
    ///      the return equals `assetsNeeded`; `_withdraw` asserts the realised figure covers the
    ///      full share-implied gross before paying the exit fee (FEE-2 / NC-11), so an adapter
    ///      that over-reports its balance can never have its shortfall funded from other holders'
    ///      idle USDC — an under-delivering adapter reverts (`InsufficientAdapterLiquidity`).
    ///
    ///      Only eligible-and-active adapters (`_isAdapterCounted`) are pulled from: a revoked
    ///      adapter is excluded from NAV (`totalAssets`), so it must likewise be excluded here —
    ///      otherwise the proportional denominator would not match NAV and a revoked adapter
    ///      could still receive/return withdrawal flow (ADP-2 / F-14).
    function _pullProportional(uint256 assetsNeeded) internal returns (uint256) {
        if (assetsNeeded == 0) return 0;

        // First satisfy from idle USDC already sitting in the vault (e.g. after emergencyWithdraw).
        uint256 idleBalance = IERC20(asset()).balanceOf(address(this));
        if (idleBalance >= assetsNeeded) {
            // Idle balance covers the full withdrawal — no adapter pull needed.
            return assetsNeeded;
        }

        uint256 totalInAdapters;
        uint256 len = adapters.length;
        for (uint256 i = 0; i < len; i++) {
            if (_isAdapterCounted(i)) totalInAdapters += adapters[i].adapter.totalAssets();
        }

        // Remaining amount that must come from adapters (after idle covers part of it).
        // Fail fast with a dedicated error when the eligible adapters cannot deliver the
        // requested amount — clamping here only converts the shortfall into an opaque
        // downstream ERC-20 transfer revert (audit 2026-06-09, L-2).
        uint256 remainingNeeded = assetsNeeded - idleBalance;
        if (remainingNeeded > totalInAdapters) {
            revert InsufficientAdapterLiquidity(remainingNeeded, totalInAdapters);
        }

        uint256 remaining = remainingNeeded;
        uint256 pulled;

        // Pass 1: proportional pulls, each capped at the adapter's reported balance.
        for (uint256 i = 0; i < len && remaining > 0; i++) {
            if (!_isAdapterCounted(i)) continue;
            IStrategyAdapter adpt = adapters[i].adapter;
            uint256 adapterBalance = adpt.totalAssets();
            uint256 pull = (remainingNeeded * adapterBalance) / totalInAdapters;
            if (pull > remaining) pull = remaining;
            if (pull > adapterBalance) pull = adapterBalance;
            if (pull == 0) continue;
            uint256 actual = adpt.withdraw(pull);
            pulled += actual;
            remaining = actual >= remaining ? 0 : remaining - actual;
            emit Pulled(i, address(adpt), actual);
        }

        // Pass 2: sweep rounding leftovers across all eligible adapters, capping each
        // pull at min(remaining, balance). Replaces the old behaviour of dumping the
        // full leftover on the last active adapter regardless of its balance, which
        // could DoS a withdrawal other adapters could cover (audit 2026-06-09, L-2).
        for (uint256 i = 0; i < len && remaining > 0; i++) {
            if (!_isAdapterCounted(i)) continue;
            IStrategyAdapter adpt = adapters[i].adapter;
            uint256 adapterBalance = adpt.totalAssets();
            uint256 pull = adapterBalance < remaining ? adapterBalance : remaining;
            if (pull == 0) continue;
            uint256 actual = adpt.withdraw(pull);
            pulled += actual;
            remaining = actual >= remaining ? 0 : remaining - actual;
            emit Pulled(i, address(adpt), actual);
        }

        // Adapters under-delivered versus their reported balances (e.g. a buggy or
        // lying adapter). Surface the typed error instead of letting the subsequent
        // transfer to the receiver revert opaquely.
        if (remaining > 0) {
            revert InsufficientAdapterLiquidity(remainingNeeded, remainingNeeded - remaining);
        }

        // Realised = idle consumed by this withdrawal + USDC genuinely pulled from adapters.
        // Clamp to `assetsNeeded`: an adapter that returns MORE than requested leaves the
        // surplus idle for ALL holders rather than over-paying this one position. The fee and
        // payout are therefore bounded by the share-implied gross from above and by realised
        // proceeds from below — never socializing one holder's shortfall onto the others.
        uint256 realized = idleBalance + pulled;
        return realized < assetsNeeded ? realized : assetsNeeded;
    }

    // slither-disable-end reentrancy-balance

    // ─── Adapter management ──────────────────────────────────────────

    /// @notice Register a new strategy adapter. Restricted to `ADMIN_ROLE`.
    /// @param adapter_ Address of the `IStrategyAdapter`-compatible contract.
    /// @param capBps_  Maximum allocation cap in basis points (1–10 000).
    function addAdapter(address adapter_, uint16 capBps_) external onlyRole(ADMIN_ROLE) {
        if (adapter_ == address(0)) revert ZeroAddress();
        if (capBps_ == 0 || capBps_ > MAX_BPS) revert InvalidCap();
        if (_activeAdapterCount() >= MAX_ADAPTERS) revert MaxAdaptersReached();
        _requireAdapterEligible(adapter_);
        adapters.push(
            AdapterInfo({adapter: IStrategyAdapter(adapter_), capBps: capBps_, active: true})
        );
        emit AdapterAdded(adapters.length - 1, adapter_, capBps_);
    }

    /// @notice Approve or revoke an exact adapter instance for this vault. Restricted to `ADMIN_ROLE`.
    /// @param adapter_ Adapter address whose eligibility should change.
    /// @param allowed_ True to allow onboarding/allocation, false to revoke future allocations.
    /// @dev Revoking the allowlist does NOT affect adapters already registered and active in the
    ///      registry. To fully quarantine an adapter, callers must separately call
    ///      `emergencyWithdrawAdapter` (to drain assets) followed by `removeAdapter` or
    ///      `forceRemoveAdapter` (to deactivate the registry entry). Setting `allowed_ = false`
    ///      only blocks future deposit allocations and new `addAdapter` calls for this address.
    function setAdapterAllowed(address adapter_, bool allowed_) external onlyRole(ADMIN_ROLE) {
        if (adapter_ == address(0)) revert ZeroAddress();
        adapterAllowed[adapter_] = allowed_;
        emit AdapterAllowedSet(adapter_, allowed_);
    }

    /// @notice Approve or revoke an adapter runtime bytecode hash. Restricted to `ADMIN_ROLE`.
    /// @param codeHash_ Runtime bytecode hash whose eligibility should change.
    /// @param allowed_  True to allow onboarding/allocation, false to revoke future allocations.
    function setAdapterCodeHashAllowed(bytes32 codeHash_, bool allowed_)
        external
        onlyRole(ADMIN_ROLE)
    {
        if (codeHash_ == bytes32(0)) revert InvalidParam();
        adapterCodeHashAllowed[codeHash_] = allowed_;
        emit AdapterCodeHashAllowedSet(codeHash_, allowed_);
    }

    /// @notice Deactivate an adapter. The adapter must hold zero assets. Restricted to `ADMIN_ROLE`.
    /// @param index Registry index of the adapter to remove.
    function removeAdapter(uint256 index) external onlyRole(ADMIN_ROLE) {
        if (index >= adapters.length || !adapters[index].active) revert AdapterNotFound();
        if (adapters[index].adapter.totalAssets() > 0) revert AdapterNotEmpty();
        adapters[index].active = false;
        emit AdapterRemoved(index, address(adapters[index].adapter));
    }

    /// @notice Update the allocation cap for an existing adapter. Restricted to `ADMIN_ROLE`.
    /// @param index     Registry index of the adapter.
    /// @param newCapBps New maximum allocation cap in basis points (1–10 000).
    function setAdapterCap(uint256 index, uint16 newCapBps) external onlyRole(ADMIN_ROLE) {
        if (index >= adapters.length || !adapters[index].active) revert AdapterNotFound();
        if (newCapBps == 0 || newCapBps > MAX_BPS) revert InvalidCap();
        uint16 old = adapters[index].capBps;
        adapters[index].capBps = newCapBps;
        emit AdapterCapUpdated(index, old, newCapBps);
    }

    // ─── Rebalance ────────────────────────────────────────────────────

    /// @notice Keeper-triggered equal-weight rebalance. Callable by `ADMIN_ROLE` or `KEEPER_ROLE`.
    ///         Pulls excess from over-weight adapters and re-routes into under-weight adapters.
    ///         Subject to `minRebalanceInterval` and `maxRebalanceBpsPerCall` throttles.
    function rebalance() external nonReentrant {
        if (!hasRole(ADMIN_ROLE, msg.sender) && !hasRole(KEEPER_ROLE, msg.sender)) {
            revert UnauthorizedRebalancer();
        }
        if (!isRebalanceAvailable()) revert RebalanceTooSoon();
        uint256 activeCount = _activeAdapterCount();
        if (activeCount == 0) revert NoActiveAdapters();

        lastRebalanceAt = block.timestamp;

        uint256 totalAssetsCached = totalAssets();
        uint256 maxMovePerCall = (totalAssetsCached * maxRebalanceBpsPerCall) / MAX_BPS;
        uint256 totalMoved;
        uint256 rank;

        uint256 len = adapters.length;
        for (uint256 i = 0; i < len; i++) {
            if (!adapters[i].active) continue;
            if (totalMoved >= maxMovePerCall) break;
            // Same exactly-partitioning target set the allocator uses (#1391), so
            // a rebalance does not pull dust that `_routeDeposit` hands straight
            // back on the `_routeDeposit(idle)` leg below.
            uint256 targetBalance = _equalWeightBalance(totalAssetsCached, rank, activeCount);
            rank++;
            uint256 currentBalance = adapters[i].adapter.totalAssets();
            if (currentBalance <= targetBalance) continue;
            uint256 excess = currentBalance - targetBalance;
            uint256 pull =
                (totalMoved + excess > maxMovePerCall) ? (maxMovePerCall - totalMoved) : excess;
            if (pull == 0) continue;
            uint256 actual = adapters[i].adapter.withdraw(pull);
            totalMoved += actual;
            emit Pulled(i, address(adapters[i].adapter), actual);
        }

        uint256 idle = IERC20(asset()).balanceOf(address(this));
        if (idle > 0) _routeDeposit(idle);

        emit Rebalanced(totalMoved);
    }

    /// @notice Admin-specified precision rebalance: sets each adapter to an explicit target balance.
    ///         Restricted to `ADMIN_ROLE`.
    /// @param targetBalances Target USDC balance for each adapter (must match `adapters.length`).
    function adminRebalance(uint256[] calldata targetBalances)
        external
        onlyRole(ADMIN_ROLE)
        nonReentrant
    {
        if (targetBalances.length != adapters.length) revert InvalidParam();
        uint256 len = adapters.length;
        uint256 totalMoved = 0;

        for (uint256 i = 0; i < len; i++) {
            if (!adapters[i].active) continue;
            uint256 current = adapters[i].adapter.totalAssets();
            if (current > targetBalances[i]) {
                uint256 excess = current - targetBalances[i];
                uint256 actual = adapters[i].adapter.withdraw(excess);
                totalMoved += actual;
                emit Pulled(i, address(adapters[i].adapter), actual);
            }
        }

        uint256 idle = IERC20(asset()).balanceOf(address(this));
        if (idle > 0) {
            for (uint256 i = 0; i < len && idle > 0; i++) {
                if (!adapters[i].active) continue;
                uint256 current = adapters[i].adapter.totalAssets();
                if (current < targetBalances[i]) {
                    uint256 deficit = targetBalances[i] - current;
                    uint256 allocation = deficit < idle ? deficit : idle;
                    _allocateTo(i, allocation);
                    idle -= allocation;
                }
            }
        }

        lastRebalanceAt = block.timestamp;
        emit Rebalanced(totalMoved);
    }

    /// @notice Update the per-call rebalance cap. Restricted to `ADMIN_ROLE`.
    /// @param newBps New cap in basis points (1–5 000; must not exceed `MAX_REBALANCE_BPS_CEILING`).
    function setMaxRebalanceBpsPerCall(uint16 newBps) external onlyRole(ADMIN_ROLE) {
        if (newBps == 0 || newBps > MAX_REBALANCE_BPS_CEILING) revert InvalidParam();
        uint16 old = maxRebalanceBpsPerCall;
        maxRebalanceBpsPerCall = newBps;
        emit MaxRebalanceBpsUpdated(old, newBps);
    }

    /// @notice Update the minimum interval between rebalance calls. Restricted to `ADMIN_ROLE`.
    /// @param newInterval New minimum interval in seconds (must be ≥ `MIN_REBALANCE_INTERVAL_FLOOR`).
    function setMinRebalanceInterval(uint256 newInterval) external onlyRole(ADMIN_ROLE) {
        if (newInterval < MIN_REBALANCE_INTERVAL_FLOOR) revert InvalidParam();
        uint256 old = minRebalanceInterval;
        minRebalanceInterval = newInterval;
        emit MinRebalanceIntervalUpdated(old, newInterval);
    }

    // ─── Emergency ────────────────────────────────────────────────────

    /// @notice Pause all deposits and withdrawals. Restricted to `EMERGENCY_ROLE`.
    function pause() external onlyRole(EMERGENCY_ROLE) {
        _setDepositsPaused(true);
        _setWithdrawalsPaused(true);
    }

    /// @notice Resume deposits and withdrawals. Restricted to `ADMIN_ROLE`.
    ///         Intentionally asymmetric: pausing is fast and unilateral (`EMERGENCY_ROLE`);
    ///         unpausing is deliberate and requires the higher-trust admin role.
    function unpause() external onlyRole(ADMIN_ROLE) {
        _setDepositsPaused(false);
        _setWithdrawalsPaused(false);
    }

    /// @notice Pause the vault and attempt to withdraw all assets from every active adapter.
    ///         Uses `try/catch` so a failed adapter does not block others. Restricted to `EMERGENCY_ROLE`.
    ///         After this call, deposits are blocked but withdrawals remain open so users can exit.
    function emergencyWithdraw() external onlyRole(EMERGENCY_ROLE) nonReentrant {
        _setDepositsPaused(true);
        // Withdrawals are intentionally left open so existing users can redeem idle USDC.
        uint256 len = adapters.length;
        for (uint256 i = 0; i < len; i++) {
            if (!adapters[i].active) continue;
            IStrategyAdapter adpt = adapters[i].adapter;
            address adptAddr = address(adpt);
            uint256 balance;
            try adpt.totalAssets() returns (uint256 reported) {
                balance = reported;
            } catch {
                emit EmergencyWithdrawAdapterCalled(i, adptAddr, 0, false);
                continue;
            }
            if (balance == 0) continue;
            try adpt.withdraw(balance) returns (uint256 actual) {
                emit EmergencyWithdrawAdapterCalled(i, adptAddr, actual, true);
            } catch {
                emit EmergencyWithdrawAdapterCalled(i, adptAddr, 0, false);
            }
        }
        emit EmergencyWithdrawCalled();
    }

    /// @notice Pause deposits and withdraw all assets from a single adapter. Restricted to `EMERGENCY_ROLE`.
    ///         Withdrawals remain open so users can redeem assets pulled into idle USDC.
    /// @param index Registry index of the adapter to drain.
    function emergencyWithdrawAdapter(uint256 index)
        external
        onlyRole(EMERGENCY_ROLE)
        nonReentrant
    {
        if (index >= adapters.length || !adapters[index].active) {
            revert AdapterNotFound();
        }
        _setDepositsPaused(true);
        uint256 balance;
        try adapters[index].adapter.totalAssets() returns (uint256 reported) {
            balance = reported;
        } catch {
            emit EmergencyWithdrawAdapterCalled(index, address(adapters[index].adapter), 0, false);
            return;
        }
        if (balance == 0) {
            emit EmergencyWithdrawAdapterCalled(index, address(adapters[index].adapter), 0, true);
            return;
        }
        try adapters[index].adapter.withdraw(balance) returns (uint256 actual) {
            emit EmergencyWithdrawAdapterCalled(
                index, address(adapters[index].adapter), actual, true
            );
        } catch {
            emit EmergencyWithdrawAdapterCalled(index, address(adapters[index].adapter), 0, false);
        }
    }

    /// @notice Force-remove an adapter without withdrawing its assets (last-resort action).
    ///         Assets in the adapter are treated as lost. Restricted to `EMERGENCY_ROLE`.
    /// @param index Registry index of the adapter to force-remove.
    function forceRemoveAdapter(uint256 index) external onlyRole(EMERGENCY_ROLE) {
        if (index >= adapters.length || !adapters[index].active) revert AdapterNotFound();
        _setDepositsPaused(true);
        IStrategyAdapter adapter = adapters[index].adapter;
        adapters[index].active = false;
        uint256 lossAmount;
        try adapter.totalAssets() returns (uint256 reported) {
            lossAmount = reported;
        } catch {}
        emit AdapterForceRemoved(index, address(adapter), lossAmount);
    }

    // ─── Unified governance retire (DI-2) ─────────────────────────────────
    //
    // Canonical: docs/architecture.md §4.7 (decided target, decision #925) and
    // §4.5 authority tiers; docs/prd.md §6 lifecycle + §12 INV-3.
    //
    // The vault deposit-halt leg of the unified governance `retire()` action.
    // `shutdownVault` (below) is the EMERGENCY-tier, vault-only deposit
    // hard-stop (hot key) and is deliberately NOT a lifecycle decision — it
    // makes no registry change. The governance `retire()` is the opposite: it
    // is the deliberate, governance-tier (multisig + TimelockController)
    // lifecycle decision. To keep the registry `Retired` flip and the vault
    // deposit-halt from drifting, the registry's `retire(vault)` drives this
    // `retire()` in the same transaction. The `retired` flag is distinct from
    // the emergency `shutdown` flag so the two paths never alias.

    /// @notice Set the linked `VaultRegistry` once. Restricted to `ADMIN_ROLE`
    ///         (TimelockController in production — INV-3). The registry is the
    ///         only address permitted to call `retire()` / `unretire()`; this
    ///         dedicated link keeps the registry's authority over the vault
    ///         narrow (deposit-halt only, not full admin) while letting the
    ///         unified governance retire action land atomically.
    /// @param newRegistry Address of the `VaultRegistry` (must not be zero).
    function setRegistry(address newRegistry) external onlyRole(ADMIN_ROLE) {
        if (newRegistry == address(0)) revert ZeroAddress();
        if (registry != address(0)) revert RegistryAlreadySet();
        registry = newRegistry;
        emit RegistrySet(address(0), newRegistry);
    }

    /// @notice Retire the vault: hard-stop direct deposits/mints. Callable ONLY
    ///         by the linked registry, which sets registry status to `Retired`
    ///         in the same call (atomic unified governance retire, DI-2). Not an
    ///         emergency control: it makes the deliberate lifecycle decision the
    ///         registry status flip records. Idempotent — re-retiring a retired
    ///         vault is a no-op event. Withdrawals/redemptions stay open
    ///         (ERC-4626 `redeem` is never revoked; ADR-0009).
    function retire() external {
        if (msg.sender != registry) revert OnlyRegistry();
        if (retired) return;
        retired = true;
        emit Retired();
    }

    /// @notice Reactivate a retired vault and re-open direct deposits. Callable
    ///         ONLY by the linked registry, which flips registry status back to
    ///         `Active` in the same call (governance abort path, mirroring the
    ///         `Retired → Active` transition in docs/architecture.md §4.7).
    function unretire() external {
        if (msg.sender != registry) revert OnlyRegistry();
        if (!retired) return;
        retired = false;
        emit Unretired();
    }

    /// @notice Shut down the vault: set `shutdown = true` and zero the TVL cap.
    ///         Restricted to `EMERGENCY_ROLE`. Recoverable only by `ADMIN_ROLE`
    ///         via `restoreVault`, mirroring the `pause`/`unpause` trust
    ///         asymmetry: a compromised emergency hot key can DoS deposits but
    ///         cannot permanently brick the vault — re-opening requires the
    ///         higher-trust admin role.
    function shutdownVault() external onlyRole(EMERGENCY_ROLE) {
        shutdown = true;
        tvlCap = 0;
        emit Shutdown();
    }

    /// @notice Reverse a `shutdownVault` and re-open deposits. Restricted to
    ///         `ADMIN_ROLE`. Intentionally asymmetric: shutting down is fast and
    ///         unilateral (`EMERGENCY_ROLE`); restoring is deliberate and requires
    ///         the higher-trust admin role. Because `shutdownVault` zeroed the TVL
    ///         cap, the admin supplies a fresh cap so deposits resume under an
    ///         explicit limit rather than silently reusing a stale value.
    /// @param newTvlCap New maximum total assets in 6-decimal USDC units (must be > 0).
    function restoreVault(uint256 newTvlCap) external onlyRole(ADMIN_ROLE) {
        if (!shutdown) revert NotShutdown();
        if (newTvlCap == 0) revert InvalidCap();
        if (perDepositCap > newTvlCap) revert InvalidParam();
        shutdown = false;
        uint256 old = tvlCap;
        tvlCap = newTvlCap;
        emit VaultRestored(newTvlCap);
        emit TvlCapUpdated(old, newTvlCap);
    }

    // ─── Param setters ────────────────────────────────────────────────

    /// @notice Update the TVL cap. Restricted to `ADMIN_ROLE`.
    /// @param newCap New maximum total assets in 6-decimal USDC units.
    function setTvlCap(uint256 newCap) external onlyRole(ADMIN_ROLE) {
        if (newCap > 0 && perDepositCap > newCap) revert InvalidParam();
        uint256 old = tvlCap;
        tvlCap = newCap;
        emit TvlCapUpdated(old, newCap);
    }

    /// @notice Update the per-deposit cap. Restricted to `ADMIN_ROLE`.
    /// @param newCap New maximum single-deposit amount in 6-decimal USDC units.
    function setPerDepositCap(uint256 newCap) external onlyRole(ADMIN_ROLE) {
        if (tvlCap > 0 && newCap > tvlCap) revert InvalidParam();
        uint256 old = perDepositCap;
        perDepositCap = newCap;
        emit PerDepositCapUpdated(old, newCap);
    }

    /// @notice Update the exit fee. Restricted to `ADMIN_ROLE`.
    /// @param newBps New exit fee in basis points (0–`MAX_EXIT_FEE_BPS`).
    function setExitFeeBps(uint256 newBps) external onlyRole(ADMIN_ROLE) {
        if (newBps > MAX_EXIT_FEE_BPS) revert InvalidFee();
        uint256 old = exitFeeBps;
        exitFeeBps = newBps;
        emit ExitFeeUpdated(old, newBps);
    }

    /// @notice Update the fee recipient address. Restricted to `ADMIN_ROLE`.
    /// @param newRecipient New address to receive collected exit fees.
    function setFeeRecipient(address newRecipient) external onlyRole(ADMIN_ROLE) {
        if (newRecipient == address(0)) revert ZeroAddress();
        address old = feeRecipient;
        feeRecipient = newRecipient;
        emit FeeRecipientUpdated(old, newRecipient);
    }

    /// @notice Update the quarantine address for foreign-token sweeps. Restricted
    ///         to `ADMIN_ROLE` (held by TimelockController in production — INV-3).
    /// @param newAddr New quarantine address. Must not be address(0).
    function setQuarantineAddress(address newAddr) external onlyRole(ADMIN_ROLE) {
        if (newAddr == address(0)) revert ZeroAddress();
        address old = quarantineAddress;
        quarantineAddress = newAddr;
        emit QuarantineAddressUpdated(old, newAddr);
    }

    /// @notice Permissionlessly sweep a NON-protected foreign token held by the
    ///         vault to the governed quarantine address (custody invariants
    ///         INV-1/INV-2).
    ///
    ///         Anyone may call; the destination is the timelock-gated
    ///         `quarantineAddress` storage variable — never a caller-supplied
    ///         address (INV-1). This replaces the deleted arbitrary-recipient
    ///         `rescueTokens(token,to)` admin function. The vault asset (USDC,
    ///         already counted in `totalAssets` and redeemable) and the vault
    ///         share token cannot be swept; protocol-asset donations therefore
    ///         stay in NAV and accrue pro-rata to all holders (INV-2). Adapter
    ///         strategy tokens live on the adapters, each of which exposes its
    ///         own guarded `sweepForeignToken`.
    /// @param token Foreign ERC-20 to quarantine. Must not be the vault asset or
    ///        the vault share token.
    function sweepForeignToken(address token) external nonReentrant {
        if (token == asset() || token == address(this)) {
            revert ForeignTokenQuarantine.TokenIsProtected(token);
        }
        ForeignTokenQuarantine.sweep(token, quarantineAddress, msg.sender);
    }

    // ─── Internal helpers ─────────────────────────────────────────────

    /// @dev Set `depositsPaused` and emit an event if the state changes.
    function _setDepositsPaused(bool paused_) internal {
        if (depositsPaused != paused_) {
            depositsPaused = paused_;
            emit DepositsPausedChanged(paused_);
        }
    }

    /// @dev Set `withdrawalsPaused` and emit an event if the state changes.
    function _setWithdrawalsPaused(bool paused_) internal {
        if (withdrawalsPaused != paused_) {
            withdrawalsPaused = paused_;
            emit WithdrawalsPausedChanged(paused_);
        }
    }

    /// @dev Non-reverting twin of `_requireAdapterEligible`, used by `_routeDeposit`
    ///      to skip (rather than revert on) adapters whose eligibility was revoked
    ///      while still active in the registry (audit 2026-06-09, L-4).
    function _isAdapterEligible(address adapter_) internal view returns (bool) {
        if (!adapterAllowed[adapter_]) return false;
        if (!adapterCodeHashAllowed[adapter_.codehash]) return false;
        (bool usdcOk, bytes memory usdcData) =
            adapter_.staticcall(abi.encodeWithSignature("USDC()"));
        if (!usdcOk || usdcData.length != 32 || abi.decode(usdcData, (address)) != asset()) {
            return false;
        }
        (bool vaultOk, bytes memory vaultData) =
            adapter_.staticcall(abi.encodeWithSignature("VAULT()"));
        if (!vaultOk || vaultData.length != 32 || abi.decode(vaultData, (address)) != address(this))
        {
            return false;
        }
        return true;
    }

    function _requireAdapterEligible(address adapter_) internal view {
        if (!adapterAllowed[adapter_]) revert AdapterNotAllowed(adapter_);
        bytes32 codeHash = adapter_.codehash;
        if (!adapterCodeHashAllowed[codeHash]) {
            revert AdapterCodeHashNotAllowed(adapter_, codeHash);
        }
        _requireAdapterCompatible(adapter_);
    }

    function _requireAdapterCompatible(address adapter_) internal view {
        (bool usdcOk, bytes memory usdcData) =
            adapter_.staticcall(abi.encodeWithSignature("USDC()"));
        if (!usdcOk || usdcData.length != 32) revert AdapterCompatibilityCheckFailed(adapter_);
        address reportedAsset = abi.decode(usdcData, (address));
        if (reportedAsset != asset()) {
            revert AdapterAssetMismatch(adapter_, asset(), reportedAsset);
        }

        (bool vaultOk, bytes memory vaultData) =
            adapter_.staticcall(abi.encodeWithSignature("VAULT()"));
        if (!vaultOk || vaultData.length != 32) revert AdapterCompatibilityCheckFailed(adapter_);
        address reportedVault = abi.decode(vaultData, (address));
        if (reportedVault != address(this)) {
            revert AdapterVaultMismatch(adapter_, address(this), reportedVault);
        }
    }

    /// @dev Share of `MAX_BPS` owed to the `rank`-th (0-based, registry order) of
    ///      `active` equally-weighted adapters.
    ///
    ///      `MAX_BPS / active` on its own FLOORS: with three active adapters it
    ///      is 3333, so the per-adapter targets summed to 9999 bps and never to
    ///      100 %. Pass 1 of `_routeDeposit` could therefore never place the last
    ///      bps of NAV into a balanced, fully-deployed vault — `remaining` stayed
    ///      positive and pass 2 ran on EVERY deposit, re-reading every adapter's
    ///      `totalAssets()` (issue #1391). The `MAX_BPS % active` leftover bps are
    ///      handed out one each to the lowest ranks, so the target set sums to
    ///      exactly `MAX_BPS`.
    ///
    ///      Callers still take `min(capBps, targetBps)`, so a distributed bps can
    ///      never lift an adapter above its own cap.
    function _equalWeightBps(uint256 rank, uint256 active) internal pure returns (uint256) {
        if (active == 0) return 0;
        return MAX_BPS / active + (rank < MAX_BPS % active ? 1 : 0);
    }

    /// @dev The `rank`-th active adapter's slice of `total` when `active`
    ///      adapters split it equally — the balance `_equalWeightBps` denotes.
    ///
    ///      Computed as the DIFFERENCE OF TWO PREFIX SHARES rather than as
    ///      `total * bps / MAX_BPS`, so the slices partition `total` exactly.
    ///      Distributing the remainder bps is necessary but not sufficient:
    ///      flooring each `total * bps / MAX_BPS` independently still loses up to
    ///      `active - 1` wei, and on the real devnet composition that dust alone
    ///      kept `remaining > 0` and dragged pass 2 — and its second round of
    ///      `totalAssets()` reads — into every deposit (issue #1391).
    ///
    ///      Because the slices sum to `total` while the adapters hold
    ///      `total - idle`, pass 1's deficits always sum to at least the deposit,
    ///      so pass 1 can place all of it whenever no `capBps` binds.
    function _equalWeightBalance(uint256 total, uint256 rank, uint256 active)
        internal
        pure
        returns (uint256)
    {
        if (active == 0) return 0;
        uint256 base = MAX_BPS / active;
        uint256 extra = MAX_BPS % active;
        uint256 cumBefore = base * rank + (rank < extra ? rank : extra);
        uint256 cumAfter = cumBefore + base + (rank < extra ? 1 : 0);
        return (total * cumAfter) / MAX_BPS - (total * cumBefore) / MAX_BPS;
    }

    /// @dev Equal-weight target in bps for the adapter at registry `index`, or 0
    ///      when it is inactive. The one definition every consumer shares — the
    ///      allocator, `rebalance()` and the drift views — so the target set can
    ///      never drift out of summing to `MAX_BPS` in one place but not another.
    function _targetBpsForIndex(uint256 index) internal view returns (uint256) {
        if (index >= adapters.length || !adapters[index].active) return 0;
        uint256 len = adapters.length;
        uint256 active;
        uint256 rank;
        for (uint256 i = 0; i < len; i++) {
            if (!adapters[i].active) continue;
            if (i < index) rank++;
            active++;
        }
        return _equalWeightBps(rank, active);
    }

    function _activeAdapterCount() internal view returns (uint256) {
        uint256 count;
        uint256 len = adapters.length;
        for (uint256 i = 0; i < len; i++) {
            if (adapters[i].active) count++;
        }
        return count;
    }

    // ─── Views ────────────────────────────────────────────────────────

    /// @notice Returns true when both deposits and withdrawals are blocked (full pause).
    ///         Provided for compatibility with tooling that queries `paused()`.
    function paused() external view returns (bool) {
        return depositsPaused && withdrawalsPaused;
    }

    /// @notice Total number of adapters in the registry (active and inactive).
    function adapterCount() external view returns (uint256) {
        return adapters.length;
    }

    /// @notice Whether the vault has been permanently shut down.
    function isShutdown() external view returns (bool) {
        return shutdown;
    }

    /// @notice Detailed information about a single adapter entry.
    /// @param index         Registry index of the adapter.
    /// @return adapterAddr  Address of the adapter contract.
    /// @return capBps       Maximum allocation cap in basis points.
    /// @return active       Whether the adapter is currently active.
    /// @return currentBalance Live USDC value held by the adapter.
    /// @return targetBps    Current equal-weight target in basis points.
    function getAdapterInfo(uint256 index)
        external
        view
        returns (
            address adapterAddr,
            uint16 capBps,
            bool active,
            uint256 currentBalance,
            uint256 targetBps
        )
    {
        AdapterInfo memory info = adapters[index];
        return (
            address(info.adapter),
            info.capBps,
            info.active,
            info.adapter.totalAssets(),
            _targetBpsForIndex(index)
        );
    }

    /// @notice Compute current vs. target balances and signed drift for every adapter.
    /// @return currentBalances Live USDC values for each adapter (6-decimal units).
    /// @return targetBalances  Equal-weight target USDC values for each adapter.
    /// @return drifts          Signed difference (current − target) per adapter.
    function getAdapterDrift()
        external
        view
        returns (
            uint256[] memory currentBalances,
            uint256[] memory targetBalances,
            int256[] memory drifts
        )
    {
        uint256 len = adapters.length;
        currentBalances = new uint256[](len);
        targetBalances = new uint256[](len);
        drifts = new int256[](len);

        uint256 total = totalAssets();
        uint256 activeCount = _activeAdapterCount();
        uint256 rank;

        for (uint256 i = 0; i < len; i++) {
            if (!adapters[i].active) continue;
            targetBalances[i] = _equalWeightBalance(total, rank, activeCount);
            rank++;
            currentBalances[i] = adapters[i].adapter.totalAssets();
            drifts[i] = int256(currentBalances[i]) - int256(targetBalances[i]);
        }
    }

    /// @notice Whether `minRebalanceInterval` has elapsed since the last rebalance.
    function isRebalanceAvailable() public view returns (bool) {
        return block.timestamp >= lastRebalanceAt + minRebalanceInterval;
    }

    /// @notice Timestamp at which the next rebalance call will be permitted.
    function nextRebalanceAt() external view returns (uint256) {
        return lastRebalanceAt + minRebalanceInterval;
    }

    /// @notice Number of currently active strategy adapters.
    function activeAdapterCount() external view returns (uint256) {
        return _activeAdapterCount();
    }

    /// @notice Equal-weight target allocation per active adapter in basis points.
    /// @dev    The FLOORED base share, `MAX_BPS / activeAdapterCount()`. It is a
    ///         single scalar and so cannot carry the `MAX_BPS % active` remainder
    ///         bps the allocator distributes to the lowest-indexed active
    ///         adapters (#1391) — for the exact per-adapter target read
    ///         `getAdapterInfo(index)` or `getAdapterDrift()`, whose targets sum
    ///         to the whole NAV.
    function currentTargetBps() external view returns (uint256) {
        uint256 active = _activeAdapterCount();
        return active == 0 ? 0 : MAX_BPS / active;
    }
}
