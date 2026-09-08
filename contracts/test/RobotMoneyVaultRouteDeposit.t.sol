// SPDX-License-Identifier: MIT
// Canonical: none — Foundry tests for contracts/RobotMoneyVault.sol `_routeDeposit`
// Covers: issue #1391 — `_targetBpsFor()` floored `MAX_BPS / active` to 3333 for a
//         three-adapter vault, so pass 1's per-adapter targets summed to 9999 bps and
//         could never absorb a whole deposit into a balanced, fully-deployed vault.
//         Pass 2 therefore ran on EVERY such deposit and re-read every adapter's
//         `totalAssets()` — the expensive read for adapters wrapping live protocols.
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
import {console2} from "forge-std/console2.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {RobotMoneyVault} from "../RobotMoneyVault.sol";
import {IStrategyAdapter} from "../interfaces/IStrategyAdapter.sol";

// ─── Minimal USDC mock ───────────────────────────────────────────────────────

contract RouteUSDC is ERC20 {
    constructor() ERC20("USD Coin", "USDC") {}

    function decimals() public pure override returns (uint8) {
        return 6;
    }

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}

// ─── Read-counting adapter ───────────────────────────────────────────────────

/// @dev A lossless no-yield adapter whose `totalAssets()` reads a dedicated
///      storage slot NOTHING else in the contract touches. `totalAssets()` is
///      `view`, so the vault reaches it through `STATICCALL` and it cannot
///      increment a counter itself. Instead the test brackets the call under
///      test with `vm.record()` / `vm.accesses()` and counts `SLOAD`s of
///      `probe` (slot 0) — exactly one per `totalAssets()` call.
///
///      `setProbe` exists so the slot is provably writable and the optimizer
///      cannot fold the `SLOAD` away as a known zero.
contract ReadCountingAdapter is IStrategyAdapter {
    using SafeERC20 for IERC20;

    /// @dev Slot 0 — read ONLY by `totalAssets()`. See `PROBE_SLOT` in the test.
    uint256 private probe;

    /// @dev Gas each `totalAssets()` call should burn, so a test can price the
    ///      read at what a real protocol read costs (`MetaMorpho.totalAssets()`
    ///      measures 201 145 gas). `0` keeps the adapter cheap.
    uint256 private readCostGas;

    IERC20 public immutable USDC;
    address public immutable VAULT;

    error OnlyVault();

    modifier onlyVault() {
        if (msg.sender != VAULT) revert OnlyVault();
        _;
    }

    constructor(address usdc_, address vault_) {
        USDC = IERC20(usdc_);
        VAULT = vault_;
    }

    function setProbe(uint256 probe_) external {
        probe = probe_;
    }

    function setReadCostGas(uint256 gas_) external {
        readCostGas = gas_;
    }

    /// @inheritdoc IStrategyAdapter
    function deploy(uint256) external onlyVault {}

    /// @inheritdoc IStrategyAdapter
    function withdraw(uint256 amount) external onlyVault returns (uint256 actual) {
        uint256 bal = USDC.balanceOf(address(this));
        actual = amount > bal ? bal : amount;
        if (actual > 0) USDC.safeTransfer(VAULT, actual);
    }

    /// @inheritdoc IStrategyAdapter
    function totalAssets() external view returns (uint256) {
        uint256 cost = readCostGas;
        if (cost != 0) {
            uint256 start = gasleft();
            // `gasleft()` is a side-effecting opcode, so the optimizer cannot
            // fold this spin away.
            while (start - gasleft() < cost) {}
        }
        return USDC.balanceOf(address(this)) + probe;
    }

    /// @inheritdoc IStrategyAdapter
    function sweepForeignToken(address) external {}

    /// @inheritdoc IStrategyAdapter
    function harvestRewards() external {}
}

// ─── Vault harness ───────────────────────────────────────────────────────────

/// @dev Exposes the allocator on its own so a test can count the adapter reads
///      `_routeDeposit` makes, without the surrounding `deposit()` NAV reads.
contract RouteHarness is RobotMoneyVault {
    constructor(
        IERC20 asset_,
        uint256 tvlCap_,
        uint256 perDepositCap_,
        address feeRecipient_,
        address admin_
    ) RobotMoneyVault(asset_, tvlCap_, perDepositCap_, 0, feeRecipient_, admin_, admin_) {}

    /// @dev Call with `amount` USDC ALREADY transferred into the vault — exactly
    ///      the state `_deposit` hands `_routeDeposit`.
    function exposed_routeDeposit(uint256 amount) external {
        _routeDeposit(amount);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

contract RobotMoneyVaultRouteDepositTest is Test {
    uint256 internal constant ONE_USDC = 1e6;
    uint256 internal constant TVL_CAP = 1_000_000_000 * ONE_USDC;
    uint256 internal constant PER_DEPOSIT_CAP = 100_000_000 * ONE_USDC;
    uint256 internal constant MAX_BPS = 10_000;

    /// @dev `ReadCountingAdapter.probe` — slot 0.
    bytes32 internal constant PROBE_SLOT = bytes32(uint256(0));

    /// @dev Reads per adapter that a CORRECT `_routeDeposit` makes: one for the
    ///      `totalAssets()` NAV snapshot at the top of the function, one in
    ///      pass 1. A third read means pass 2 ran and re-read the adapter.
    uint256 internal constant READS_PER_ADAPTER_ONE_ROUND = 2;

    /// @dev `MetaMorpho.totalAssets()` measured against the committed fork
    ///      fixture (issue #1391 / PR #1394). Used to price the mock adapter's
    ///      read at what the production read actually costs.
    uint256 internal constant METAMORPHO_TOTAL_ASSETS_GAS = 201_145;

    /// @dev The devnet / `Deploy.s.sol` cap set: 3334 / 3333 / 3333, summing to
    ///      exactly `MAX_BPS`. Reproduced verbatim so the test models the real
    ///      three-adapter vault the out-of-gas failure was observed on.
    uint16[3] internal CAPS = [uint16(3334), uint16(3333), uint16(3333)];

    RouteUSDC internal usdc;
    RouteHarness internal vault;
    ReadCountingAdapter[3] internal adapters;

    address internal admin = makeAddr("admin");
    address internal feeRecipient = makeAddr("feeRecipient");
    address internal alice = makeAddr("alice");

    event Allocated(uint256 indexed index, address indexed adapter, uint256 amount);
    event UnroutedDeposit(uint256 amount);

    function setUp() public {
        usdc = new RouteUSDC();
        vault = new RouteHarness(
            IERC20(address(usdc)), TVL_CAP, PER_DEPOSIT_CAP, feeRecipient, admin
        );

        for (uint256 i = 0; i < 3; i++) {
            adapters[i] = new ReadCountingAdapter(address(usdc), address(vault));
            vm.startPrank(admin);
            vault.setAdapterAllowed(address(adapters[i]), true);
            vault.setAdapterCodeHashAllowed(address(adapters[i]).codehash, true);
            vault.addAdapter(address(adapters[i]), CAPS[i]);
            vm.stopPrank();
        }

        usdc.mint(alice, 1_000_000 * ONE_USDC);
        vm.prank(alice);
        usdc.approve(address(vault), type(uint256).max);
    }

    // ─── Helpers ─────────────────────────────────────────────────────────────

    /// @dev Number of `totalAssets()` calls the recorded window made against
    ///      `adapter_`, counted as `SLOAD`s of its dedicated probe slot.
    function _reads(address adapter_) internal view returns (uint256 n) {
        (bytes32[] memory slots,) = vm.accesses(adapter_);
        for (uint256 i = 0; i < slots.length; i++) {
            if (slots[i] == PROBE_SLOT) n++;
        }
    }

    /// @dev Bring the vault to the steady state the failure was observed in: a
    ///      balanced, fully-deployed three-adapter vault with no idle USDC.
    function _bootstrapBalancedVault(uint256 seed) internal {
        vm.prank(alice);
        vault.deposit(seed, alice);
        assertEq(usdc.balanceOf(address(vault)), 0, "bootstrap left idle USDC");
    }

    /// @dev Fund the vault with `amount` idle USDC and run the allocator alone.
    function _routeAlone(uint256 amount) internal {
        vm.prank(alice);
        usdc.transfer(address(vault), amount);
        vm.record();
        vault.exposed_routeDeposit(amount);
    }

    function _adapterBalances() internal view returns (uint256[3] memory bals) {
        for (uint256 i = 0; i < 3; i++) {
            bals[i] = usdc.balanceOf(address(adapters[i]));
        }
    }

    // ─── The defect ──────────────────────────────────────────────────────────

    /// @notice `_routeDeposit` into a balanced, fully-deployed vault must make
    ///         exactly ONE round of adapter reads in the allocator: pass 1
    ///         places the whole deposit and pass 2 never runs.
    ///
    /// @dev RED before #1391: `_targetBpsFor()` returned `10000 / 3 == 3333`, so
    ///      the three pass-1 targets summed to 9999 bps. Pass 1 could never place
    ///      the last ~1 bps of TVL, `remaining` stayed positive, and pass 2
    ///      walked all three adapters a second time — 3 reads each, not 2.
    function test_routeDeposit_balancedVault_readsEachAdapterOnce() public {
        _bootstrapBalancedVault(3_000 * ONE_USDC);

        _routeAlone(5 * ONE_USDC);

        for (uint256 i = 0; i < 3; i++) {
            assertEq(
                _reads(address(adapters[i])),
                READS_PER_ADAPTER_ONE_ROUND,
                "pass 2 ran and re-read the adapter"
            );
        }
        assertEq(usdc.balanceOf(address(vault)), 0, "routing left idle USDC");
    }

    /// @notice Pass 1 alone absorbs the entire deposit into a balanced vault: no
    ///         adapter is allocated to twice and nothing is left unrouted.
    /// @dev RED before #1391 — pass 2 allocated the 9999-vs-10000 shortfall,
    ///      producing a second `Allocated` for the first adapter with headroom.
    function test_routeDeposit_balancedVault_passOneCompletesTheFill() public {
        _bootstrapBalancedVault(3_000 * ONE_USDC);

        uint256[3] memory balancesBefore = _adapterBalances();

        vm.recordLogs();
        vm.prank(alice);
        vault.deposit(5 * ONE_USDC, alice);

        uint256 allocations;
        uint256 unrouted;
        bool[3] memory seen;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            if (logs[i].emitter != address(vault)) continue;
            if (logs[i].topics[0] == Allocated.selector) {
                uint256 index = uint256(logs[i].topics[1]);
                assertFalse(seen[index], "adapter allocated to twice: pass 2 ran");
                seen[index] = true;
                allocations++;
            } else if (logs[i].topics[0] == UnroutedDeposit.selector) {
                unrouted = abi.decode(logs[i].data, (uint256));
            }
        }

        assertEq(allocations, 3, "pass 1 did not fill every adapter");
        assertEq(unrouted, 0, "deposit left USDC unrouted");
        assertEq(usdc.balanceOf(address(vault)), 0, "deposit left idle USDC in the vault");

        uint256[3] memory balancesAfter = _adapterBalances();
        uint256 moved;
        for (uint256 i = 0; i < 3; i++) {
            moved += balancesAfter[i] - balancesBefore[i];
        }
        assertEq(moved, 5 * ONE_USDC, "adapters did not receive the whole deposit");
    }

    /// @notice A deposit smaller than 1 bps of TVL is the regime in which pass 1
    ///         placed literally NOTHING before #1391: every adapter already held
    ///         at least the floored 3333 bps target, so all three `continue`d and
    ///         pass 2 did the entire job on a second round of reads.
    function test_routeDeposit_dustDeposit_isPlacedByPassOne() public {
        _bootstrapBalancedVault(3_000 * ONE_USDC);

        // 0.10 USDC against 3 000 USDC TVL is ~0.33 bps — far below the 1 bps
        // shortfall the floored target set could never close.
        uint256 dust = ONE_USDC / 10;

        vm.recordLogs();
        _routeAlone(dust);

        for (uint256 i = 0; i < 3; i++) {
            assertEq(
                _reads(address(adapters[i])),
                READS_PER_ADAPTER_ONE_ROUND,
                "pass 2 ran and re-read the adapter"
            );
        }

        uint256 allocations;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            if (logs[i].emitter == address(vault) && logs[i].topics[0] == Allocated.selector) {
                allocations++;
            }
        }
        assertGt(allocations, 0, "pass 1 placed nothing at all");
        assertEq(usdc.balanceOf(address(vault)), 0, "dust deposit left idle USDC");
    }

    // ─── Invariants the fix must preserve ────────────────────────────────────

    /// @notice The per-adapter equal-weight targets sum to the whole NAV. Before
    ///         #1391 they summed to 9999 bps of it, leaving 1 bps permanently
    ///         un-targetable.
    /// @dev Asserted through the public drift view so the claim is about
    ///      observable contract state, not an internal helper.
    function test_targetBalances_sumToTotalAssets() public {
        _bootstrapBalancedVault(3_000 * ONE_USDC);

        (, uint256[] memory targets,) = vault.getAdapterDrift();
        uint256 sum;
        for (uint256 i = 0; i < targets.length; i++) {
            sum += targets[i];
        }

        uint256 total = vault.totalAssets();
        // Per-adapter flooring of `total * bps / MAX_BPS` can lose at most 1 wei
        // per adapter; anything more means the bps themselves do not sum to 100%.
        assertLe(sum, total, "targets sum above totalAssets");
        assertGe(sum + targets.length, total, "targets sum more than dust below totalAssets");
    }

    /// @notice No adapter may ever be pushed above its `capBps` share of NAV —
    ///         distributing the rounding remainder must go through
    ///         `min(capBps, targetBps)`, never around it.
    function test_routeDeposit_neverExceedsCapBps() public {
        _bootstrapBalancedVault(3_000 * ONE_USDC);
        vm.prank(alice);
        vault.deposit(5 * ONE_USDC, alice);

        uint256 total = vault.totalAssets();
        for (uint256 i = 0; i < 3; i++) {
            uint256 capBalance = (total * CAPS[i]) / MAX_BPS;
            assertLe(usdc.balanceOf(address(adapters[i])), capBalance, "adapter above capBps");
        }
    }

    /// @notice Fuzzed: routing any deposit into any starting composition must
    ///         never push an adapter above its `capBps` share of NAV.
    function testFuzz_routeDeposit_neverExceedsCapBps(uint256 seed, uint256 amount) public {
        seed = bound(seed, 1 * ONE_USDC, 100_000 * ONE_USDC);
        amount = bound(amount, 1, 100_000 * ONE_USDC);

        vm.prank(alice);
        vault.deposit(seed, alice);
        vm.prank(alice);
        vault.deposit(amount, alice);

        uint256 total = vault.totalAssets();
        for (uint256 i = 0; i < 3; i++) {
            assertLe(
                usdc.balanceOf(address(adapters[i])),
                (total * CAPS[i]) / MAX_BPS,
                "adapter above capBps"
            );
        }
    }

    /// @notice A tight cap still binds: when one adapter's `capBps` is below its
    ///         equal-weight share, pass 1 stops at the cap and pass 2 places the
    ///         leftover in an adapter that still has absolute headroom.
    function test_routeDeposit_tightCapBindsAndLeftoverGoesElsewhere() public {
        vm.startPrank(admin);
        vault.setAdapterCap(0, 10_000); // absorbs whatever the tight cap rejects
        vault.setAdapterCap(2, 100); // 1% cap on the last adapter
        vm.stopPrank();

        vm.prank(alice);
        vault.deposit(3_000 * ONE_USDC, alice);

        uint256 total = vault.totalAssets();
        assertLe(
            usdc.balanceOf(address(adapters[2])),
            (total * 100) / MAX_BPS,
            "capped adapter above its capBps"
        );
        assertEq(usdc.balanceOf(address(vault)), 0, "capped routing left idle USDC");
    }

    /// @notice When pass 2 does have to run, an adapter that pass 1 SKIPPED is
    ///         not re-read: no allocation touched it, so its balance cannot have
    ///         moved and the pass-1 read is reused (#1391 caching remedy).
    function test_routeDeposit_passTwoReusesReadsForAdaptersPassOneSkipped() public {
        vm.startPrank(admin);
        vault.setAdapterCap(0, 10_000);
        vault.setAdapterCap(2, 100);
        vm.stopPrank();

        // Fill adapter 2 to its 1% cap and leave the vault fully deployed, so a
        // follow-up deposit makes pass 1 skip it while `remaining` is still > 0.
        vm.prank(alice);
        vault.deposit(3_000 * ONE_USDC, alice);
        assertEq(usdc.balanceOf(address(vault)), 0, "setup left idle USDC");

        _routeAlone(500 * ONE_USDC);

        assertEq(_reads(address(adapters[2])), 1, "adapter pass 1 skipped was re-read by pass 2");
    }

    // ─── Recorded gas for the ordinary balanced-vault deposit path ───────────

    /// @notice Not an assertion — records the headline number for #1391: gas for
    ///         one ordinary deposit into a balanced, fully-deployed three-adapter
    ///         vault, plus the adapter `totalAssets()` reads it pays for.
    function test_gas_balancedVaultDeposit() public {
        _bootstrapBalancedVault(3_000 * ONE_USDC);

        vm.prank(alice);
        vm.record();
        uint256 gasBefore = gasleft();
        vault.deposit(5 * ONE_USDC, alice);
        uint256 gasUsed = gasBefore - gasleft();

        uint256 reads;
        for (uint256 i = 0; i < 3; i++) {
            reads += _reads(address(adapters[i]));
        }

        console2.log("balanced-vault deposit gas", gasUsed);
        console2.log("balanced-vault deposit adapter totalAssets() reads", reads);
    }

    /// @notice Not an assertion — the same ordinary balanced-vault deposit with
    ///         each adapter's `totalAssets()` priced at the 201 145 gas
    ///         `MetaMorpho.totalAssets()` measures on the committed fork fixture,
    ///         so the recorded number reflects the production deposit path rather
    ///         than a free mock read.
    function test_gas_balancedVaultDeposit_realisticAdapterReadCost() public {
        _bootstrapBalancedVault(3_000 * ONE_USDC);

        for (uint256 i = 0; i < 3; i++) {
            adapters[i].setReadCostGas(METAMORPHO_TOTAL_ASSETS_GAS);
        }

        vm.prank(alice);
        uint256 gasBefore = gasleft();
        vault.deposit(5 * ONE_USDC, alice);
        uint256 gasUsed = gasBefore - gasleft();

        console2.log("balanced-vault deposit gas (adapter reads priced at 201145)", gasUsed);
    }
}
