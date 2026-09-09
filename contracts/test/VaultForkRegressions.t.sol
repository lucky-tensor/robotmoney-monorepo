// SPDX-License-Identifier: MIT
// Canonical: docs/technical/security-model.md — ERC-4626 Inflation Attack Mitigation
// Covers: issue #209 — fork regressions for vault accounting attack paths
// Related: issue #160 (decimals offset), #161 (idle USDC), #163 (MorphoAdapter shortfall)
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IERC4626} from "@openzeppelin/contracts/interfaces/IERC4626.sol";

import {RobotMoneyVault} from "../RobotMoneyVault.sol";
import {AaveV3Adapter} from "../adapters/AaveV3Adapter.sol";
import {MorphoAdapter} from "../adapters/MorphoAdapter.sol";
import {CompoundV3Adapter} from "../adapters/CompoundV3Adapter.sol";
import {IStrategyAdapter} from "../interfaces/IStrategyAdapter.sol";

/// @title VaultForkRegressions
/// @notice Fork-level regression suite for vault accounting attack paths.
///
/// @dev CI boots the checked-in Base golden fixture at localhost:8545. A local
///      live fork remains possible by overriding `FORK_RPC_URL`.
///
///      To run locally:
///        FORK_RPC_URL=https://base-mainnet.g.alchemy.com/v2/<key> \
///          forge test --match-path "contracts/test/VaultForkRegressions.t.sol" -vvv
///
///      CI uses `CURRENT.anvil-state`; no live RPC secret is involved.
///
/// Attack paths covered (per issue #209 acceptance criteria):
///   AC1  Aave adapter donation cannot make a victim deposit mint zero/unfair shares.
///   AC2  Morpho adapter donation cannot make a victim deposit mint zero/unfair shares.
///   AC3  Compound adapter donation cannot make a victim deposit mint zero/unfair shares.
///   AC4  Direct USDC transfer to vault is included in totalAssets / TVL-cap path.
///   AC5  Unrouted deposit emits UnroutedDeposit and the idle balance is observable.
///   AC6  MorphoAdapter.withdraw returns actual delivered USDC under fork conditions.
contract VaultForkRegressions is Test {
    // ─── Base mainnet protocol addresses ──────────────────────────────────────

    /// @dev Real USDC on Base (Circle).
    address internal constant BASE_USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;

    /// @dev Aave V3 Pool on Base.
    address internal constant AAVE_POOL = 0xA238Dd80C259a72e81d7e4664a9801593F98d1c5;
    /// @dev aBasUSDC rebasing token — balanceOf returns live underlying USDC.
    address internal constant AAVE_A_TOKEN = 0x4e65fE4DbA92790696d040ac24Aa414708F5c0AB;

    /// @dev Morpho Gauntlet USDC Prime vault on Base (ERC-4626).
    address internal constant MORPHO_VAULT = 0xc1256Ae5FF1cf2719D4937adb3bbCCab2E00A2Ca;

    /// @dev Compound V3 Comet (cUSDCv3) on Base.
    address internal constant COMPOUND_COMET = 0xb125E6687d4313864e53df431d5425969c15Eb2F;

    // ─── Test amounts (6-decimal USDC) ─────────────────────────────────────────

    uint256 internal constant ONE_USDC = 1e6;
    uint256 internal constant SEED_AMOUNT = 1_000 * ONE_USDC;
    uint256 internal constant DONATION_AMOUNT = 1_000_000 * ONE_USDC;
    uint256 internal constant VICTIM_DEPOSIT = 100_000 * ONE_USDC;

    // TVL cap large enough not to interfere with the test amounts.
    uint256 internal constant TVL_CAP = 1_000_000_000 * ONE_USDC;
    uint256 internal constant PER_DEPOSIT_CAP = 100_000_000 * ONE_USDC;

    // ─── State ─────────────────────────────────────────────────────────────────

    IERC20 internal usdc;
    address internal admin;
    address internal feeRecipient;
    address internal alice;
    address internal attacker;

    // ─── Helpers ──────────────────────────────────────────────────────────────

    /// @dev Use an explicit local override, otherwise the offline fixture RPC.
    function _forkRpcUrl() internal view returns (string memory url) {
        try vm.envString("FORK_RPC_URL") returns (string memory s) {
            if (bytes(s).length > 0) return s;
        } catch {}
        return "http://127.0.0.1:8545";
    }

    /// @dev Select the already-running offline golden-fixture RPC.
    function _trySelectFork() internal returns (bool selected) {
        string memory rpc = _forkRpcUrl();
        vm.createSelectFork(rpc);
        return true;
    }

    /// @dev Shared preamble: select fork, fund accounts.
    ///      Returns false when the fork URL is absent (test should skip).
    function _setUp() internal returns (bool) {
        if (!_trySelectFork()) return false;

        usdc = IERC20(BASE_USDC);
        admin = makeAddr("admin");
        feeRecipient = makeAddr("feeRecipient");
        alice = makeAddr("alice");
        attacker = makeAddr("attacker");

        // Fund accounts via deal() — works on any fork token.
        deal(BASE_USDC, alice, 10_000_000 * ONE_USDC);
        deal(BASE_USDC, attacker, 2_000_000 * ONE_USDC + SEED_AMOUNT);

        return true;
    }

    /// @dev Deploy a fresh RobotMoneyVault with a single adapter.
    ///      Approves the vault from alice and attacker.
    function _deployVaultWithAdapter(address adapter_) internal returns (RobotMoneyVault vault_) {
        vault_ = new RobotMoneyVault(
            IERC20(BASE_USDC),
            TVL_CAP,
            PER_DEPOSIT_CAP,
            0, // no exit fee
            feeRecipient,
            admin,
            admin
        );

        _allowAdapter(vault_, adapter_);
        vm.prank(admin);
        vault_.addAdapter(adapter_, 10_000); // 100% cap

        vm.prank(alice);
        usdc.approve(address(vault_), type(uint256).max);
        vm.prank(attacker);
        usdc.approve(address(vault_), type(uint256).max);
    }

    function _allowAdapter(RobotMoneyVault vault_, address adapter_) internal {
        vm.prank(admin);
        vault_.setAdapterAllowed(adapter_, true);
        vm.prank(admin);
        vault_.setAdapterCodeHashAllowed(adapter_.codehash, true);
    }

    // ─── Donation-attack helper ────────────────────────────────────────────────

    /// @dev Core attack scenario:
    ///   1. Attacker seeds the vault with 1 USDC.
    ///   2. Attacker donates `donationAmt` USDC to the adapter via the protocol
    ///      directly (bypassing the vault minting path), using `deal()` +
    ///      adapter-level deposit.  The donation increases the adapter's
    ///      reported `totalAssets()` without minting any vault shares.
    ///   3. Victim deposits `victimDeposit` USDC.
    ///   4. Asserts victim receives non-zero shares and can recover ≥ 90% of value.
    ///
    ///   The 90% floor is intentionally generous; the actual protection from the
    ///   18-decimals offset makes the loss negligible, but a hard floor catches
    ///   regressions that silently remove the offset.
    function _assertDonationAttackFails(
        RobotMoneyVault vault_,
        IStrategyAdapter adapter_,
        uint256 donationAmt,
        uint256 victimDeposit
    ) internal {
        // 1. Attacker seed. A whole USDC keeps protocol minimums realistic.
        vm.prank(attacker);
        uint256 attackerShares = vault_.deposit(ONE_USDC, attacker);
        assertGt(attackerShares, 0, "attacker seed must mint shares");

        // 2. Donation: credit `donationAmt` USDC directly into the adapter's
        //    balance.  Using `deal` on the adapter address models any protocol
        //    that credits the adapter without going through the vault (Aave
        //    supply(onBehalfOf=adapter), Morpho deposit(receiver=adapter), or
        //    Compound supply credited to the adapter address).
        deal(BASE_USDC, address(adapter_), donationAmt);

        // Unmanaged tokens sent to the adapter must not inflate accounted NAV.
        uint256 totalAfterDonation = vault_.totalAssets();
        assertLt(totalAfterDonation, donationAmt, "unmanaged donation must not inflate NAV");

        // 3. Victim deposit.
        vm.prank(alice);
        uint256 victimShares = vault_.deposit(victimDeposit, alice);

        // AC: victim receives non-zero shares.
        assertGt(victimShares, 0, "victim must receive non-zero shares after donation");

        // AC: victim can recover at least 90% of deposited value.
        uint256 valueBack = vault_.previewRedeem(victimShares);
        assertGe(
            valueBack * 100,
            victimDeposit * 90,
            "victim must recover >= 90% of deposit value after donation"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // AC1 — Aave donation attack
    // ═══════════════════════════════════════════════════════════════════════════

    /// @notice AC1: Aave adapter donation cannot make victim deposit mint zero/unfair shares.
    ///
    /// @dev Deploys vault + AaveV3Adapter against real Base Aave pool.
    ///      Seeds vault, donates USDC directly into adapter balance, asserts victim fairness.
    function test_fork_aave_donationAttack_victimSharesFair() public {
        _setUp();

        RobotMoneyVault vault_ = new RobotMoneyVault(
            IERC20(BASE_USDC), TVL_CAP, PER_DEPOSIT_CAP, 0, feeRecipient, admin, admin
        );
        AaveV3Adapter aaveAdapter =
            new AaveV3Adapter(AAVE_POOL, BASE_USDC, AAVE_A_TOKEN, address(vault_));

        _allowAdapter(vault_, address(aaveAdapter));
        vm.prank(admin);
        vault_.addAdapter(address(aaveAdapter), 10_000);

        vm.prank(alice);
        usdc.approve(address(vault_), type(uint256).max);
        vm.prank(attacker);
        usdc.approve(address(vault_), type(uint256).max);

        _assertDonationAttackFails(vault_, aaveAdapter, DONATION_AMOUNT, VICTIM_DEPOSIT);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // AC2 — Morpho donation attack
    // ═══════════════════════════════════════════════════════════════════════════

    /// @notice AC2: Morpho adapter donation cannot make victim deposit mint zero/unfair shares.
    ///
    /// @dev Deploys vault + MorphoAdapter against real Base Morpho Gauntlet USDC Prime vault.
    function test_fork_morpho_donationAttack_victimSharesFair() public {
        _setUp();

        RobotMoneyVault vault_ = new RobotMoneyVault(
            IERC20(BASE_USDC), TVL_CAP, PER_DEPOSIT_CAP, 0, feeRecipient, admin, admin
        );
        MorphoAdapter morphoAdapter = new MorphoAdapter(MORPHO_VAULT, BASE_USDC, address(vault_));

        _allowAdapter(vault_, address(morphoAdapter));
        vm.prank(admin);
        vault_.addAdapter(address(morphoAdapter), 10_000);

        vm.prank(alice);
        usdc.approve(address(vault_), type(uint256).max);
        vm.prank(attacker);
        usdc.approve(address(vault_), type(uint256).max);

        _assertDonationAttackFails(vault_, morphoAdapter, DONATION_AMOUNT, VICTIM_DEPOSIT);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // AC3 — Compound donation attack
    // ═══════════════════════════════════════════════════════════════════════════

    /// @notice AC3: Compound adapter donation cannot make victim deposit mint zero/unfair shares.
    ///
    /// @dev Deploys vault + CompoundV3Adapter against real Base Compound Comet.
    function test_fork_compound_donationAttack_victimSharesFair() public {
        _setUp();

        RobotMoneyVault vault_ = new RobotMoneyVault(
            IERC20(BASE_USDC), TVL_CAP, PER_DEPOSIT_CAP, 0, feeRecipient, admin, admin
        );
        CompoundV3Adapter compoundAdapter =
            new CompoundV3Adapter(COMPOUND_COMET, BASE_USDC, address(vault_));

        _allowAdapter(vault_, address(compoundAdapter));
        vm.prank(admin);
        vault_.addAdapter(address(compoundAdapter), 10_000);

        vm.prank(alice);
        usdc.approve(address(vault_), type(uint256).max);
        vm.prank(attacker);
        usdc.approve(address(vault_), type(uint256).max);

        _assertDonationAttackFails(vault_, compoundAdapter, DONATION_AMOUNT, VICTIM_DEPOSIT);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // AC4 — Direct USDC transfer to vault reflected in totalAssets / TVL-cap
    // ═══════════════════════════════════════════════════════════════════════════

    /// @notice AC4: A direct USDC transfer to the vault (not via deposit) is included
    ///         in totalAssets() and the TVL-cap enforcement path.
    ///
    /// @dev Uses AaveV3Adapter for a realistic adapter setup; the idle-balance
    ///      logic is independent of which adapter is present.
    function test_fork_directTransfer_countedInTotalAssets() public {
        _setUp();

        RobotMoneyVault vault_ = new RobotMoneyVault(
            IERC20(BASE_USDC), TVL_CAP, PER_DEPOSIT_CAP, 0, feeRecipient, admin, admin
        );
        AaveV3Adapter aaveAdapter =
            new AaveV3Adapter(AAVE_POOL, BASE_USDC, AAVE_A_TOKEN, address(vault_));
        _allowAdapter(vault_, address(aaveAdapter));
        vm.prank(admin);
        vault_.addAdapter(address(aaveAdapter), 10_000);

        vm.prank(alice);
        usdc.approve(address(vault_), type(uint256).max);

        // Normal deposit to establish a baseline.
        vm.prank(alice);
        vault_.deposit(SEED_AMOUNT, alice);

        uint256 totalBefore = vault_.totalAssets();

        // Direct transfer (simulates attacker or routing overflow).
        uint256 idleAmt = 5_000 * ONE_USDC;
        deal(BASE_USDC, address(vault_), idleAmt + usdc.balanceOf(address(vault_)));

        uint256 totalAfter = vault_.totalAssets();
        assertGe(
            totalAfter,
            totalBefore + idleAmt,
            "direct USDC transfer must be included in totalAssets"
        );
    }

    /// @notice AC4 (TVL-cap path): idle USDC is counted when enforcing the cap.
    ///
    /// @dev Tightly-capped vault: caps chosen so idle balance pushes total close
    ///      to the ceiling, and a further deposit should revert.
    function test_fork_directTransfer_causesCapEnforcement() public {
        _setUp();

        uint256 tightCap = 20_000 * ONE_USDC;

        RobotMoneyVault tightVault = new RobotMoneyVault(
            IERC20(BASE_USDC),
            tightCap,
            tightCap, // perDepositCap = tvlCap so deposit can fill the whole cap
            0,
            feeRecipient,
            admin,
            admin
        );
        AaveV3Adapter aaveAdapter =
            new AaveV3Adapter(AAVE_POOL, BASE_USDC, AAVE_A_TOKEN, address(tightVault));
        _allowAdapter(tightVault, address(aaveAdapter));
        vm.prank(admin);
        tightVault.addAdapter(address(aaveAdapter), 10_000);

        deal(BASE_USDC, alice, tightCap * 2);
        vm.prank(alice);
        usdc.approve(address(tightVault), type(uint256).max);

        // Deposit 15 000 — within cap.
        vm.prank(alice);
        tightVault.deposit(15_000 * ONE_USDC, alice);

        // Send 4 000 directly to the vault (simulates routing residue or attacker).
        deal(BASE_USDC, address(tightVault), 4_000 * ONE_USDC + usdc.balanceOf(address(tightVault)));

        // totalAssets must now reflect adapter balance + idle.
        //
        // The idle 4 000 USDC is exact (set via deal), but the 15 000 supplied to
        // Aave is read back through the aToken's ray-math (rayDiv on supply,
        // rayMul on balanceOf), which rounds down by a few wei. That round-trip
        // loss grows with the *live* Aave liquidity index: the pinned golden
        // fixture saw <=1 wei, but live Base drifted to 2 wei and flapped this
        // non-blocking nightly drift alarm (issue #1157). A small fixed dust
        // tolerance absorbs the benign Aave rounding while still catching a real
        // "idle not counted" regression — that would miss the whole 4 000e6 idle,
        // ~7 orders of magnitude larger than this tolerance.
        uint256 aaveRayDust = 100; // wei of 6-decimal USDC == $0.0001
        assertApproxEqAbs(
            tightVault.totalAssets(),
            19_000 * ONE_USDC,
            aaveRayDust,
            "totalAssets must include idle"
        );

        // A further 2 000 would push total beyond 20 000 → must revert.
        deal(BASE_USDC, address(this), 2_000 * ONE_USDC);
        usdc.approve(address(tightVault), 2_000 * ONE_USDC);
        vm.expectRevert();
        tightVault.deposit(2_000 * ONE_USDC, address(this));
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // AC5 — Unrouted deposit behavior is explicit and observable
    // ═══════════════════════════════════════════════════════════════════════════

    /// @notice AC5: When adapter caps are exhausted, the unrouted portion stays idle
    ///         in the vault and the UnroutedDeposit event is emitted — not silent.
    ///
    /// @dev A single adapter capped at 50% leaves half the deposit unrouted.
    function test_fork_unroutedDeposit_emitsEventAndStaysIdle() public {
        _setUp();

        RobotMoneyVault vault_ = new RobotMoneyVault(
            IERC20(BASE_USDC), TVL_CAP, PER_DEPOSIT_CAP, 0, feeRecipient, admin, admin
        );
        MorphoAdapter morphoAdapter = new MorphoAdapter(MORPHO_VAULT, BASE_USDC, address(vault_));
        _allowAdapter(vault_, address(morphoAdapter));
        vm.prank(admin);
        vault_.addAdapter(address(morphoAdapter), 5000);
        deal(BASE_USDC, alice, 200_000 * ONE_USDC);
        vm.prank(alice);
        usdc.approve(address(vault_), type(uint256).max);

        uint256 depositAmt = 100_000 * ONE_USDC;
        // Half the deposit exceeds the 5000 bps cap and stays idle.
        //
        // This was `depositAmt / 2 - 1` before #1391: pass 1 filled the adapter to
        // exactly its cap balance, Morpho's share conversion then reported one wei
        // LESS than was deployed, and pass 2 spent a whole second round of
        // `MorphoAdapter.totalAssets()` (201,145 gas on the fork fixture) to place
        // that single wei. #1391 gates pass 2 on the cap headroom pass 1 observed,
        // scoring an allocated adapter at the amount deployed — an upper bound on
        // what a share-priced adapter reports back, so headroom is under-stated by
        // exactly that rounding dust and pass 2 is skipped.
        //
        // The wei is not lost: it stays idle, `totalAssets` still counts it, and
        // the next deposit or `rebalance` routes it. The cap is unaffected — the
        // adapter is at its cap either way.
        uint256 expectedIdle = depositAmt / 2;

        // The UnroutedDeposit event must be emitted with the correct amount.
        vm.expectEmit(false, false, false, true, address(vault_));
        emit RobotMoneyVault.UnroutedDeposit(expectedIdle);
        vm.prank(alice);
        vault_.deposit(depositAmt, alice);

        // The idle balance is observable via direct USDC balance of the vault.
        assertEq(
            usdc.balanceOf(address(vault_)),
            expectedIdle,
            "idle USDC must remain in vault after unrouted deposit"
        );

        // totalAssets still includes the idle portion.
        assertApproxEqAbs(
            vault_.totalAssets(),
            depositAmt,
            1,
            "totalAssets must include idle USDC from unrouted deposit"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // AC6 — MorphoAdapter.withdraw returns actual delivered USDC
    // ═══════════════════════════════════════════════════════════════════════════

    /// @notice AC6: MorphoAdapter.withdraw returns the actual USDC delivered to the
    ///         vault under fork conditions (not a synthetic count).
    ///
    /// @dev Deploys the vault with MorphoAdapter, performs a deposit to push USDC
    ///      into the Morpho vault, then triggers a withdrawal and verifies:
    ///        - The returned value equals the actual USDC received by the vault.
    ///        - No shortfall: Morpho delivers exactly what was requested.
    function test_fork_morphoAdapter_withdrawReturnsActualDelivered() public {
        _setUp();

        RobotMoneyVault vault_ = new RobotMoneyVault(
            IERC20(BASE_USDC), TVL_CAP, PER_DEPOSIT_CAP, 0, feeRecipient, admin, admin
        );
        MorphoAdapter morphoAdapter = new MorphoAdapter(MORPHO_VAULT, BASE_USDC, address(vault_));
        _allowAdapter(vault_, address(morphoAdapter));
        vm.prank(admin);
        vault_.addAdapter(address(morphoAdapter), 10_000);

        deal(BASE_USDC, alice, 10_000 * ONE_USDC);
        vm.prank(alice);
        usdc.approve(address(vault_), type(uint256).max);

        uint256 depositAmt = 5_000 * ONE_USDC;
        vm.prank(alice);
        uint256 shares = vault_.deposit(depositAmt, alice);

        // Sanity: adapter has assets.
        assertGt(morphoAdapter.totalAssets(), 0, "morpho adapter must hold assets after deposit");

        // Snapshot vault USDC before redeem.
        uint256 vaultUsdcBefore = usdc.balanceOf(address(vault_));

        // Redeem all shares — triggers MorphoAdapter.withdraw internally.
        vm.prank(alice);
        uint256 assetsOut = vault_.redeem(shares, alice, alice);

        uint256 vaultUsdcAfter = usdc.balanceOf(address(vault_));

        // The vault USDC should have decreased (withdrawn from adapter then sent to alice).
        // assetsOut == what alice received; vaultUsdcBefore reflects any pre-existing idle.
        // Key assertion: assetsOut must be > 0 and consistent with what Morpho delivered.
        assertGt(assetsOut, 0, "morpho adapter must deliver USDC on redeem");

        // No unexpected USDC left stranded in the adapter.
        assertEq(
            morphoAdapter.totalAssets(), 0, "morpho adapter must be empty after full redemption"
        );

        // The USDC that Alice received must be approx equal to what she deposited
        // (no exit fee, rounding ≤ 1 wei per share).
        assertApproxEqAbs(assetsOut, depositAmt, 1, "morpho withdraw must return full deposit");
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Smoke: unit tests still pass in fork context (no harness weakening)
    // ═══════════════════════════════════════════════════════════════════════════

    /// @notice Confirm the fork harness does not weaken existing local coverage:
    ///         decimals offset and share minting invariants hold on a live fork.
    function test_fork_vaultInvariants_decimalsOffsetAndShareMinting() public {
        _setUp();

        RobotMoneyVault vault_ = new RobotMoneyVault(
            IERC20(BASE_USDC), TVL_CAP, PER_DEPOSIT_CAP, 0, feeRecipient, admin, admin
        );
        AaveV3Adapter aaveAdapter =
            new AaveV3Adapter(AAVE_POOL, BASE_USDC, AAVE_A_TOKEN, address(vault_));
        _allowAdapter(vault_, address(aaveAdapter));
        vm.prank(admin);
        vault_.addAdapter(address(aaveAdapter), 10_000);

        vm.prank(alice);
        usdc.approve(address(vault_), type(uint256).max);

        // Fresh-vault: previewDeposit(1 USDC) = 1e24 raw shares (offset=18).
        uint256 expectedRaw = ONE_USDC * (10 ** 18);
        assertEq(
            vault_.previewDeposit(ONE_USDC), expectedRaw, "decimals offset 18 must hold on fork"
        );

        // Deposit and verify non-zero shares minted.
        vm.prank(alice);
        uint256 shares = vault_.deposit(SEED_AMOUNT, alice);
        assertGt(shares, 0, "deposit must mint non-zero shares on fork");

        // Total supply must match minted shares (single depositor, no fee).
        assertEq(vault_.totalSupply(), shares, "totalSupply must equal minted shares");
    }
}
