// Canonical: docs/development/false-green-shapes.md

/**
 * Error tables for the third-party protocols the primary vault's strategy
 * adapters call into (issue #1380). Diagnostic decode surface only — nothing
 * here is ever used to build a call.
 *
 * WHY THESE ARE HERE
 * `RobotMoneyVault._routeDeposit` has no revert path of its own: every skip is
 * a `continue`, and anything it cannot place stays idle and emits
 * `UnroutedDeposit` (`contracts/RobotMoneyVault.sol:454-505`). The revert the
 * flake produces must therefore come from `_allocateTo` (:511-518), whose only
 * external move is `adpt.deploy(amount)` — and on this devnet that lands in
 * REAL Base protocol code: the geth genesis alloc ingests Aave V3, Compound V3
 * Comet and the Morpho Gauntlet USDC Prime MetaMorpho vault at a pinned Base
 * block (`testing/ethereum-testnet/config/fork-block.json`, addresses wired in
 * `contracts/script/Deploy.s.sol:399-401`).
 *
 * A custom error raised inside a nested protocol call bubbles up as a bare
 * 4-byte selector with no indication of which contract raised it. Decoding
 * only against this repository's own ABIs would therefore name the UNLIKELY
 * causes and print "unknown selector" for the likely one — a supply cap being
 * the obvious candidate, since these protocols accrue by timestamp and the
 * devnet's clock advances between an otherwise byte-identical passing and
 * failing run.
 *
 * PROVENANCE AND DRIFT
 * Transcribed mechanically from the canonical upstream sources, which are the
 * only place these names exist — the contracts are external deployments, so
 * `forge build` produces no artifact for them and
 * `.github/scripts/generate_abi_bindings.sh` cannot cover them:
 *
 *   Compound V3  compound-finance/comet   contracts/CometMainInterface.sol
 *   MetaMorpho   morpho-org/metamorpho    src/libraries/ErrorsLib.sol
 *   Aave V3      aave/aave-v3-core        contracts/protocol/libraries/helpers/Errors.sol
 *
 * These tables are best-effort and may lag upstream. That is safe by
 * construction: an unmatched selector is reported verbatim with its full
 * return data rather than swallowed, so a stale table degrades to exactly the
 * behaviour we would have had without it, never to a wrong name.
 */

/**
 * Compound V3 (Comet) custom errors. All zero-argument. `SupplyCapExceeded()`
 * here is distinct from MetaMorpho's `SupplyCapExceeded(bytes32)` below —
 * different signatures, different selectors, so a decoded name is unambiguous.
 */
export const cometErrorAbi = [
  { type: "error", name: "Absurd", inputs: [] },
  { type: "error", name: "AlreadyInitialized", inputs: [] },
  { type: "error", name: "BadAsset", inputs: [] },
  { type: "error", name: "BadDecimals", inputs: [] },
  { type: "error", name: "BadDiscount", inputs: [] },
  { type: "error", name: "BadMinimum", inputs: [] },
  { type: "error", name: "BadPrice", inputs: [] },
  { type: "error", name: "BorrowTooSmall", inputs: [] },
  { type: "error", name: "BorrowCFTooLarge", inputs: [] },
  { type: "error", name: "InsufficientReserves", inputs: [] },
  { type: "error", name: "LiquidateCFTooLarge", inputs: [] },
  { type: "error", name: "NoSelfTransfer", inputs: [] },
  { type: "error", name: "NotCollateralized", inputs: [] },
  { type: "error", name: "NotForSale", inputs: [] },
  { type: "error", name: "NotLiquidatable", inputs: [] },
  { type: "error", name: "Paused", inputs: [] },
  { type: "error", name: "ReentrantCallBlocked", inputs: [] },
  { type: "error", name: "SupplyCapExceeded", inputs: [] },
  { type: "error", name: "TimestampTooLarge", inputs: [] },
  { type: "error", name: "TooManyAssets", inputs: [] },
  { type: "error", name: "TooMuchSlippage", inputs: [] },
  { type: "error", name: "TransferInFailed", inputs: [] },
  { type: "error", name: "TransferOutFailed", inputs: [] },
  { type: "error", name: "Unauthorized", inputs: [] },
] as const;

/**
 * MetaMorpho (Morpho Gauntlet USDC Prime) custom errors. Morpho's `Id` type is
 * a `bytes32` market id, rendered as such here.
 *
 * `AllCapsReached()` is the one to watch: MetaMorpho's deposit walks its supply
 * queue and reverts with it when every market in the queue is at cap — a
 * cap-crossing that timestamp-driven interest accrual can produce between two
 * otherwise identical runs.
 */
export const metaMorphoErrorAbi = [
  { type: "error", name: "ZeroAddress", inputs: [] },
  { type: "error", name: "NotCuratorRole", inputs: [] },
  { type: "error", name: "NotAllocatorRole", inputs: [] },
  { type: "error", name: "NotGuardianRole", inputs: [] },
  { type: "error", name: "NotCuratorNorGuardianRole", inputs: [] },
  { type: "error", name: "UnauthorizedMarket", inputs: [{ name: "id", type: "bytes32" }] },
  { type: "error", name: "InconsistentAsset", inputs: [{ name: "id", type: "bytes32" }] },
  { type: "error", name: "SupplyCapExceeded", inputs: [{ name: "id", type: "bytes32" }] },
  { type: "error", name: "MaxFeeExceeded", inputs: [] },
  { type: "error", name: "AlreadySet", inputs: [] },
  { type: "error", name: "AlreadyPending", inputs: [] },
  { type: "error", name: "PendingCap", inputs: [{ name: "id", type: "bytes32" }] },
  { type: "error", name: "PendingRemoval", inputs: [] },
  { type: "error", name: "NonZeroCap", inputs: [] },
  { type: "error", name: "DuplicateMarket", inputs: [{ name: "id", type: "bytes32" }] },
  {
    type: "error",
    name: "InvalidMarketRemovalNonZeroCap",
    inputs: [{ name: "id", type: "bytes32" }],
  },
  {
    type: "error",
    name: "InvalidMarketRemovalNonZeroSupply",
    inputs: [{ name: "id", type: "bytes32" }],
  },
  {
    type: "error",
    name: "InvalidMarketRemovalTimelockNotElapsed",
    inputs: [{ name: "id", type: "bytes32" }],
  },
  { type: "error", name: "NoPendingValue", inputs: [] },
  { type: "error", name: "NotEnoughLiquidity", inputs: [] },
  { type: "error", name: "MarketNotCreated", inputs: [] },
  { type: "error", name: "MarketNotEnabled", inputs: [{ name: "id", type: "bytes32" }] },
  { type: "error", name: "AboveMaxTimelock", inputs: [] },
  { type: "error", name: "BelowMinTimelock", inputs: [] },
  { type: "error", name: "TimelockNotElapsed", inputs: [] },
  { type: "error", name: "MaxQueueLengthExceeded", inputs: [] },
  { type: "error", name: "ZeroFeeRecipient", inputs: [] },
  { type: "error", name: "InconsistentReallocation", inputs: [] },
  { type: "error", name: "AllCapsReached", inputs: [] },
] as const;

/**
 * Aave V3 does NOT use custom errors: it reverts with `require(cond, Errors.X)`
 * where each constant is a stringified number. viem decodes the envelope as
 * `Error(string)` on its own, which yields an opaque `"51"`; this table turns
 * that into `SUPPLY_CAP_EXCEEDED`.
 */
export const aaveV3ErrorCodes: Record<string, string> = {
  "1": "CALLER_NOT_POOL_ADMIN",
  "2": "CALLER_NOT_EMERGENCY_ADMIN",
  "3": "CALLER_NOT_POOL_OR_EMERGENCY_ADMIN",
  "4": "CALLER_NOT_RISK_OR_POOL_ADMIN",
  "5": "CALLER_NOT_ASSET_LISTING_OR_POOL_ADMIN",
  "6": "CALLER_NOT_BRIDGE",
  "7": "ADDRESSES_PROVIDER_NOT_REGISTERED",
  "8": "INVALID_ADDRESSES_PROVIDER_ID",
  "9": "NOT_CONTRACT",
  "10": "CALLER_NOT_POOL_CONFIGURATOR",
  "11": "CALLER_NOT_ATOKEN",
  "12": "INVALID_ADDRESSES_PROVIDER",
  "13": "INVALID_FLASHLOAN_EXECUTOR_RETURN",
  "14": "RESERVE_ALREADY_ADDED",
  "15": "NO_MORE_RESERVES_ALLOWED",
  "16": "EMODE_CATEGORY_RESERVED",
  "17": "INVALID_EMODE_CATEGORY_ASSIGNMENT",
  "18": "RESERVE_LIQUIDITY_NOT_ZERO",
  "19": "FLASHLOAN_PREMIUM_INVALID",
  "20": "INVALID_RESERVE_PARAMS",
  "21": "INVALID_EMODE_CATEGORY_PARAMS",
  "22": "BRIDGE_PROTOCOL_FEE_INVALID",
  "23": "CALLER_MUST_BE_POOL",
  "24": "INVALID_MINT_AMOUNT",
  "25": "INVALID_BURN_AMOUNT",
  "26": "INVALID_AMOUNT",
  "27": "RESERVE_INACTIVE",
  "28": "RESERVE_FROZEN",
  "29": "RESERVE_PAUSED",
  "30": "BORROWING_NOT_ENABLED",
  "31": "STABLE_BORROWING_NOT_ENABLED",
  "32": "NOT_ENOUGH_AVAILABLE_USER_BALANCE",
  "33": "INVALID_INTEREST_RATE_MODE_SELECTED",
  "34": "COLLATERAL_BALANCE_IS_ZERO",
  "35": "HEALTH_FACTOR_LOWER_THAN_LIQUIDATION_THRESHOLD",
  "36": "COLLATERAL_CANNOT_COVER_NEW_BORROW",
  "37": "COLLATERAL_SAME_AS_BORROWING_CURRENCY",
  "38": "AMOUNT_BIGGER_THAN_MAX_LOAN_SIZE_STABLE",
  "39": "NO_DEBT_OF_SELECTED_TYPE",
  "40": "NO_EXPLICIT_AMOUNT_TO_REPAY_ON_BEHALF",
  "41": "NO_OUTSTANDING_STABLE_DEBT",
  "42": "NO_OUTSTANDING_VARIABLE_DEBT",
  "43": "UNDERLYING_BALANCE_ZERO",
  "44": "INTEREST_RATE_REBALANCE_CONDITIONS_NOT_MET",
  "45": "HEALTH_FACTOR_NOT_BELOW_THRESHOLD",
  "46": "COLLATERAL_CANNOT_BE_LIQUIDATED",
  "47": "SPECIFIED_CURRENCY_NOT_BORROWED_BY_USER",
  "49": "INCONSISTENT_FLASHLOAN_PARAMS",
  "50": "BORROW_CAP_EXCEEDED",
  "51": "SUPPLY_CAP_EXCEEDED",
  "52": "UNBACKED_MINT_CAP_EXCEEDED",
  "53": "DEBT_CEILING_EXCEEDED",
  "54": "UNDERLYING_CLAIMABLE_RIGHTS_NOT_ZERO",
  "55": "STABLE_DEBT_NOT_ZERO",
  "56": "VARIABLE_DEBT_SUPPLY_NOT_ZERO",
  "57": "LTV_VALIDATION_FAILED",
  "58": "INCONSISTENT_EMODE_CATEGORY",
  "59": "PRICE_ORACLE_SENTINEL_CHECK_FAILED",
  "60": "ASSET_NOT_BORROWABLE_IN_ISOLATION",
  "61": "RESERVE_ALREADY_INITIALIZED",
  "62": "USER_IN_ISOLATION_MODE_OR_LTV_ZERO",
  "63": "INVALID_LTV",
  "64": "INVALID_LIQ_THRESHOLD",
  "65": "INVALID_LIQ_BONUS",
  "66": "INVALID_DECIMALS",
  "67": "INVALID_RESERVE_FACTOR",
  "68": "INVALID_BORROW_CAP",
  "69": "INVALID_SUPPLY_CAP",
  "70": "INVALID_LIQUIDATION_PROTOCOL_FEE",
  "71": "INVALID_EMODE_CATEGORY",
  "72": "INVALID_UNBACKED_MINT_CAP",
  "73": "INVALID_DEBT_CEILING",
  "74": "INVALID_RESERVE_INDEX",
  "75": "ACL_ADMIN_CANNOT_BE_ZERO",
  "76": "INCONSISTENT_PARAMS_LENGTH",
  "77": "ZERO_ADDRESS_NOT_VALID",
  "78": "INVALID_EXPIRATION",
  "79": "INVALID_SIGNATURE",
  "80": "OPERATION_NOT_SUPPORTED",
  "81": "DEBT_CEILING_NOT_ZERO",
  "82": "ASSET_NOT_LISTED",
  "83": "INVALID_OPTIMAL_USAGE_RATIO",
  "84": "INVALID_OPTIMAL_STABLE_TO_TOTAL_DEBT_RATIO",
  "85": "UNDERLYING_CANNOT_BE_RESCUED",
  "86": "ADDRESSES_PROVIDER_ALREADY_ADDED",
  "87": "POOL_ADDRESSES_DO_NOT_MATCH",
  "88": "STABLE_BORROWING_ENABLED",
  "89": "SILOED_BORROWING_VIOLATION",
  "90": "RESERVE_DEBT_NOT_ZERO",
  "91": "FLASHLOAN_DISABLED",
};
