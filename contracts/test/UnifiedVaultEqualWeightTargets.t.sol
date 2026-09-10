// SPDX-License-Identifier: MIT
// Canonical: docs/technical/unified-vault-spec.md §5.6 (rebalance + routing),
//            docs/adr/ADR-0010-unified-vault-architecture.md.
//
// Covers: issue #1396 — `contracts/Vault.sol` carried the same flooring defect
//         #1391 fixed in `RobotMoneyVault`. `_targetBpsFor()` returned the
//         FLOORED `MAX_BPS / active` (3333 for a three-adapter vault), so:
//
//           * `_drawSurplusToIdle` pulled every adapter down to 3333 bps and
//             `_fillDeficitFirst` put the recovered ~1 bps of NAV straight back
//             against the same 9999-bps target set — a pure round trip through
//             the adapters on every `forceRebalance`;
//           * `getAdapterInfo` / `getAdapterDrift` published a target set
//             summing to 9999 bps of NAV, so an off-chain rebalancer read a
//             permanent phantom over-target drift;
//           * `_routeDeposit`'s separate `equalShare = amount / n` floored, so
//             `amount % n` wei always fell through to the leftover-spread pass,
//             which re-read every adapter to place them.
//
// Every test below is RED against the pre-#1396 contract and GREEN after. They
// run in the default `forge test` CI job (suite-01-02-forge-tests.yml) with a
// non-zero executed count — no external resource, no skip.
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

import {Vault} from "../Vault.sol";
import {IPositionAdapter} from "../interfaces/IPositionAdapter.sol";
import {TestERC20} from "./helpers/TestERC20.sol";

// ─── Read-counting exact adapter ─────────────────────────────────────────────

/// @dev A lossless 1:1 `IPositionAdapter` whose `totalAssets()` reads a dedicated
///      storage slot NOTHING else in the contract touches. `totalAssets()` is
///      `view`, so the vault reaches it through `STATICCALL` and it cannot
///      increment a counter itself. Instead a test brackets the call under test
///      with `vm.record()` / `vm.accesses()` and counts `SLOAD`s of `probe`
///      (slot 0) — exactly one per `totalAssets()` call. Mirrors
///      `ReadCountingAdapter` in `RobotMoneyVaultRouteDeposit.t.sol` (#1391),
///      retargeted at the unified `IPositionAdapter` surface. TEST FIXTURE.
contract ProbeHoldAdapter is IPositionAdapter {
    using SafeERC20 for IERC20;

    /// @dev Slot 0 — read ONLY by `totalAssets()`. See `PROBE_SLOT` in the test.
    uint256 private probe;

    address public immutable USDC;
    address public immutable VAULT;
    address internal constant SINK = address(0xdEaD);

    error TokenProtected();

    constructor(address usdc_, address vault_) {
        USDC = usdc_;
        VAULT = vault_;
    }

    modifier onlyVault() {
        if (msg.sender != VAULT) revert OnlyVault();
        _;
    }

    /// @notice Exists so the probe slot is provably writable and the optimizer
    ///         cannot fold its `SLOAD` away as a known zero.
    function setProbe(uint256 probe_) external {
        probe = probe_;
    }

    function deploy(uint256 usdcIn, uint256 minValueOut)
        external
        onlyVault
        returns (uint256 valueAdded)
    {
        valueAdded = usdcIn;
        if (valueAdded < minValueOut) revert SlippageExceeded();
    }

    function withdraw(uint256 usdcWanted, uint256 minUsdcOut)
        external
        onlyVault
        returns (uint256 usdcOut)
    {
        uint256 bal = IERC20(USDC).balanceOf(address(this));
        usdcOut = usdcWanted > bal ? bal : usdcWanted;
        if (usdcOut < minUsdcOut) revert SlippageExceeded();
        IERC20(USDC).safeTransfer(VAULT, usdcOut);
    }

    function totalAssets() external view returns (uint256) {
        return IERC20(USDC).balanceOf(address(this)) + probe;
    }

    function isExact() external pure returns (bool) {
        return true;
    }

    function harvestRewards() external {}

    function sweepForeignToken(address token) external {
        if (token == USDC) revert TokenProtected();
        IERC20(token).safeTransfer(SINK, IERC20(token).balanceOf(address(this)));
    }
}

// ─── Vault harness ───────────────────────────────────────────────────────────

/// @dev Exposes the allocator on its own so a test can count the adapter reads
///      `_routeDeposit` makes without the surrounding `deposit()` NAV reads.
contract EqualWeightHarness is Vault {
    constructor(IERC20 asset_, address feeRecipient_, address admin_, address emergency_)
        Vault(
            asset_,
            "Robot Money USDC",
            "rmUSDC",
            type(uint256).max,
            type(uint256).max,
            0,
            200,
            1e30,
            feeRecipient_,
            admin_,
            emergency_
        )
    {}

    /// @dev Call with `amount` USDC ALREADY transferred into the vault — exactly
    ///      the state `_deposit` hands `_routeDeposit`.
    function exposed_routeDeposit(uint256 amount) external returns (uint256) {
        return _routeDeposit(amount);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

contract UnifiedVaultEqualWeightTargetsTest is Test {
    uint256 internal constant ONE_USDC = 1e6;
    uint256 internal constant MAX_BPS = 10_000;
    uint256 internal constant N = 3;

    /// @dev `ProbeHoldAdapter.probe` — slot 0.
    bytes32 internal constant PROBE_SLOT = bytes32(uint256(0));

    /// @dev Reads per adapter a CORRECT `_routeDeposit` makes: one for the
    ///      `totalAssets()` NAV snapshot at the top of the function, one in
    ///      pass 1. A third read means the leftover-spread pass ran and re-read
    ///      the adapter.
    uint256 internal constant READS_PER_ADAPTER_ONE_ROUND = 2;

    /// @dev Registered cap set. Deliberately NON-BINDING for a three-way equal
    ///      split (5000 bps each vs a ~3333 bps target), so these tests isolate
    ///      the TARGET-SET defect this issue is about. A cap set that sums to
    ///      exactly `MAX_BPS` floors a couple of wei BELOW NAV and makes those
    ///      wei unplaceable whatever the targets say — that interaction has its
    ///      own test below, on `DEVNET_CAPS`.
    uint16[3] internal CAPS = [uint16(5000), uint16(5000), uint16(5000)];

    /// @dev The devnet / `Deploy.s.sol` cap set: 3334 / 3333 / 3333, summing to
    ///      exactly `MAX_BPS`. Reproduced verbatim.
    uint16[3] internal DEVNET_CAPS = [uint16(3334), uint16(3333), uint16(3333)];

    TestERC20 internal usdc;
    EqualWeightHarness internal vault;
    ProbeHoldAdapter[3] internal adapters;

    address internal admin = makeAddr("admin");
    address internal emergency = makeAddr("emergency");
    address internal feeRecipient = makeAddr("feeRecipient");
    address internal alice = makeAddr("alice");

    event Rebalanced(uint256 totalMoved);
    event UnroutedDeposit(uint256 amount);

    function setUp() public {
        usdc = new TestERC20();
        vault = new EqualWeightHarness(IERC20(address(usdc)), feeRecipient, admin, emergency);

        for (uint256 i = 0; i < N; i++) {
            adapters[i] = new ProbeHoldAdapter(address(usdc), address(vault));
            vm.startPrank(admin);
            vault.setAdapterAllowed(address(adapters[i]), true);
            vault.setAdapterCodeHashAllowed(address(adapters[i]).codehash, true);
            vault.addAdapter(address(adapters[i]), CAPS[i], true);
            vm.stopPrank();
        }

        usdc.mint(alice, 10_000_000 * ONE_USDC);
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

    function _adapterBalances() internal view returns (uint256[3] memory bals) {
        for (uint256 i = 0; i < N; i++) {
            bals[i] = usdc.balanceOf(address(adapters[i]));
        }
    }

    function _setCaps(uint16[3] memory caps) internal {
        vm.startPrank(admin);
        for (uint256 i = 0; i < N; i++) {
            vault.setAdapterCap(i, caps[i]);
        }
        vm.stopPrank();
    }

    /// @dev Place `bals` into the adapters by direct transfer and mint shares
    ///      against them, so the composition does not depend on how routing
    ///      happens to split a deposit. Leaves ZERO idle USDC in the vault.
    function _seedComposition(uint256[3] memory bals) internal {
        // A token deposit first, so shares exist and the vault is operating.
        // 3 wei is the smallest deposit the flat split places entirely.
        vm.prank(alice);
        vault.deposit(N, alice);
        assertEq(usdc.balanceOf(address(vault)), 0, "bootstrap left idle USDC");
        for (uint256 i = 0; i < N; i++) {
            uint256 held = usdc.balanceOf(address(adapters[i]));
            assertLe(held, bals[i], "bootstrap overshot the seeded balance");
            vm.prank(alice);
            usdc.transfer(address(adapters[i]), bals[i] - held);
        }
        assertEq(usdc.balanceOf(address(vault)), 0, "seed left idle USDC");
    }

    /// @dev The equal-weight target set the fix defines: base `MAX_BPS / active`
    ///      bps with the `MAX_BPS % active` remainder handed one each to the
    ///      lowest ranks, and each balance taken as the DIFFERENCE OF TWO PREFIX
    ///      SHARES of `total`. Re-derived here independently of the contract.
    function _expectedTargets(uint256 total) internal pure returns (uint256[3] memory t) {
        uint256 base = MAX_BPS / N;
        uint256 extra = MAX_BPS % N;
        for (uint256 r = 0; r < N; r++) {
            uint256 cumBefore = base * r + (r < extra ? r : extra);
            uint256 cumAfter = cumBefore + base + (r < extra ? 1 : 0);
            t[r] = (total * cumAfter) / MAX_BPS - (total * cumBefore) / MAX_BPS;
        }
    }

    // ─── AC 1: one target set that partitions NAV exactly ────────────────────

    /// @notice `getAdapterDrift`'s `targetBalances` MUST sum to the whole NAV.
    ///
    /// @dev RED before #1396: `_targetBpsFor()` returned `10000 / 3 == 3333`, so
    ///      every target was `total * 3333 / 10000` and the three summed to 9999
    ///      bps of NAV — a permanent phantom over-target drift of ~1 bps that no
    ///      rebalance could ever clear.
    function test_getAdapterDrift_targetsPartitionNavExactly() public {
        // 3 000.000001 USDC: NOT a multiple of 10 000, so the per-adapter bps
        // products floor and the prefix-difference form is load-bearing.
        _seedComposition(
            [uint256(1_000 * ONE_USDC), uint256(1_000 * ONE_USDC), uint256(1_000_000_001)]
        );

        uint256 total = vault.totalAssets();
        assertEq(total, 3_000_000_001, "fixture NAV");
        (uint256[] memory current, uint256[] memory targets, int256[] memory drifts) =
            vault.getAdapterDrift();

        uint256[3] memory expected = _expectedTargets(total);
        uint256 sum;
        for (uint256 i = 0; i < N; i++) {
            assertEq(targets[i], expected[i], "target is the exact equal-weight slice");
            assertEq(current[i], usdc.balanceOf(address(adapters[i])), "current balance");
            assertEq(drifts[i], int256(current[i]) - int256(targets[i]), "drift");
            sum += targets[i];
        }
        assertEq(sum, total, "targetBalances MUST partition NAV exactly");
    }

    /// @notice `getAdapterInfo`'s per-adapter `targetBps` MUST sum to `MAX_BPS`.
    ///
    /// @dev RED before #1396: three adapters each reported 3333, summing to 9999.
    function test_getAdapterInfo_targetBpsSumToMaxBps() public view {
        uint256 sum;
        for (uint256 i = 0; i < N; i++) {
            (,,,,, uint256 targetBps) = vault.getAdapterInfo(i);
            sum += targetBps;
        }
        assertEq(sum, MAX_BPS, "per-adapter targetBps MUST sum to MAX_BPS");
    }

    /// @notice The remainder bps go to the LOWEST ranks, one each — the exact
    ///         convention #1391 established for `RobotMoneyVault`.
    function test_getAdapterInfo_remainderBpsGoToLowestRanks() public view {
        (,,,,, uint256 bps0) = vault.getAdapterInfo(0);
        (,,,,, uint256 bps1) = vault.getAdapterInfo(1);
        (,,,,, uint256 bps2) = vault.getAdapterInfo(2);
        assertEq(bps0, 3334, "rank 0 carries the one leftover bps");
        assertEq(bps1, 3333, "rank 1 gets the base share");
        assertEq(bps2, 3333, "rank 2 gets the base share");
    }

    /// @notice An inactive adapter has no target at all, and the survivors still
    ///         split `MAX_BPS` exactly between them.
    function test_getAdapterInfo_inactiveAdapterHasZeroTarget() public {
        vm.prank(admin);
        vault.removeAdapter(2);
        (,,,,, uint256 bps2) = vault.getAdapterInfo(2);
        assertEq(bps2, 0, "inactive adapter target is 0");

        (,,,,, uint256 bps0) = vault.getAdapterInfo(0);
        (,,,,, uint256 bps1) = vault.getAdapterInfo(1);
        assertEq(bps0, 5000, "two-adapter set: rank 0");
        assertEq(bps1, 5000, "two-adapter set: rank 1");
        assertEq(bps0 + bps1, MAX_BPS, "two-adapter set still sums to MAX_BPS");
    }

    // ─── AC 2: forceRebalance stops round-tripping the untargetable bps ──────

    /// @notice A vault sitting EXACTLY on the equal-weight target set must be a
    ///         structural no-op for `forceRebalance`: nothing is drawn to idle,
    ///         nothing is re-routed, and no adapter balance moves.
    ///
    /// @dev THE BOUNDARY TEST. NAV is 30.000000 USDC, whose floored 3333-bps
    ///      target was 9 999 000 per adapter against an exact-partition target
    ///      set of 10 002 000 / 9 999 000 / 9 999 000. RED before #1396: adapter
    ///      0 read as 3 000 wei ABOVE the floored target, `_drawSurplusToIdle`
    ///      pulled those 3 000 wei out, `_fillDeficitFirst` found no deficit
    ///      against the same floored targets and handed them straight back —
    ///      `Rebalanced(3000)`, two adapter round trips, zero net effect.
    ///      GREEN after: `Rebalanced(0)`.
    function test_forceRebalance_atTargetComposition_isANoOp() public {
        uint256 total = 30 * ONE_USDC;
        uint256[3] memory targets = _expectedTargets(total);
        assertEq(targets[0], 10_002_000, "fixture: rank 0 target");
        assertEq(targets[1], 9_999_000, "fixture: rank 1 target");
        assertEq(targets[2], 9_999_000, "fixture: rank 2 target");
        assertEq(targets[0] + targets[1] + targets[2], total, "fixture: exact partition");
        // The floored target the pre-fix code used, for the record: 3 000 wei
        // below rank 0's real target — exactly the amount that round-tripped.
        assertEq((total * (MAX_BPS / N)) / MAX_BPS, 9_999_000, "fixture: floored target");

        _seedComposition(targets);
        assertEq(vault.totalAssets(), total, "fixture NAV");

        uint256[3] memory before = _adapterBalances();

        vm.expectEmit(false, false, false, true, address(vault));
        emit Rebalanced(0);
        vm.prank(admin);
        vault.forceRebalance(0);

        uint256[3] memory afterBals = _adapterBalances();
        for (uint256 i = 0; i < N; i++) {
            assertEq(afterBals[i], before[i], "no adapter balance may move");
        }
        assertEq(usdc.balanceOf(address(vault)), 0, "no USDC left stranded in idle");
        assertEq(vault.totalAssets(), total, "NAV unchanged");
    }

    /// @notice A genuinely drifted vault still rebalances — and lands EXACTLY on
    ///         the published target set, after which a repeat call is the no-op
    ///         the first one made possible.
    ///
    /// @dev RED before #1396 on the SECOND `forceRebalance`: the floored targets
    ///      left ~1 bps of NAV above every adapter's target, so every subsequent
    ///      call kept drawing it to idle and handing it straight back.
    function test_forceRebalance_drifted_landsExactlyOnTheTargetSet() public {
        uint256 total = 30 * ONE_USDC;
        _seedComposition([uint256(20 * ONE_USDC), uint256(7 * ONE_USDC), uint256(3 * ONE_USDC)]);
        assertEq(vault.totalAssets(), total, "fixture NAV");

        vm.prank(admin);
        vault.forceRebalance(0);

        uint256[3] memory expected = _expectedTargets(total);
        uint256[3] memory got = _adapterBalances();
        for (uint256 i = 0; i < N; i++) {
            assertEq(got[i], expected[i], "adapter must land on its exact target");
        }
        assertEq(usdc.balanceOf(address(vault)), 0, "the whole surplus was re-placed");
        assertEq(vault.totalAssets(), total, "NAV non-decreasing and unchanged");

        vm.expectEmit(false, false, false, true, address(vault));
        emit Rebalanced(0);
        vm.prank(admin);
        vault.forceRebalance(0);
    }

    // ─── AC 3: the `amount % n` remainder is placed by pass 1 ────────────────

    /// @notice A deposit whose size is NOT a multiple of the adapter count must
    ///         be split so the slices sum to it EXACTLY — the `amount % n`
    ///         remainder lands in pass 1, not in the leftover-spread pass.
    ///
    /// @dev RED before #1396: `equalShare = amount / n` floored to 1 000 000 000
    ///      each, placing 3 000 000 000 of 3 000 000 002; the 2 leftover wei fell
    ///      through to `_spreadCapHeadroom`, which walked the adapters again and
    ///      dumped BOTH on adapter 0 (the first with headroom) — balances
    ///      1000000002 / 1000000000 / 1000000000 instead of the exact split.
    function test_routeDeposit_remainderIsPlacedByPassOne() public {
        uint256 amount = 3_000_000_002; // 3 000.000002 USDC; amount % 3 == 2

        vm.prank(alice);
        vault.deposit(amount, alice);

        uint256[3] memory got = _adapterBalances();
        // Prefix differences of `amount`: floor(a/3), floor(2a/3)-floor(a/3), a-floor(2a/3).
        assertEq(got[0], 1_000_000_000, "rank 0 slice");
        assertEq(got[1], 1_000_000_001, "rank 1 slice");
        assertEq(got[2], 1_000_000_001, "rank 2 slice");
        assertEq(got[0] + got[1] + got[2], amount, "slices MUST partition the deposit");
        assertEq(usdc.balanceOf(address(vault)), 0, "nothing left idle");
    }

    /// @notice ...and placing that remainder must NOT cost a second full round of
    ///         adapter `totalAssets()` reads.
    ///
    /// @dev THE READ-COUNT BOUNDARY. RED before #1396: the 2 unplaced wei kept
    ///      `remaining > 0`, so the leftover-spread pass re-read every adapter —
    ///      3 reads each, not 2. The reads are counted as `SLOAD`s of each
    ///      adapter's dedicated probe slot, since `totalAssets()` is `view` and
    ///      cannot count itself.
    function test_routeDeposit_remainderDoesNotForceASecondRoundOfReads() public {
        uint256 amount = 3_000_000_002; // amount % 3 == 2

        vm.prank(alice);
        usdc.transfer(address(vault), amount);

        vm.record();
        uint256 unrouted = vault.exposed_routeDeposit(amount);

        assertEq(unrouted, 0, "pass 1 must place the whole deposit");
        for (uint256 i = 0; i < N; i++) {
            assertEq(
                _reads(address(adapters[i])),
                READS_PER_ADAPTER_ONE_ROUND,
                "one NAV read + one pass-1 read; a third means pass 2 re-read it"
            );
        }
    }

    /// @notice A dust deposit SMALLER than the adapter count is still placed by
    ///         pass 1, one wei to each of the trailing ranks (`floor` puts the
    ///         remainder of a prefix split at the END, the mirror image of the
    ///         bps remainder going to the lowest ranks).
    ///
    /// @dev RED before #1396: `equalShare = amount / n == 0` skipped pass 1
    ///      entirely and the whole deposit went through the leftover-spread pass,
    ///      which dumped it on adapter 0 — balances 1 / 1 / 0.
    function test_routeDeposit_dustBelowAdapterCount_isSpreadByPassOne() public {
        vm.prank(alice);
        vault.deposit(2, alice); // 2 wei across 3 adapters

        uint256[3] memory got = _adapterBalances();
        assertEq(got[0], 0, "rank 0's prefix slice is zero");
        assertEq(got[1], 1, "rank 1 gets one wei");
        assertEq(got[2], 1, "rank 2 gets one wei");
        assertEq(usdc.balanceOf(address(vault)), 0, "nothing left idle");
    }

    /// @notice REGRESSION GUARD (not a pre-#1396 red): when the cap set sums to
    ///         exactly `MAX_BPS`, the per-adapter `capBps` balances FLOOR to a
    ///         couple of wei below NAV, so those wei are unplaceable by
    ///         construction. Pass 1 observes zero cap headroom and pass 2 is
    ///         skipped rather than re-reading every adapter to rediscover it —
    ///         the deliberate trade #1391 pinned, ported here with the targets.
    ///         The wei stay idle, are still counted by `totalAssets`, and are
    ///         reported by `UnroutedDeposit`.
    function test_routeDeposit_capFlooredWeiStayIdleWithoutASecondRound() public {
        // NAV 30.000001 USDC: the 3334/3333/3333 cap balances floor to
        // 10 002 000 / 9 999 000 / 9 999 000, which is 30 000 000 — one wei short.
        // Seed under the non-binding caps, THEN tighten to the devnet set, so the
        // bootstrap deposit is not itself cap-bound.
        _seedComposition([uint256(10_002_000), uint256(9_999_000), uint256(9_999_000)]);
        _setCaps(DEVNET_CAPS);

        vm.prank(alice);
        usdc.transfer(address(vault), 1);

        vm.record();
        vm.expectEmit(false, false, false, true, address(vault));
        emit UnroutedDeposit(1);
        uint256 unrouted = vault.exposed_routeDeposit(1);

        assertEq(unrouted, 1, "the cap-floored wei is unplaceable");
        for (uint256 i = 0; i < N; i++) {
            assertEq(
                _reads(address(adapters[i])),
                READS_PER_ADAPTER_ONE_ROUND,
                "pass 2 must be skipped, not walked"
            );
        }
        assertEq(usdc.balanceOf(address(vault)), 1, "the wei stays idle");
        assertEq(vault.totalAssets(), 30_000_001, "and is still counted by NAV");
    }

    // ─── AC 5: no adapter can exceed capBps ──────────────────────────────────

    /// @notice FUZZ (as #1391 did): across arbitrary deposit sizes and cap sets,
    ///         no adapter is ever filled past its `capBps` share of NAV, and the
    ///         published target set always partitions NAV exactly.
    function testFuzz_noAdapterExceedsCapBps(uint96 depositRaw, uint16 c0, uint16 c1, uint16 c2)
        public
    {
        uint256 amount = uint256(depositRaw) % (100_000 * ONE_USDC) + 1;
        _setCaps(
            [
                uint16(bound(c0, 1, MAX_BPS)),
                uint16(bound(c1, 1, MAX_BPS)),
                uint16(bound(c2, 1, MAX_BPS))
            ]
        );

        usdc.mint(alice, amount);
        vm.prank(alice);
        vault.deposit(amount, alice);

        uint256 total = vault.totalAssets();
        for (uint256 i = 0; i < N; i++) {
            (, uint16 capBps,,,,) = vault.getAdapterInfo(i);
            assertLe(
                usdc.balanceOf(address(adapters[i])),
                (total * capBps) / MAX_BPS,
                "adapter filled past its capBps share of NAV"
            );
        }

        (, uint256[] memory targets,) = vault.getAdapterDrift();
        assertEq(targets[0] + targets[1] + targets[2], total, "targets MUST partition NAV");
    }

    /// @notice FUZZ: `forceRebalance` never lifts an adapter past its `capBps`
    ///         share either, never decreases NAV, and always lands the
    ///         composition on the published (exactly-partitioning) target set.
    function testFuzz_forceRebalanceRespectsCapBps(uint96 a0, uint96 a1, uint96 a2) public {
        uint256[3] memory seed = [
            uint256(bound(a0, 1, 100_000 * ONE_USDC)),
            uint256(bound(a1, 1, 100_000 * ONE_USDC)),
            uint256(bound(a2, 1, 100_000 * ONE_USDC))
        ];
        _seedComposition(seed);

        uint256 navBefore = vault.totalAssets();
        vm.prank(admin);
        vault.forceRebalance(0);
        uint256 navAfter = vault.totalAssets();

        assertGe(navAfter, navBefore, "forceRebalance must be NAV non-decreasing");
        uint256[3] memory expected = _expectedTargets(navAfter);
        for (uint256 i = 0; i < N; i++) {
            assertLe(
                usdc.balanceOf(address(adapters[i])),
                (navAfter * CAPS[i]) / MAX_BPS,
                "adapter filled past its capBps share of NAV"
            );
            assertEq(
                usdc.balanceOf(address(adapters[i])), expected[i], "must land on its exact target"
            );
        }
        assertEq(usdc.balanceOf(address(vault)), 0, "nothing left stranded in idle");
    }
}
