// Canonical: none — hand-maintained ABI surface; mirrors contracts/

/**
 * Minimal ABI excerpt for the depositor-owned agent-policy actions
 * (issue #269 — each depositor is the sole authority over her own
 * agent). The `authorizeAgent` / `setPolicy` / `revokeAgent` surface is
 * permissionless on the contract; gating is by recorded `agentOwner`,
 * not by `ADMIN_ROLE`. `ADMIN_ROLE` survives only as a protocol-wide
 * kill-switch counterweight to `pause` (`unpause`) held by the
 * contract-upgrader.
 *
 * Tracks the canonical interface in
 * `contracts/gateway/interfaces/IGateway.sol`. Only the functions the
 * dapp needs to encode/decode appear here; the full ABI lives with the
 * Rust client (`clients/rust-payment-client/abi/`).
 */
export const gatewayAbi = [
  {
    type: "function",
    name: "authorizeAgent",
    stateMutability: "nonpayable",
    inputs: [
      { name: "agent", type: "address" },
      {
        name: "p",
        type: "tuple",
        components: [
          { name: "active", type: "bool" },
          { name: "validUntil", type: "uint64" },
          { name: "maxPerPayment", type: "uint256" },
          { name: "maxPerWindow", type: "uint256" },
          { name: "shareReceiver", type: "address" },
          { name: "allowedDestinations", type: "address[]" },
          { name: "assetRecipient", type: "address" },
          { name: "maxWithdrawPerPayment", type: "uint256" },
          { name: "maxWithdrawPerWindow", type: "uint256" },
          { name: "allowedSourceVaults", type: "address[]" },
        ],
      },
    ],
    outputs: [],
  },
  {
    type: "function",
    name: "setPolicy",
    stateMutability: "nonpayable",
    inputs: [
      { name: "agent", type: "address" },
      {
        name: "p",
        type: "tuple",
        components: [
          { name: "active", type: "bool" },
          { name: "validUntil", type: "uint64" },
          { name: "maxPerPayment", type: "uint256" },
          { name: "maxPerWindow", type: "uint256" },
          { name: "shareReceiver", type: "address" },
          { name: "allowedDestinations", type: "address[]" },
          { name: "assetRecipient", type: "address" },
          { name: "maxWithdrawPerPayment", type: "uint256" },
          { name: "maxWithdrawPerWindow", type: "uint256" },
          { name: "allowedSourceVaults", type: "address[]" },
        ],
      },
    ],
    outputs: [],
  },
  {
    type: "function",
    name: "revokeAgent",
    stateMutability: "nonpayable",
    inputs: [{ name: "agent", type: "address" }],
    outputs: [],
  },
  {
    type: "function",
    name: "agentOwner",
    stateMutability: "view",
    inputs: [{ name: "agent", type: "address" }],
    outputs: [{ name: "", type: "address" }],
  },
  {
    type: "function",
    name: "pause",
    stateMutability: "nonpayable",
    inputs: [],
    outputs: [],
  },
  {
    type: "function",
    name: "unpause",
    stateMutability: "nonpayable",
    inputs: [],
    outputs: [],
  },
  {
    type: "function",
    name: "paused",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "bool" }],
  },
  {
    type: "function",
    name: "usdc",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "address" }],
  },
  {
    type: "function",
    name: "hasRole",
    stateMutability: "view",
    inputs: [
      { name: "role", type: "bytes32" },
      { name: "account", type: "address" },
    ],
    outputs: [{ name: "", type: "bool" }],
  },
  // OpenZeppelin AccessControl — used to grant/revoke ADMIN_ROLE and
  // PAUSER_ROLE. Caller must hold the role's admin role (DEFAULT_ADMIN_ROLE
  // for ADMIN/PAUSER per `contracts/gateway/AccessRoles.sol`).
  {
    type: "function",
    name: "grantRole",
    stateMutability: "nonpayable",
    inputs: [
      { name: "role", type: "bytes32" },
      { name: "account", type: "address" },
    ],
    outputs: [],
  },
  {
    type: "function",
    name: "revokeRole",
    stateMutability: "nonpayable",
    inputs: [
      { name: "role", type: "bytes32" },
      { name: "account", type: "address" },
    ],
    outputs: [],
  },
  // Permissionless two-step commit/reveal authorization (AZ-DAPP-1).
  // commitAuthorization takes keccak256(abi.encode(agent, caller, salt));
  // revealAuthorization re-derives the hash and authorizes the agent.
  {
    type: "function",
    name: "commitAuthorization",
    stateMutability: "nonpayable",
    inputs: [{ name: "commitHash", type: "bytes32" }],
    outputs: [],
  },
  {
    type: "function",
    name: "revealAuthorization",
    stateMutability: "nonpayable",
    inputs: [
      { name: "agent", type: "address" },
      { name: "salt", type: "bytes32" },
      {
        name: "p",
        type: "tuple",
        components: [
          { name: "active", type: "bool" },
          { name: "validUntil", type: "uint64" },
          { name: "maxPerPayment", type: "uint256" },
          { name: "maxPerWindow", type: "uint256" },
          { name: "shareReceiver", type: "address" },
          { name: "allowedDestinations", type: "address[]" },
          { name: "assetRecipient", type: "address" },
          { name: "maxWithdrawPerPayment", type: "uint256" },
          { name: "maxWithdrawPerWindow", type: "uint256" },
          { name: "allowedSourceVaults", type: "address[]" },
        ],
      },
    ],
    outputs: [],
  },
] as const;

export type AdminActionName =
  | "authorizeAgent"
  | "setPolicy"
  | "revokeAgent"
  | "pause"
  | "unpause"
  | "grantRole"
  | "revokeRole";

/**
 * Role identifiers handled by the dapp's role grant/revoke flow.
 * AGENT_ROLE has its own dedicated depositor-owned authorize/revoke
 * surface (because it carries a policy struct and is gated on
 * `msg.sender == agentOwner[agent]`, not on any privileged role).
 * The role-bytes32 path here is for ADMIN_ROLE and PAUSER_ROLE only —
 * AGENT_ROLE is intentionally excluded so it can never be granted
 * without setting a policy through the depositor-owned surface.
 */
export type RoleName = "ADMIN_ROLE" | "PAUSER_ROLE";

/**
 * keccak256 of the role string, matching `contracts/gateway/AccessRoles.sol`:
 *   bytes32 public constant ADMIN_ROLE  = keccak256("ADMIN_ROLE");
 *   bytes32 public constant PAUSER_ROLE = keccak256("PAUSER_ROLE");
 * Hard-coded to keep the dapp pure and avoid an extra RPC round trip.
 */
export const ROLE_HASH: Record<RoleName, `0x${string}`> = {
  ADMIN_ROLE: "0xa49807205ce4d355092ef5a8a18f56e8913cf4a201fbe287825b095693c21775",
  PAUSER_ROLE: "0x65d7a28e3265b37a6474929f336521b332c1681b933f6cb9f3376673440d862a",
};

export const ADMIN_ROLE_HASH = ROLE_HASH.ADMIN_ROLE;
export const PAUSER_ROLE_HASH = ROLE_HASH.PAUSER_ROLE;

/**
 * Minimal ERC-20 ABI fragment used by the testnet/devnet faucet (issue
 * #261) for `transfer(address,uint256)` writes and `balanceOf(address)`
 * read-backs. The faucet drips canonical USDC by signing a plain ERC-20
 * `transfer` from the harness holder EOA — the same path the smoke-test
 * fixture's `Fixture::fund_usdc` uses on the Rust side (issue #255).
 *
 * Kept minimal on purpose: the dapp only needs `transfer` (write) and
 * `balanceOf` (read-back). All other ERC-20 surface lives with the Rust
 * client.
 */
export const erc20Abi = [
  {
    type: "function",
    name: "transfer",
    stateMutability: "nonpayable",
    inputs: [
      { name: "to", type: "address" },
      { name: "amount", type: "uint256" },
    ],
    outputs: [{ name: "", type: "bool" }],
  },
  {
    type: "function",
    name: "balanceOf",
    stateMutability: "view",
    inputs: [{ name: "owner", type: "address" }],
    outputs: [{ name: "", type: "uint256" }],
  },
  // `approve` + `allowance` are required by the Deposit/Withdraw tab
  // (issue #257). USDC is a standard ERC-20 on Base; users approve the
  // vault to pull `assets` before calling `deposit(assets, receiver)`.
  {
    type: "function",
    name: "approve",
    stateMutability: "nonpayable",
    inputs: [
      { name: "spender", type: "address" },
      { name: "amount", type: "uint256" },
    ],
    outputs: [{ name: "", type: "bool" }],
  },
  {
    type: "function",
    name: "allowance",
    stateMutability: "view",
    inputs: [
      { name: "owner", type: "address" },
      { name: "spender", type: "address" },
    ],
    outputs: [{ name: "", type: "uint256" }],
  },
  // `decimals` + `symbol` are needed by the main-page balances panel (issue
  // #463) to render USDC/RM and per-vault receipt-token balances using each
  // token's own decimal places and ticker label.
  {
    type: "function",
    name: "decimals",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "uint8" }],
  },
  {
    type: "function",
    name: "symbol",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "string" }],
  },
] as const;

/**
 * Minimal ERC-4626 ABI excerpt for the Deposit/Withdraw tab (issue #257).
 * RobotMoneyVault is a standard ERC-4626 vault; the dapp drives
 * `deposit(assets,receiver)` and `redeem(shares,receiver,owner)`
 * directly against the vault contract (the gateway's `deposit` entry
 * point is AGENT_ROLE-only and not exposed to depositors). Only the
 * functions the dapp needs to encode/decode and read back appear here;
 * the full ABI lives with the Foundry contracts and the Rust fork-e2e
 * crate.
 */
export const vaultAbi = [
  {
    type: "function",
    name: "deposit",
    stateMutability: "nonpayable",
    inputs: [
      { name: "assets", type: "uint256" },
      { name: "receiver", type: "address" },
    ],
    outputs: [{ name: "shares", type: "uint256" }],
  },
  {
    type: "function",
    name: "redeem",
    stateMutability: "nonpayable",
    inputs: [
      { name: "shares", type: "uint256" },
      { name: "receiver", type: "address" },
      { name: "owner", type: "address" },
    ],
    outputs: [{ name: "assets", type: "uint256" }],
  },
  {
    type: "function",
    name: "balanceOf",
    stateMutability: "view",
    inputs: [{ name: "account", type: "address" }],
    outputs: [{ name: "", type: "uint256" }],
  },
  {
    type: "function",
    name: "previewDeposit",
    stateMutability: "view",
    inputs: [{ name: "assets", type: "uint256" }],
    outputs: [{ name: "shares", type: "uint256" }],
  },
  {
    type: "function",
    name: "previewRedeem",
    stateMutability: "view",
    inputs: [{ name: "shares", type: "uint256" }],
    outputs: [{ name: "assets", type: "uint256" }],
  },
  {
    type: "function",
    name: "maxRedeem",
    stateMutability: "view",
    inputs: [{ name: "owner", type: "address" }],
    outputs: [{ name: "", type: "uint256" }],
  },
  {
    type: "function",
    name: "exitFeeBps",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "uint256" }],
  },
] as const;

export type VaultActionName = "deposit" | "redeem";

/**
 * Minimal VaultRegistry ABI excerpt for VaultRegistryContext and the
 * DestinationSelector (issue #320, extended issue #417).
 * Exposes:
 *   - `listVaults()` — enumerate all registered vault addresses.
 *   - `getVault(address)` — the vault's stored `VaultMetadata` (name, asset,
 *     registeredAt) plus its lifecycle `status`, exactly as
 *     `contracts/VaultRegistry.sol`'s `getVault` returns them (two separate
 *     outputs, NOT one merged tuple — see issue #1348).
 *
 *     `riskLabel`, `mandate`, `receiptToken`, `depositCap`, and `exitFeeBps`
 *     were an earlier, aspirational read shape from
 *     `docs/technical/vault-registry-decisions.md` §3.4 that the shipped
 *     contract (#329) never implemented — they do not exist on
 *     `VaultRegistry.sol` today. See `VaultRecord` below for where the real
 *     equivalents (if any) live instead.
 *
 * VaultRecord.status encodes as uint8: 0=Active, 1=Paused, 2=Retired.
 * The `VaultSelectorDepositTab` calls `getVault` live before submit to
 * guard against cached paused-vault state (issue #417 AC §4). Because
 * `getVault` returns two top-level outputs, viem's `decodeFunctionResult`
 * decodes it as a 2-element array `[metadata, status]` — NOT a
 * `{ metadata, status }` object — so callers must index positionally.
 */
export const registryAbi = [
  {
    type: "function",
    name: "listVaults",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "addresses", type: "address[]" }],
  },
  {
    type: "function",
    name: "getVault",
    stateMutability: "view",
    inputs: [{ name: "vault", type: "address" }],
    outputs: [
      {
        name: "metadata",
        type: "tuple",
        components: [
          { name: "name", type: "string" },
          { name: "asset", type: "address" },
          { name: "registeredAt", type: "uint256" },
        ],
      },
      { name: "status", type: "uint8" },
    ],
  },
] as const;

/** VaultStatus enum — matches VaultRegistry.sol's enum order. */
export const VaultStatus = { Active: 0, Paused: 1, Retired: 2 } as const;
export type VaultStatusValue = (typeof VaultStatus)[keyof typeof VaultStatus];

/**
 * Decoded on-chain vault record, assembled from `registry.getVault(address)`
 * (the two-output `metadata`/`status` shape above) plus the `vault` address
 * that was passed into the call (the return data itself doesn't repeat it).
 *
 * Trimmed to exactly what the registry returns (issue #1348) — the
 * previous, larger shape included fields the contract never returns:
 *   - `riskLabel` / `mandate`: no on-chain source exists anywhere in the
 *     contracts today; dropped. (A UI risk/mandate taxonomy, if wanted, is
 *     product scope for a future issue, not a mechanical field restore.)
 *   - `receiptToken`: redundant with `vault` — every vault contract
 *     (`RobotMoneyVault`, `BasketVault`, `RwaVault`, ...) is itself the
 *     ERC-4626 share token, so `receiptToken` always equals `vault`.
 *     Consumers that read the receipt token now use `.vault` directly.
 *   - `depositCap` / `exitFeeBps`: real per-vault getters (`tvlCap`/
 *     `perDepositCap`/`exitFeeBps`) exist on the vault contracts
 *     themselves, not the registry. `exitFeeBps` already has an
 *     established live-read pattern (`MultiVaultWithdrawalTab` reads it
 *     directly off the vault via `vaultAbi` right before a redemption
 *     preview, per issue #417 AC §11: preview values must be live, never
 *     cached). Caching either in `VaultRecord` would reintroduce exactly
 *     the staleness risk that pattern exists to avoid, so they're left out;
 *     a component that needs them should read them live off the vault
 *     contract the same way.
 */
export interface VaultRecord {
  vault: `0x${string}`;
  name: string;
  asset: `0x${string}`;
  registeredAt: bigint;
  status: VaultStatusValue;
}

/**
 * Minimal PortfolioRouter ABI excerpt for the DestinationSelector and
 * router deposit flow (issue #320, extended issue #417). Exposes:
 *   - `previewDeposit(amount)` — per-vault breakdown without state change.
 *   - `deposit(amount, minSharesPerLeg)` — all-or-revert multi-vault deposit.
 *   - `getEffectiveWeights()` — the vault list the router currently routes
 *     by (`(address[] vaults, uint256[] bps)`); used by RouterDepositTab to
 *     detect vault-list changes between preview and submit (AC §7). Replaces
 *     the never-implemented `activeVaults()` (issue #1281) — no Solidity
 *     source ever defined that selector; `getEffectiveWeights()` is the real
 *     read `previewDeposit` itself routes by.
 *
 * The LegPreview tuple matches `PortfolioRouter.sol`'s `LegPreview` struct:
 * { vault, weightBps, legAmount, estShares, unavailable }.
 */
export const routerAbi = [
  {
    type: "function",
    name: "previewDeposit",
    stateMutability: "view",
    inputs: [{ name: "amount", type: "uint256" }],
    outputs: [
      {
        name: "legs",
        type: "tuple[]",
        components: [
          { name: "vault", type: "address" },
          { name: "weightBps", type: "uint256" },
          { name: "legAmount", type: "uint256" },
          { name: "estShares", type: "uint256" },
          { name: "unavailable", type: "bool" },
        ],
      },
    ],
  },
  {
    type: "function",
    name: "deposit",
    stateMutability: "nonpayable",
    inputs: [
      { name: "amount", type: "uint256" },
      { name: "minSharesPerLeg", type: "uint256[]" },
    ],
    outputs: [{ name: "sharesPerLeg", type: "uint256[]" }],
  },
  {
    type: "function",
    name: "getEffectiveWeights",
    stateMutability: "view",
    inputs: [],
    outputs: [
      { name: "vaults", type: "address[]" },
      { name: "bps", type: "uint256[]" },
    ],
  },
  // Router-eligibility view (issue #426): true if the vault's ERC-4626
  // `asset()` equals the router's USDC. Distinct from VaultRegistry status —
  // registry status describes lifecycle (Active/Paused/Retired) while router
  // eligibility describes asset compatibility with the router's deposit flow.
  // Clients should treat a vault as depositable only if both signals agree.
  {
    type: "function",
    name: "isRouterEligible",
    stateMutability: "view",
    inputs: [{ name: "vault", type: "address" }],
    outputs: [{ name: "eligible", type: "bool" }],
  },
] as const;

export type RouterActionName = "deposit";

/**
 * Minimal ABI fragment for AgentTokenVault.shortlist() and
 * ProtocolAssetVault.shortlist() — called by VaultDetail's Composition
 * section to enumerate the basket composition without a connected
 * wallet (wagmi useReadContract works without a signer).
 *
 * BOTH basket vaults declare it. Until issue #1364 only AgentTokenVault did,
 * so this fragment named a selector ProtocolAssetVault did not have and every
 * ProtocolAssetVault composition panel rendered "unavailable".
 * `tests/unit/abi-parity.test.ts` now checks this fragment against the
 * canonical Foundry artifact for EACH basket vault, so the next basket vault
 * that ships without `shortlist()` reds CI instead of degrading quietly.
 *
 * The on-chain `shortlist()` (contracts/vaults/AgentTokenVault.sol and
 * contracts/vaults/ProtocolAssetVault.sol, both via BasketViews.sol) returns
 * FIVE PARALLEL ARRAYS, not an array of structs:
 *
 *   (address[] tokens, address[] pools, uint24[] fees,
 *    bool[] active, uint256[] balances)
 *
 * Declaring this as a `tuple[]` (struct array) was a hard ABI decode break
 * (scan finding DAPP-5): viem either threw or decoded garbage, so the
 * composition view broke for every basket vault. The fragment below matches
 * the real return signature; callers zip the arrays by index into
 * ShortlistEntry rows (see decodeShortlist / VaultDetail).
 *
 * Canonical: docs/prd.md §11.2, §11.3 — basket vault composition.
 */
export const BASKET_VAULT_SHORTLIST_ABI = [
  {
    type: "function",
    name: "shortlist",
    stateMutability: "view",
    inputs: [],
    outputs: [
      { name: "tokens", type: "address[]" },
      { name: "pools", type: "address[]" },
      { name: "fees", type: "uint24[]" },
      { name: "active", type: "bool[]" },
      { name: "balances", type: "uint256[]" },
    ],
  },
] as const;

/** A decoded shortlist entry from AgentTokenVault / ProtocolAssetVault. */
export interface ShortlistEntry {
  token: `0x${string}`;
  active: boolean;
  vaultBalance: bigint;
}

/**
 * Raw tuple returned by the five-parallel-array `shortlist()` ABI above.
 * viem decodes the multi-return as a positional tuple of arrays.
 */
export type ShortlistResult = readonly [
  readonly `0x${string}`[], // tokens
  readonly `0x${string}`[], // pools
  readonly number[], // fees (uint24)
  readonly boolean[], // active
  readonly bigint[], // balances
];

/**
 * Zip the five parallel arrays returned by `shortlist()` into ShortlistEntry
 * rows keyed by index. The token array drives the length; the other arrays
 * are read positionally and default safely when a parallel array is shorter
 * (defensive — on-chain they are always equal length).
 */
export function decodeShortlist(result: unknown): ShortlistEntry[] {
  if (!Array.isArray(result)) return [];
  const tuple = result as readonly unknown[];
  const tokens = tuple[0];
  const active = tuple[3];
  const balances = tuple[4];
  if (!Array.isArray(tokens)) return [];
  return tokens.map((token, i) => ({
    token: token as `0x${string}`,
    active: Array.isArray(active) ? Boolean(active[i]) : false,
    vaultBalance:
      Array.isArray(balances) && typeof balances[i] === "bigint" ? (balances[i] as bigint) : 0n,
  }));
}
