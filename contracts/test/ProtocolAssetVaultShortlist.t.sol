// SPDX-License-Identifier: MIT
// Canonical: docs/prd.md §11.2, §11.3 — basket vault composition;
//            docs/architecture.md §4.1 — Vault Family
// Covers issue #1364 — ProtocolAssetVault is rendered as a basket vault by the
//                      dapp (risk label VOLATILE) but declared no shortlist(),
//                      so the composition panel was permanently "unavailable".
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

import {ProtocolAssetVault} from "../vaults/ProtocolAssetVault.sol";
import {BasketVault} from "../vaults/BasketVault.sol";
import {ISwapRouter} from "../interfaces/ISwapRouter.sol";
import {TestERC20} from "./helpers/TestERC20.sol";

/// @dev Uniswap V3 pool mock: token0/token1 reads for addAsset validation plus a
///      flat 1:1 TWAP via observe() (arithmetic-mean tick = 0). Mirrors the mock
///      in AgentTokenVault.t.sol; kept local so the two suites stay independent.
contract ShortlistMockPool {
    address public immutable token0;
    address public immutable token1;
    uint16 public cardinality = 100;
    uint128 public poolLiquidity = 1e18;
    uint24 public feeTier;

    constructor(address token0_, address token1_, uint24 fee_) {
        token0 = token0_;
        token1 = token1_;
        feeTier = fee_;
    }

    function fee() external view returns (uint24) {
        return feeTier;
    }

    function liquidity() external view returns (uint128) {
        return poolLiquidity;
    }

    function slot0() external view returns (uint160, int24, uint16, uint16, uint16, uint8, bool) {
        return (uint160(1 << 96), 0, 0, cardinality, cardinality, 0, true);
    }

    function observe(uint32[] calldata secondsAgos)
        external
        pure
        returns (int56[] memory tickCumulatives, uint160[] memory secondsPerLiq)
    {
        tickCumulatives = new int56[](secondsAgos.length);
        secondsPerLiq = new uint160[](secondsAgos.length);
    }

    function observations(uint256) external view returns (uint32, int56, uint160, bool) {
        return (uint32(block.timestamp), 0, 0, true);
    }
}

/// @dev ProtocolAssetVault's constructor only stores the router address.
contract ShortlistStubSwapRouter is ISwapRouter {
    function exactInputSingle(ExactInputSingleParams calldata) external pure returns (uint256) {
        return 0;
    }
}

/// @notice Issue #1364 — a deployed `ProtocolAssetVault` must answer `shortlist()`
///         with its configured assets, in registry order, with live balances.
///
///         The dapp's composition panel (`BasketShortlistPanel`) calls
///         `shortlist()` on any vault whose risk label is VOLATILE.
///         `ProtocolAssetVault`'s risk label IS VOLATILE
///         (contracts/vaults/ProtocolAssetVault.sol NatSpec), so before #1364 the
///         call reverted with no matching function and the panel fell into its
///         `vault-detail-composition-error` branch.
contract ProtocolAssetVaultShortlistTest is Test {
    uint256 internal constant ONE_USDC = 1e6;
    uint24 internal constant POOL_FEE = 3000;

    TestERC20 internal usdc;
    ShortlistStubSwapRouter internal router;
    ProtocolAssetVault internal vault;
    TestERC20[3] internal tokens;

    address internal admin = makeAddr("admin");

    function setUp() public {
        usdc = new TestERC20();
        router = new ShortlistStubSwapRouter();
        vault = new ProtocolAssetVault(
            IERC20(address(usdc)),
            ISwapRouter(address(router)),
            10_000_000 * ONE_USDC,
            1_000_000 * ONE_USDC,
            0,
            admin,
            admin,
            admin
        );
        for (uint256 i = 0; i < tokens.length; i++) {
            tokens[i] = new TestERC20();
            ShortlistMockPool pool =
                new ShortlistMockPool(address(tokens[i]), address(usdc), POOL_FEE);
            vm.prank(admin);
            vault.addAsset(
                address(tokens[i]), address(pool), POOL_FEE, address(0), BasketVault.Venue.V3
            );
        }
    }

    /// @notice The whole point of #1364: the selector exists and the call does not
    ///         revert. A `ProtocolAssetVault` with no `shortlist()` reverts here,
    ///         which is exactly what made the dapp panel render "unavailable".
    function test_shortlist_isCallableOnProtocolAssetVault() public view {
        (address[] memory t,,,,) = vault.shortlist();
        assertEq(t.length, tokens.length, "shortlist() returns one row per configured asset");
    }

    /// @notice All five parallel arrays are returned, equal-length and in registry
    ///         order, with the configured token/pool/fee and the active flag set.
    ///         The five-parallel-array shape (NOT a struct array) is the shape the
    ///         dapp's BASKET_VAULT_SHORTLIST_ABI declares — scan finding DAPP-5.
    function test_shortlist_returnsConfiguredAssetsInOrder() public view {
        (
            address[] memory t,
            address[] memory pools,
            uint24[] memory fees,
            bool[] memory active,
            uint256[] memory balances
        ) = vault.shortlist();

        assertEq(t.length, tokens.length, "tokens length");
        assertEq(pools.length, tokens.length, "pools length matches tokens");
        assertEq(fees.length, tokens.length, "fees length matches tokens");
        assertEq(active.length, tokens.length, "active length matches tokens");
        assertEq(balances.length, tokens.length, "balances length matches tokens");

        for (uint256 i = 0; i < tokens.length; i++) {
            (address regToken, address regPool, uint24 regFee, bool regActive,,) = vault.assets(i);
            assertEq(t[i], address(tokens[i]), "token matches configured asset, in order");
            assertEq(t[i], regToken, "token matches the asset registry entry");
            assertEq(pools[i], regPool, "pool matches the asset registry entry");
            assertEq(fees[i], regFee, "swap fee matches the asset registry entry");
            assertTrue(regActive, "asset is active after addAsset");
            assertTrue(active[i], "active flag mirrors the asset registry entry");
            assertEq(balances[i], 0, "no balance before any asset is transferred in");
        }
    }

    /// @notice `balances` reads the vault's live ERC-20 balance per asset, so the
    ///         dapp renders real numbers rather than zeros.
    function test_shortlist_reportsLiveVaultBalances() public {
        tokens[1].mint(address(vault), 42e18);

        (address[] memory t,,,, uint256[] memory balances) = vault.shortlist();
        assertEq(t[1], address(tokens[1]), "second row is the funded asset");
        assertEq(balances[1], 42e18, "balance reflects the vault's live token balance");
        assertEq(balances[0], 0, "unfunded asset still reports zero");
        assertEq(balances[2], 0, "unfunded asset still reports zero");
    }

    /// @notice A removed asset stays in the shortlist with `active == false`, which
    ///         is what the dapp renders as an "inactive" row.
    function test_shortlist_marksRemovedAssetInactive() public {
        vm.prank(admin);
        vault.removeAsset(0);

        (address[] memory t,,, bool[] memory active,) = vault.shortlist();
        assertEq(t.length, tokens.length, "removed asset is retained as a row");
        assertFalse(active[0], "removed asset is reported inactive");
        assertTrue(active[1], "remaining assets stay active");
        assertTrue(active[2], "remaining assets stay active");
    }

    /// @notice An empty basket returns five empty arrays, NOT a revert. The dapp
    ///         distinguishes this ("No basket assets.") from the failure branch
    ///         ("unavailable"), so the empty case must succeed.
    function test_shortlist_emptyBasketReturnsEmptyArrays() public {
        ProtocolAssetVault empty = new ProtocolAssetVault(
            IERC20(address(usdc)),
            ISwapRouter(address(router)),
            10_000_000 * ONE_USDC,
            1_000_000 * ONE_USDC,
            0,
            admin,
            admin,
            admin
        );

        (
            address[] memory t,
            address[] memory pools,
            uint24[] memory fees,
            bool[] memory active,
            uint256[] memory balances
        ) = empty.shortlist();
        assertEq(t.length, 0, "empty basket returns zero rows");
        assertEq(pools.length, 0, "empty basket returns zero pools");
        assertEq(fees.length, 0, "empty basket returns zero fees");
        assertEq(active.length, 0, "empty basket returns zero active flags");
        assertEq(balances.length, 0, "empty basket returns zero balances");
    }
}
