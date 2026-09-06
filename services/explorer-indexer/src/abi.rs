//! Canonical: docs/architecture.md §5.4 — Explorer Indexer and API
//! ABI surfaces the indexer decodes — IGateway, RobotMoneyVault, and
//! VaultRegistry events plus the ERC-4626 / vault state reads.  Mirrors
//! the contract sources in `contracts/gateway/interfaces/IGateway.sol`,
//! `contracts/RobotMoneyVault.sol`, `contracts/VaultRegistry.sol`, and the canonical event signatures
//! in `docs/technical/vault-registry-decisions.md` §3.5.
//!
//! # ABI Drift Map (dev-scout #383 findings — 2026-05-15) — HISTORICAL RECORD
//!
//! The following divergences were identified between the `sol!` declarations
//! in this file and the compiled Solidity sources.  Each item maps to the
//! downstream fix issue that will correct it.  No runtime behavior is changed
//! by this scout pass — only documentation is added.
//!
//! **Read this section as a dated record, not as current state (issue #1346).**
//! Every `Foo.sol:NN` reference *inside this section* is the line number as it
//! stood on 2026-05-15; the sources have moved since and these numbers are
//! deliberately left frozen so the record still reads as the snapshot it was.
//! The `AgentAuthorized` / `AgentRevoked` / `VaultRegistered` /
//! `VaultStatusChanged` / `ProposalCreated` / `VoteCast` / `ProposalExecuted` /
//! `WeightsApplied` drifts below were all corrected by issue #366 (closed) and
//! no longer exist.  The **live, maintained** citations are the ones on the
//! `sol!` declarations, the `Topics` fields, and the `abi_drift_gate` table
//! further down this file — those were re-derived from the current sources in
//! issue #1346 and are the ones to trust.
//!
//! ## IGatewayEvents — `contracts/gateway/interfaces/IGateway.sol`
//!
//! ### `AgentAuthorized` — DRIFTED (fix in issue #366)
//! - **abi.rs (current):**
//!   `AgentAuthorized(address indexed agent, uint64 validUntil, uint256 maxPerPayment, uint256 maxPerWindow, address shareReceiver)`
//! - **IGateway.sol:74 (authoritative):**
//!   `AgentAuthorized(address indexed agent, address indexed owner, uint64 validUntil, uint256 maxPerPayment, uint256 maxPerWindow, address shareReceiver)`
//! - **Missing:** `address indexed owner` as second parameter.
//! - **Topic-0 impact:** `keccak256("AgentAuthorized(address,uint64,uint256,uint256,address)")` ≠
//!   `keccak256("AgentAuthorized(address,address,uint64,uint256,uint256,address)")` — all
//!   `AgentAuthorized` events are silently dropped by the indexer.
//!
//! ### `AgentRevoked` — DRIFTED (fix in issue #366)
//! - **abi.rs (current):**
//!   `AgentRevoked(address indexed agent)`
//! - **IGateway.sol:85 (authoritative):**
//!   `AgentRevoked(address indexed agent, address indexed owner)`
//! - **Missing:** `address indexed owner` as second parameter.
//! - **Topic-0 impact:** All `AgentRevoked` events silently dropped.
//!
//! ### `AgentDepositRouted` — MISSING (not indexed; add if needed)
//! - **IGateway.sol:119** declares `AgentDepositRouted(...)` for multi-leg router deposits.
//!   Not present in this file — add to `IGatewayEvents` when router path is ingested.
//!
//! ### `AgentWithdrawal` — added by issue #654
//! - **IGateway.sol:139** declares `AgentWithdrawal(bytes32 indexed paymentId,
//!   bytes32 indexed orderId, address indexed agent, address sourceVault,
//!   uint256 shares, uint256 assetsOut, address assetRecipient, uint64 windowId)`.
//!   Indexed in issue #654 — stores a withdrawal history row for the agent.
//!
//! ## IVaultRegistryEvents — `contracts/VaultRegistry.sol`
//!
//! ### `VaultRegistered` — DRIFTED (fix in issue #366)
//! - **abi.rs (current):**
//!   `VaultRegistered(address indexed vault, string name, string riskLabel, uint256 depositCap, uint64 registeredAt)`
//! - **VaultRegistry.sol:67 (authoritative):**
//!   `VaultRegistered(address indexed vault, string name, address indexed asset)`
//! - **Fields removed:** `riskLabel`, `depositCap`, `registeredAt` (no longer in Solidity).
//! - **Fields added:** `asset` (indexed address).
//! - **Topic-0 impact:** All `VaultRegistered` events silently dropped.
//!
//! ### `VaultStatusChanged` — DRIFTED (fix in issue #366)
//! - **abi.rs (current):**
//!   `VaultStatusChanged(address indexed vault, uint8 oldStatus, uint8 newStatus, uint64 changedAt)`
//! - **VaultRegistry.sol:73 (authoritative):**
//!   `VaultStatusChanged(address indexed vault, VaultStatus indexed newStatus, uint256 timestamp)`
//! - **Fields removed:** `oldStatus`, `changedAt`.
//! - **Fields changed:** `newStatus` is now `indexed`; `timestamp` is `uint256` not `uint64`.
//! - **Topic-0 impact:** All `VaultStatusChanged` events silently dropped.
//!
//! ## IRouterGovernanceEvents — `contracts/RouterGovernance.sol`
//!
//! ### `ProposalCreated` — DRIFTED (fix in issue #366)
//! - **abi.rs (current):**
//!   `ProposalCreated(uint256 indexed proposalId, address indexed proposer, string description, uint256 deadlineBlock, uint64 createdAt)`
//! - **RouterGovernance.sol:106 (authoritative):**
//!   `ProposalCreated(uint256 indexed proposalId, address indexed proposer, address[] vaults, uint256[] bps, uint64 votingDeadline)`
//! - **Fields changed:** `description`→`address[] vaults`, `deadlineBlock`→`uint256[] bps`, `createdAt`→`uint64 votingDeadline`.
//! - **Topic-0 impact:** All `ProposalCreated` events silently dropped.
//!
//! ### `VoteCast` — DRIFTED (fix in issue #366)
//! - **abi.rs (current):**
//!   `VoteCast(uint256 indexed proposalId, address indexed voter, bool support, uint256 weight)`
//! - **RouterGovernance.sol:119 (authoritative):**
//!   `VoteCast(uint256 indexed proposalId, address indexed voter, uint256 power, uint256 totalFor)`
//! - **Fields changed:** `bool support` removed, `weight`→`power`, `totalFor` added.
//! - **Topic-0 impact:** All `VoteCast` events silently dropped.
//!
//! ### `ProposalExecuted` — DRIFTED (fix in issue #366)
//! - **abi.rs (current):**
//!   `ProposalExecuted(uint256 indexed proposalId)`
//! - **RouterGovernance.sol:126 (authoritative):**
//!   `ProposalExecuted(uint256 indexed proposalId, address indexed executor)`
//! - **Missing:** `address indexed executor`.
//! - **Topic-0 impact:** All `ProposalExecuted` events silently dropped.
//!
//! ### `WeightsSet` → `WeightsApplied` — NAME AND SIGNATURE DRIFTED (fix in issue #366)
//! - **abi.rs (current):**
//!   `WeightsSet(address[] vaults, uint256[] bps)`
//! - **RouterGovernance.sol:132 (authoritative):**
//!   `WeightsApplied(uint256 indexed proposalId, address[] vaults, uint256[] bps)`
//! - **Name changed** from `WeightsSet` to `WeightsApplied`; `proposalId` added.
//! - **Topic-0 impact:** All `WeightsApplied` events silently dropped (wrong name too).
//!
//! # Vault Pause Split Seam (issue #368)
//!
//! `RobotMoneyVault` inherits OZ `Pausable` which uses a single boolean pause flag.
//! Both `_deposit` (line 303: `whenNotPaused`) and `_withdraw` (line 401: `whenNotPaused`)
//! gate on the same flag.  `emergencyWithdraw()` (line 610) calls `_pause()` then drains
//! adapters — leaving USDC idle in the vault — but blocks all user redemptions because
//! `_withdraw` is still guarded by `whenNotPaused`.
//!
//! **State variables to split for issue #368:**
//! - Replace OZ `Pausable` (single `_paused` bool) with two independent booleans:
//!   `depositsPaused` and `withdrawalsPaused` (or equivalent modifier split).
//! - `pause()` (EMERGENCY_ROLE) sets both to `true`.
//! - `emergencyWithdraw()` (EMERGENCY_ROLE) sets only `depositsPaused = true`.
//! - `unpause()` (ADMIN_ROLE) clears both.
//! - `_deposit` guards on `depositsPaused`; `_withdraw` guards on `withdrawalsPaused`.
//! - Coupling risk: removing OZ `Pausable` also removes `Paused(address)`/`Unpaused(address)`
//!   events from the inherited contract — those are re-declared in `IGateway` (gateway-side);
//!   the vault emits them via `_pause()`/`_unpause()` calls today.  Issue #368 must either
//!   retain those OZ events or re-emit them manually from the new modifier paths.
//!
//! # Gateway Pinned-Vault vs Multi-Vault Constraint (issue #370)
//!
//! `IGateway.sol:35` documents `allowedDestinations`: "An empty array disables the allowlist —
//! any registered vault or the router is permitted." `IGateway.sol:46` similarly documents
//! `allowedSourceVaults`: "An empty array permits any registered vault."
//!
//! **Actual enforcement in `RobotMoneyGateway.sol`:**
//! - `_validateDestination` (line 473): checks `destination == address(vaultContract)` OR
//!   `destination == address(routerContract)` — only the single pinned vault and the router.
//! - `withdraw()` (line 599): `if (sourceVault != address(vaultContract)) revert InvalidSourceVault()`.
//! - **Conclusion: Option A (docs fix) is sufficient.** There is no VaultRegistry injection
//!   point in the constructor or storage — the immutables `vaultContract` and `routerContract`
//!   are the only valid targets. Issue #370 should update IGateway NatSpec for both
//!   `allowedDestinations` (line 35-40) and `allowedSourceVaults` (line 46-48) to state:
//!   "When empty, only the pinned vault (`vault()`) and the portfolio router (`router()`) are
//!   accepted." No contract logic change is required for Option A.
//!
//! # Governance Documentation Ownership (issue #372)
//!
//! `RouterGovernance.sol` implements admin-weighted voting via explicit `votingPower` mapping
//! (set by `ADMIN_ROLE` via `setVotingPower`).  The contract is NOT token-holder governance.
//! Documentation in `docs/prd.md` and NatSpec should label it as an admin-weighted MVP mock.
//! The `WeightsApplied` event (not `WeightsSet`) is the canonical on-chain signal that the
//! weight vector changed as a result of a governance proposal execution.

use alloy_primitives::{keccak256, B256};
use alloy_sol_types::sol;

sol! {
    /// Event surface from `RouterGovernance`.
    ///
    /// Signatures match `RouterGovernance.sol` exactly so that
    /// `SolEvent::SIGNATURE_HASH` and the `Topics` keccak strings agree
    /// with the on-chain topic-0.  See `docs/architecture.md §5.4`.
    #[allow(missing_docs)]
    interface IRouterGovernanceEvents {
        /// Emitted when a new governance proposal is created.
        /// RouterGovernance.sol:163
        event ProposalCreated(
            uint256 indexed proposalId,
            address indexed proposer,
            address[] vaults,
            uint256[] bps,
            uint64  votingDeadline
        );

        /// Emitted when a voter casts a vote in favour.
        /// RouterGovernance.sol:176
        event VoteCast(
            uint256 indexed proposalId,
            address indexed voter,
            uint256 power,
            uint256 totalFor
        );

        /// Emitted when a queued proposal is executed.
        /// RouterGovernance.sol:183
        event ProposalExecuted(uint256 indexed proposalId, address indexed executor);

        /// Emitted by RouterGovernance when the router weight vector is updated.
        /// RouterGovernance.sol:189
        event WeightsApplied(uint256 indexed proposalId, address[] vaults, uint256[] bps);
    }

    /// Event surface from `IGateway`. Names match the Solidity source so
    /// `SolEvent::SIGNATURE_HASH` lines up with the on-chain topic.
    #[allow(missing_docs)]
    interface IGatewayEvents {
        /// IGateway.sol:97
        event AgentAuthorized(
            address indexed agent,
            address indexed owner,
            uint64 validUntil,
            uint256 maxPerPayment,
            uint256 maxPerWindow,
            address shareReceiver
        );
        /// IGateway.sol:108
        event AgentRevoked(address indexed agent, address indexed owner);
        event Paused(address indexed by);
        event Unpaused(address indexed by);
        event AgentDeposit(
            bytes32 indexed paymentId,
            bytes32 indexed orderId,
            address indexed agent,
            address shareReceiver,
            uint256 amount,
            uint256 sharesMinted,
            uint64 windowId
        );
        /// IGateway.sol:162 — emitted on every successful agent withdrawal.
        event AgentWithdrawal(
            bytes32 indexed paymentId,
            bytes32 indexed orderId,
            address indexed agent,
            address sourceVault,
            uint256 shares,
            uint256 assetsOut,
            address assetRecipient,
            uint64  windowId
        );
        /// IGateway.sol:142 — emitted for router-path deposits (multi-leg).
        event AgentDepositRouted(
            bytes32 indexed paymentId,
            bytes32 indexed orderId,
            address indexed agent,
            address shareReceiver,
            address router,
            uint256 amount,
            uint256[] sharesPerLeg,
            uint64  windowId
        );
    }

    /// Event surface from `RobotMoneyVault`. Trigger set for state
    /// snapshots per ADR §3.5.
    #[allow(missing_docs)]
    interface IVaultEvents {
        event Allocated(uint256 indexed index, address indexed adapter, uint256 amount);
        event Pulled(uint256 indexed index, address indexed adapter, uint256 amount);
        event Rebalanced(uint256 totalMoved);
        event ExitFeeCharged(
            address indexed owner,
            address indexed receiver,
            uint256 grossAssets,
            uint256 fee,
            uint256 netAssets
        );
        /// ERC-4626 standard deposit event. Emitted by the vault on every deposit
        /// regardless of call path (direct, gateway, or router). Watched so that
        /// router-seeded deposits trigger an event-driven TVL snapshot without
        /// waiting for the SNAPSHOT_HEARTBEAT_BLOCKS interval.
        event Deposit(
            address indexed caller,
            address indexed owner,
            uint256 assets,
            uint256 shares
        );
        /// ERC-4626 standard withdraw event. Watched to snapshot TVL after any redemption.
        event Withdraw(
            address indexed caller,
            address indexed receiver,
            address indexed owner,
            uint256 assets,
            uint256 shares
        );
    }

    /// Read surface for vault state snapshots.
    #[allow(missing_docs)]
    interface IVaultReads {
        function totalAssets() external view returns (uint256);
        function totalSupply() external view returns (uint256);
        function exitFeeBps() external view returns (uint256);
        function tvlCap() external view returns (uint256);
        function paused() external view returns (bool);
    }

    /// Event surface from `VaultRegistry`.  Signatures match `VaultRegistry.sol`
    /// exactly — see §3.5 in `docs/technical/vault-registry-decisions.md`.
    #[allow(missing_docs)]
    interface IVaultRegistryEvents {
        /// Emitted once when a vault is added to the registry.
        /// VaultRegistry.sol:146
        event VaultRegistered(
            address indexed vault,
            string  name,
            address indexed asset
        );

        /// Emitted each time an admin changes a vault's operational status.
        /// VaultRegistry.sol:152 — `VaultStatus` enum encodes as uint8 in ABI.
        event VaultStatusChanged(
            address indexed vault,
            uint8   indexed newStatus,
            uint256         timestamp
        );
    }

    /// Event surface from `PortfolioRouter`.  The `RouterDeposit` event is
    /// emitted once per vault leg on every successful `deposit()` call.
    /// `WeightsSet` and `DefaultWeightsSet` are emitted by admin calls to
    /// `setWeights()` and `setDefaultWeights()` respectively — the direct-seed
    /// paths taken by the demo seed script (not governance execution).
    ///
    /// Signatures match `PortfolioRouter.sol` exactly so that
    /// `SolEvent::SIGNATURE_HASH` agrees with the on-chain topic-0.
    #[allow(missing_docs)]
    interface IPortfolioRouterEvents {
        /// Emitted once per vault leg on each `deposit()` call.
        /// PortfolioRouter.sol:122
        event RouterDeposit(
            address indexed depositor,
            address indexed vault,
            uint256 amount,
            uint256 shares,
            uint256 weightBps
        );

        /// Emitted when the voted weight vector is set directly by admin.
        /// PortfolioRouter.sol:133
        event WeightsSet(address[] vaults, uint256[] bps);

        /// Emitted when the default (below-quorum fallback) weight vector is
        /// set by ADMIN_ROLE.
        /// PortfolioRouter.sol:139
        event DefaultWeightsSet(address[] vaults, uint256[] bps);
    }

    /// Minimum stable read surface for `VaultRegistry`.  Defined in
    /// `docs/technical/vault-registry-decisions.md` §3.4.
    #[allow(missing_docs)]
    interface IVaultRegistryReads {
        /// Returns all registered vault addresses regardless of status.
        function listVaults() external view returns (address[] memory);
        /// Returns the number of registered vaults (all statuses).
        function vaultCount() external view returns (uint256);
    }

    /// Event surface from `InvestmentCommitteePolicy`.
    ///
    /// Signatures match `contracts/gateway/InvestmentCommitteePolicy.sol` and
    /// `contracts/gateway/interfaces/IInvestmentCommitteePolicy.sol` exactly.
    /// See `docs/architecture.md §5.4` and issue #1053.
    ///
    /// Three events are indexed:
    ///   AgentRegistered  — upserts committee_agents row.
    ///   AgentRevoked     — sets committee_agents.active = false.
    ///   VoteSubmitted    — stores commitment; memo hash-verification sets verified.
    #[allow(missing_docs)]
    interface IInvestmentCommitteePolicyEvents {
        /// Emitted when ADMIN_ROLE registers a committee agent.
        /// InvestmentCommitteePolicy.sol:104
        event AgentRegistered(address indexed agent, string agentId);

        /// Emitted when ADMIN_ROLE revokes a committee agent.
        /// InvestmentCommitteePolicy.sol:107
        event AgentRevoked(address indexed agent);

        /// Emitted when an allowlisted agent submits a signed vote.
        /// InvestmentCommitteePolicy.sol:110
        /// Stance ABI-encodes as uint8: Overweight=0, Neutral=1, Underweight=2.
        event VoteSubmitted(
            uint256 indexed voteId,
            address indexed agent,
            address indexed vault,
            uint8   stance,
            uint16  targetWeightBps,
            uint8   confidence,
            string  rationaleUri,
            bytes32 voteJsonHash,
            uint64  timestamp
        );
    }

    /// Event surface from `ConsensusRecommendationReceipt`.
    ///
    /// Signatures match `contracts/gateway/ConsensusRecommendationReceipt.sol` and
    /// `contracts/gateway/interfaces/IConsensusRecommendationReceipt.sol` exactly.
    /// See `docs/architecture.md §4.9` and issue #1247.
    ///
    /// Two events are indexed:
    ///   ReceiptRecorded — inserts a consensus_receipts row; the indexer fetches
    ///                     `payloadUri` and sets `verified` from the keccak256
    ///                     comparison against `payloadDigest`.
    ///   ReceiptReleased — flips `released` / `released_at` on the existing row.
    ///
    /// **No event carries a signature parameter** (architecture §4.9): the
    /// analysts' ed25519 signatures are payload data verified off-chain, never
    /// event data.
    #[allow(missing_docs)]
    interface IConsensusRecommendationReceiptEvents {
        /// Emitted when a committee submitter records a receipt commitment.
        /// IConsensusRecommendationReceipt.sol:51 — three indexed params (EVM limit).
        event ReceiptRecorded(
            bytes32 indexed receiptId,
            address indexed submitter,
            uint256 indexed index,
            bytes32 payloadDigest,
            string  payloadUri,
            uint64  recordedAt
        );

        /// Emitted when ADMIN_ROLE (the timelock) releases a recorded receipt.
        /// IConsensusRecommendationReceipt.sol:63
        event ReceiptReleased(bytes32 indexed receiptId, address indexed releasedBy, uint64 releasedAt);
    }
}

/// Topic-0 hashes the indexer matches on `eth_getLogs`. Computed once
/// at startup from the canonical event signature strings.
pub struct Topics {
    pub agent_authorized: B256,
    pub agent_revoked: B256,
    pub agent_deposit: B256,
    /// Agent-initiated withdrawal emitted by IGateway.sol:162.
    pub agent_withdrawal: B256,
    /// Multi-leg router deposit emitted by IGateway.sol:142.
    pub agent_deposit_routed: B256,
    pub paused: B256,
    pub unpaused: B256,
    pub vault_allocated: B256,
    pub vault_pulled: B256,
    pub vault_rebalanced: B256,
    pub vault_exit_fee_charged: B256,
    // VaultRegistry events.
    pub vault_registered: B256,
    pub vault_status_changed: B256,
    // RouterGovernance events — docs/architecture.md §5.4.
    pub proposal_created: B256,
    pub vote_cast: B256,
    pub proposal_executed: B256,
    pub weights_applied: B256,
    /// Per-leg deposit emitted by PortfolioRouter.sol:122.
    pub router_deposit: B256,
    /// Voted weight vector set directly by admin — PortfolioRouter.sol:133.
    /// Emitted by `setWeights()` (demo-seed and direct-admin path, not governance).
    pub weights_set: B256,
    /// Default (fallback) weight vector set by ADMIN_ROLE — PortfolioRouter.sol:139.
    /// Emitted by `setDefaultWeights()`.
    pub default_weights_set: B256,
    /// ERC-4626 Deposit event emitted by any vault on every deposit.
    /// Watching this ensures TVL snapshots fire after router-seeded deposits.
    pub erc4626_deposit: B256,
    /// ERC-4626 Withdraw event emitted by any vault on every redemption.
    pub erc4626_withdraw: B256,
    // InvestmentCommitteePolicy events — docs/architecture.md §5.4, issue #1053.
    /// Emitted when ADMIN_ROLE registers a committee agent.
    /// InvestmentCommitteePolicy.sol:104
    pub ic_agent_registered: B256,
    /// Emitted when ADMIN_ROLE revokes a committee agent.
    /// InvestmentCommitteePolicy.sol:107
    pub ic_agent_revoked: B256,
    /// Emitted when an allowlisted agent submits a signed vote.
    /// InvestmentCommitteePolicy.sol:110
    pub ic_vote_submitted: B256,
    // ConsensusRecommendationReceipt events — docs/architecture.md §4.9, issue #1247.
    /// Emitted when a committee submitter records a receipt commitment.
    pub consensus_receipt_recorded: B256,
    /// Emitted when ADMIN_ROLE (the timelock) releases a recorded receipt.
    pub consensus_receipt_released: B256,
}

impl Topics {
    pub fn new() -> Self {
        Self {
            agent_authorized: keccak256(
                b"AgentAuthorized(address,address,uint64,uint256,uint256,address)",
            ),
            agent_revoked: keccak256(b"AgentRevoked(address,address)"),
            agent_deposit: keccak256(
                b"AgentDeposit(bytes32,bytes32,address,address,uint256,uint256,uint64)",
            ),
            agent_withdrawal: keccak256(
                b"AgentWithdrawal(bytes32,bytes32,address,address,uint256,uint256,address,uint64)",
            ),
            agent_deposit_routed: keccak256(
                b"AgentDepositRouted(bytes32,bytes32,address,address,address,uint256,uint256[],uint64)",
            ),
            paused: keccak256(b"Paused(address)"),
            unpaused: keccak256(b"Unpaused(address)"),
            vault_allocated: keccak256(b"Allocated(uint256,address,uint256)"),
            vault_pulled: keccak256(b"Pulled(uint256,address,uint256)"),
            vault_rebalanced: keccak256(b"Rebalanced(uint256)"),
            vault_exit_fee_charged: keccak256(
                b"ExitFeeCharged(address,address,uint256,uint256,uint256)",
            ),
            // VaultRegistry — docs/technical/vault-registry-decisions.md §3.5.
            vault_registered: keccak256(b"VaultRegistered(address,string,address)"),
            vault_status_changed: keccak256(b"VaultStatusChanged(address,uint8,uint256)"),
            // RouterGovernance — docs/architecture.md §5.4.
            proposal_created: keccak256(
                b"ProposalCreated(uint256,address,address[],uint256[],uint64)",
            ),
            vote_cast: keccak256(b"VoteCast(uint256,address,uint256,uint256)"),
            proposal_executed: keccak256(b"ProposalExecuted(uint256,address)"),
            weights_applied: keccak256(b"WeightsApplied(uint256,address[],uint256[])"),
            router_deposit: keccak256(
                b"RouterDeposit(address,address,uint256,uint256,uint256)",
            ),
            weights_set: keccak256(b"WeightsSet(address[],uint256[])"),
            default_weights_set: keccak256(b"DefaultWeightsSet(address[],uint256[])"),
            erc4626_deposit: keccak256(b"Deposit(address,address,uint256,uint256)"),
            erc4626_withdraw: keccak256(b"Withdraw(address,address,address,uint256,uint256)"),
            // InvestmentCommitteePolicy — issue #1053.
            ic_agent_registered: keccak256(b"AgentRegistered(address,string)"),
            ic_agent_revoked: keccak256(b"AgentRevoked(address)"),
            ic_vote_submitted: keccak256(
                b"VoteSubmitted(uint256,address,address,uint8,uint16,uint8,string,bytes32,uint64)",
            ),
            // ConsensusRecommendationReceipt — issue #1247.
            consensus_receipt_recorded: keccak256(
                b"ReceiptRecorded(bytes32,address,uint256,bytes32,string,uint64)",
            ),
            consensus_receipt_released: keccak256(b"ReceiptReleased(bytes32,address,uint64)"),
        }
    }

    /// All topic-0s the indexer subscribes to, suitable for an
    /// `eth_getLogs` `topics: [[t0, t1, ...]]` first-slot OR-filter.
    pub fn all_topic0(&self) -> Vec<B256> {
        vec![
            self.agent_authorized,
            self.agent_revoked,
            self.agent_deposit,
            self.agent_withdrawal,
            self.agent_deposit_routed,
            self.paused,
            self.unpaused,
            self.vault_allocated,
            self.vault_pulled,
            self.vault_rebalanced,
            self.vault_exit_fee_charged,
            self.vault_registered,
            self.vault_status_changed,
            self.proposal_created,
            self.vote_cast,
            self.proposal_executed,
            self.weights_applied,
            self.router_deposit,
            self.weights_set,
            self.default_weights_set,
            self.erc4626_deposit,
            self.erc4626_withdraw,
            // InvestmentCommitteePolicy — issue #1053.
            self.ic_agent_registered,
            self.ic_agent_revoked,
            self.ic_vote_submitted,
            // ConsensusRecommendationReceipt — issue #1247. A topic omitted from this
            // list is silently never fetched by `eth_getLogs`.
            self.consensus_receipt_recorded,
            self.consensus_receipt_released,
        ]
    }
}

impl Default for Topics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolEvent;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// Foundry's build-output directory (`out = "out"` in `foundry.toml`),
    /// resolved from this crate's manifest directory so the test is
    /// independent of the working directory `cargo test` was launched from.
    fn foundry_out_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("out")
    }

    /// Canonical ABI type of one parameter, expanding tuples recursively so a
    /// struct parameter renders as `(address,uint256)` — the form that goes
    /// into the topic-0 preimage.
    fn param_type(param: &serde_json::Value) -> String {
        let ty = param["type"].as_str().unwrap_or_default();
        match ty.strip_prefix("tuple") {
            Some(array_suffix) => {
                let components = param["components"]
                    .as_array()
                    .map(|c| c.iter().map(param_type).collect::<Vec<_>>().join(","))
                    .unwrap_or_default();
                format!("({components}){array_suffix}")
            }
            None => ty.to_string(),
        }
    }

    /// Read a Foundry artifact and return every event it declares as
    /// `name -> [topic-0, ...]` (a `Vec` because Solidity permits overloads).
    ///
    /// Panics — loudly, never skips — when the artifact is missing. A missing
    /// `out/` means `forge build` did not run, and a drift gate that quietly
    /// passes when it cannot read the thing it guards is the exact defect
    /// issue #1346 exists to fix.
    fn artifact_event_topics(contract_dir: &str, contract: &str) -> BTreeMap<String, Vec<B256>> {
        let out = foundry_out_dir();
        let path = out.join(contract_dir).join(format!("{contract}.json"));
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "cannot read Foundry artifact {}: {e}\n\
                 This test derives event topic-0 hashes from the compiled contracts, so the \
                 artifacts must exist. Run `forge build` from the repository root first \
                 (CI: see the `forge build` step in .github/workflows/suite-16-abi-drift.yml).",
                path.display()
            )
        });
        let artifact: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("malformed Foundry artifact {}: {e}", path.display()));
        let entries = artifact["abi"]
            .as_array()
            .unwrap_or_else(|| panic!("Foundry artifact {} has no `abi` array", path.display()));

        let mut topics: BTreeMap<String, Vec<B256>> = BTreeMap::new();
        for entry in entries {
            if entry["type"].as_str() != Some("event") {
                continue;
            }
            let name = entry["name"].as_str().unwrap_or_default();
            let params = entry["inputs"]
                .as_array()
                .map(|p| p.iter().map(param_type).collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            let signature = format!("{name}({params})");
            topics
                .entry(name.to_string())
                .or_default()
                .push(keccak256(signature.as_bytes()));
        }
        topics
    }

    /// **The Solidity-truth ABI drift gate (issue #1346).**
    ///
    /// `abi_drift_gate` below compares this file's three hand-maintained
    /// copies of every event signature (the `sol!` declarations, the
    /// `Topics::new()` keccak literals, and its own `SOL_SIGS` table) only
    /// against *each other*. It reads no Solidity, so it cannot fail when a
    /// contract changes — only when someone edits one of the three copies and
    /// forgets the other two. Every copy can be simultaneously, consistently
    /// wrong and that gate still passes green.
    ///
    /// This test closes that hole: it re-derives topic-0 from the compiled
    /// Foundry artifacts in `out/` — the same bytes the deployed contract
    /// emits — and fails if the indexer's topic differs. A Solidity signature
    /// change with no corresponding `abi.rs` change turns this RED, which is
    /// exactly the class of silent event-drop the indexer shipped in #366.
    ///
    /// Requires `forge build` to have populated `out/`; it panics with
    /// instructions rather than skipping when the artifacts are absent.
    #[test]
    fn event_topics_match_foundry_artifacts() {
        let out = foundry_out_dir();
        assert!(
            out.is_dir(),
            "Foundry out/ directory not found at {} — run `forge build` from the repository \
             root before this test. This gate derives topic-0 from compiled artifacts and \
             must never pass without them.",
            out.display()
        );

        let t = Topics::new();

        // (artifact dir, contract, event name, the topic the indexer filters on).
        // One entry per topic-0 in `Topics` that a first-party contract emits.
        let checks: &[(&str, &str, &str, B256)] = &[
            // IGateway events, taken from the concrete implementation's artifact
            // (RobotMoneyGateway inherits the IGateway event surface).
            (
                "RobotMoneyGateway.sol",
                "RobotMoneyGateway",
                "AgentAuthorized",
                t.agent_authorized,
            ),
            (
                "RobotMoneyGateway.sol",
                "RobotMoneyGateway",
                "AgentRevoked",
                t.agent_revoked,
            ),
            (
                "RobotMoneyGateway.sol",
                "RobotMoneyGateway",
                "AgentDeposit",
                t.agent_deposit,
            ),
            (
                "RobotMoneyGateway.sol",
                "RobotMoneyGateway",
                "AgentDepositRouted",
                t.agent_deposit_routed,
            ),
            (
                "RobotMoneyGateway.sol",
                "RobotMoneyGateway",
                "AgentWithdrawal",
                t.agent_withdrawal,
            ),
            (
                "RobotMoneyGateway.sol",
                "RobotMoneyGateway",
                "Paused",
                t.paused,
            ),
            (
                "RobotMoneyGateway.sol",
                "RobotMoneyGateway",
                "Unpaused",
                t.unpaused,
            ),
            // RobotMoneyVault — vault state-snapshot triggers plus the two
            // ERC-4626 standard events the vault inherits.
            (
                "RobotMoneyVault.sol",
                "RobotMoneyVault",
                "Allocated",
                t.vault_allocated,
            ),
            (
                "RobotMoneyVault.sol",
                "RobotMoneyVault",
                "Pulled",
                t.vault_pulled,
            ),
            (
                "RobotMoneyVault.sol",
                "RobotMoneyVault",
                "Rebalanced",
                t.vault_rebalanced,
            ),
            (
                "RobotMoneyVault.sol",
                "RobotMoneyVault",
                "ExitFeeCharged",
                t.vault_exit_fee_charged,
            ),
            (
                "RobotMoneyVault.sol",
                "RobotMoneyVault",
                "Deposit",
                t.erc4626_deposit,
            ),
            (
                "RobotMoneyVault.sol",
                "RobotMoneyVault",
                "Withdraw",
                t.erc4626_withdraw,
            ),
            // VaultRegistry.
            (
                "VaultRegistry.sol",
                "VaultRegistry",
                "VaultRegistered",
                t.vault_registered,
            ),
            (
                "VaultRegistry.sol",
                "VaultRegistry",
                "VaultStatusChanged",
                t.vault_status_changed,
            ),
            // RouterGovernance.
            (
                "RouterGovernance.sol",
                "RouterGovernance",
                "ProposalCreated",
                t.proposal_created,
            ),
            (
                "RouterGovernance.sol",
                "RouterGovernance",
                "VoteCast",
                t.vote_cast,
            ),
            (
                "RouterGovernance.sol",
                "RouterGovernance",
                "ProposalExecuted",
                t.proposal_executed,
            ),
            (
                "RouterGovernance.sol",
                "RouterGovernance",
                "WeightsApplied",
                t.weights_applied,
            ),
            // PortfolioRouter.
            (
                "PortfolioRouter.sol",
                "PortfolioRouter",
                "RouterDeposit",
                t.router_deposit,
            ),
            (
                "PortfolioRouter.sol",
                "PortfolioRouter",
                "WeightsSet",
                t.weights_set,
            ),
            (
                "PortfolioRouter.sol",
                "PortfolioRouter",
                "DefaultWeightsSet",
                t.default_weights_set,
            ),
            // InvestmentCommitteePolicy — issue #1053.
            (
                "InvestmentCommitteePolicy.sol",
                "InvestmentCommitteePolicy",
                "AgentRegistered",
                t.ic_agent_registered,
            ),
            (
                "InvestmentCommitteePolicy.sol",
                "InvestmentCommitteePolicy",
                "AgentRevoked",
                t.ic_agent_revoked,
            ),
            (
                "InvestmentCommitteePolicy.sol",
                "InvestmentCommitteePolicy",
                "VoteSubmitted",
                t.ic_vote_submitted,
            ),
            // ConsensusRecommendationReceipt — issue #1247.
            (
                "ConsensusRecommendationReceipt.sol",
                "ConsensusRecommendationReceipt",
                "ReceiptRecorded",
                t.consensus_receipt_recorded,
            ),
            (
                "ConsensusRecommendationReceipt.sol",
                "ConsensusRecommendationReceipt",
                "ReceiptReleased",
                t.consensus_receipt_released,
            ),
        ];

        // Cache each artifact so a contract's JSON is parsed once, not once per event.
        let mut cache: BTreeMap<&str, BTreeMap<String, Vec<B256>>> = BTreeMap::new();
        let mut checked = 0usize;

        for (dir, contract, event, indexer_topic) in checks {
            let artifact = cache
                .entry(contract)
                .or_insert_with(|| artifact_event_topics(dir, contract));
            let candidates = artifact.get(*event).unwrap_or_else(|| {
                panic!(
                    "ABI drift: `{event}` is not declared by {contract} in the compiled \
                     artifact, but services/explorer-indexer/src/abi.rs still filters \
                     `eth_getLogs` on its topic-0. The event was renamed or removed in \
                     Solidity — update abi.rs. Events present: {:?}",
                    artifact.keys().collect::<Vec<_>>()
                )
            });
            assert!(
                candidates.contains(indexer_topic),
                "ABI drift for {contract}.{event}: the indexer filters on topic-0 \
                 {indexer_topic:?}, but the compiled Foundry artifact declares \
                 {candidates:?}. The Solidity signature changed without a matching change \
                 in services/explorer-indexer/src/abi.rs, so every {event} log would be \
                 silently dropped. Update the `sol!` declaration, the `Topics::new()` \
                 keccak literal, and the `abi_drift_gate` table to the new signature."
            );
            checked += 1;
        }

        // Guard against the table itself being emptied or shadowed by a refactor:
        // a gate that checks nothing is indistinguishable from a passing gate.
        assert_eq!(
            checked,
            checks.len(),
            "not every entry in the artifact cross-check table was evaluated"
        );
        assert!(
            checked >= 27,
            "artifact cross-check covered only {checked} events — entries were removed \
             from the table without removing the corresponding topic from `Topics`"
        );
    }

    /// Sanity-check: locally-computed topic hashes match what `sol!`
    /// derives. If a contract event signature drifts, this catches it.
    #[test]
    fn topic_hashes_match_sol_macros() {
        let t = Topics::new();
        assert_eq!(
            t.agent_deposit,
            IGatewayEvents::AgentDeposit::SIGNATURE_HASH
        );
        assert_eq!(
            t.agent_deposit_routed,
            IGatewayEvents::AgentDepositRouted::SIGNATURE_HASH
        );
        assert_eq!(
            t.agent_withdrawal,
            IGatewayEvents::AgentWithdrawal::SIGNATURE_HASH
        );
        assert_eq!(
            t.agent_authorized,
            IGatewayEvents::AgentAuthorized::SIGNATURE_HASH
        );
        assert_eq!(
            t.agent_revoked,
            IGatewayEvents::AgentRevoked::SIGNATURE_HASH
        );
        assert_eq!(t.paused, IGatewayEvents::Paused::SIGNATURE_HASH);
        assert_eq!(t.unpaused, IGatewayEvents::Unpaused::SIGNATURE_HASH);
        assert_eq!(t.vault_allocated, IVaultEvents::Allocated::SIGNATURE_HASH);
        assert_eq!(t.vault_pulled, IVaultEvents::Pulled::SIGNATURE_HASH);
        assert_eq!(t.vault_rebalanced, IVaultEvents::Rebalanced::SIGNATURE_HASH);
        assert_eq!(
            t.vault_exit_fee_charged,
            IVaultEvents::ExitFeeCharged::SIGNATURE_HASH
        );
        // VaultRegistry events.
        assert_eq!(
            t.vault_registered,
            IVaultRegistryEvents::VaultRegistered::SIGNATURE_HASH
        );
        assert_eq!(
            t.vault_status_changed,
            IVaultRegistryEvents::VaultStatusChanged::SIGNATURE_HASH
        );
        // RouterGovernance — docs/architecture.md §5.4.
        assert_eq!(
            t.proposal_created,
            IRouterGovernanceEvents::ProposalCreated::SIGNATURE_HASH
        );
        assert_eq!(
            t.vote_cast,
            IRouterGovernanceEvents::VoteCast::SIGNATURE_HASH
        );
        assert_eq!(
            t.proposal_executed,
            IRouterGovernanceEvents::ProposalExecuted::SIGNATURE_HASH
        );
        assert_eq!(
            t.weights_applied,
            IRouterGovernanceEvents::WeightsApplied::SIGNATURE_HASH
        );
        // PortfolioRouter events.
        assert_eq!(
            t.router_deposit,
            IPortfolioRouterEvents::RouterDeposit::SIGNATURE_HASH
        );
        assert_eq!(
            t.weights_set,
            IPortfolioRouterEvents::WeightsSet::SIGNATURE_HASH
        );
        assert_eq!(
            t.default_weights_set,
            IPortfolioRouterEvents::DefaultWeightsSet::SIGNATURE_HASH
        );
        // ERC-4626 standard events.
        assert_eq!(t.erc4626_deposit, IVaultEvents::Deposit::SIGNATURE_HASH);
        assert_eq!(t.erc4626_withdraw, IVaultEvents::Withdraw::SIGNATURE_HASH);
        // InvestmentCommitteePolicy — issue #1053.
        assert_eq!(
            t.ic_agent_registered,
            IInvestmentCommitteePolicyEvents::AgentRegistered::SIGNATURE_HASH
        );
        assert_eq!(
            t.ic_agent_revoked,
            IInvestmentCommitteePolicyEvents::AgentRevoked::SIGNATURE_HASH
        );
        assert_eq!(
            t.ic_vote_submitted,
            IInvestmentCommitteePolicyEvents::VoteSubmitted::SIGNATURE_HASH
        );
        // ConsensusRecommendationReceipt — issue #1247.
        assert_eq!(
            t.consensus_receipt_recorded,
            IConsensusRecommendationReceiptEvents::ReceiptRecorded::SIGNATURE_HASH
        );
        assert_eq!(
            t.consensus_receipt_released,
            IConsensusRecommendationReceiptEvents::ReceiptReleased::SIGNATURE_HASH
        );
    }

    /// Every topic-0 the indexer knows about must also appear in
    /// `all_topic0()` — a topic omitted there is never requested from
    /// `eth_getLogs`, so its events are silently dropped with no error.
    /// Issue #1247 added the two ConsensusRecommendationReceipt topics; this gate
    /// keeps them (and the pre-existing set) wired to the fetch filter.
    #[test]
    fn all_topic0_includes_consensus_receipt_topics() {
        let t = Topics::new();
        let all = t.all_topic0();
        assert!(
            all.contains(&t.consensus_receipt_recorded),
            "ReceiptRecorded topic-0 missing from Topics::all_topic0() — \
             the indexer would never fetch it"
        );
        assert!(
            all.contains(&t.consensus_receipt_released),
            "ReceiptReleased topic-0 missing from Topics::all_topic0() — \
             the indexer would never fetch it"
        );
        // No duplicate topic-0 may appear in the OR-filter.
        let mut sorted = all.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            all.len(),
            "duplicate topic-0 in Topics::all_topic0()"
        );
    }

    /// CI ABI drift gate — compares `sol!`-derived SIGNATURE_HASH constants
    /// against canonical topic-0 values computed at runtime from the
    /// authoritative event signature strings in the Solidity sources.
    ///
    /// This test is the automated ABI drift check required by issue #366.
    /// It catches any mismatch between:
    ///   1. The `sol!` event declarations in this file, and
    ///   2. The signature strings hand-transcribed into the table below.
    ///
    /// If any field is added, removed, or renamed in `abi.rs` without a
    /// matching change in the table below (or vice-versa), the test fails.
    ///
    /// **What it cannot catch (issue #1346).** Both sides of every comparison
    /// here are hand-maintained copies living in this file; the test reads no
    /// Solidity and no build artifact, so a signature change in the contracts
    /// leaves it green. `event_topics_match_foundry_artifacts` above is the
    /// half that reads the compiled `out/` artifacts and is therefore the gate
    /// that actually fails on real Solidity drift. Keep both: this one localises
    /// an internal inconsistency to a specific copy, that one establishes truth.
    ///
    /// Source references:
    ///   IGateway.sol:97,108,123,142  VaultRegistry.sol:146,152
    ///   RouterGovernance.sol:163,176,183,189  PortfolioRouter.sol:122
    #[test]
    fn abi_drift_gate() {
        use alloy_primitives::keccak256;

        // One entry per indexed event: (event_name, canonical_signature, sol_macro_hash).
        let checks: &[(&str, &[u8], B256)] = &[
            // IGateway.sol:97
            (
                "AgentAuthorized",
                b"AgentAuthorized(address,address,uint64,uint256,uint256,address)",
                IGatewayEvents::AgentAuthorized::SIGNATURE_HASH,
            ),
            // IGateway.sol:108
            (
                "AgentRevoked",
                b"AgentRevoked(address,address)",
                IGatewayEvents::AgentRevoked::SIGNATURE_HASH,
            ),
            // IGateway.sol:123
            (
                "AgentDeposit",
                b"AgentDeposit(bytes32,bytes32,address,address,uint256,uint256,uint64)",
                IGatewayEvents::AgentDeposit::SIGNATURE_HASH,
            ),
            // IGateway.sol:162
            (
                "AgentWithdrawal",
                b"AgentWithdrawal(bytes32,bytes32,address,address,uint256,uint256,address,uint64)",
                IGatewayEvents::AgentWithdrawal::SIGNATURE_HASH,
            ),
            // IGateway.sol:142
            (
                "AgentDepositRouted",
                b"AgentDepositRouted(bytes32,bytes32,address,address,address,uint256,uint256[],uint64)",
                IGatewayEvents::AgentDepositRouted::SIGNATURE_HASH,
            ),
            // VaultRegistry.sol:146
            (
                "VaultRegistered",
                b"VaultRegistered(address,string,address)",
                IVaultRegistryEvents::VaultRegistered::SIGNATURE_HASH,
            ),
            // VaultRegistry.sol:152 — VaultStatus enum ABI-encodes as uint8
            (
                "VaultStatusChanged",
                b"VaultStatusChanged(address,uint8,uint256)",
                IVaultRegistryEvents::VaultStatusChanged::SIGNATURE_HASH,
            ),
            // RouterGovernance.sol:163
            (
                "ProposalCreated",
                b"ProposalCreated(uint256,address,address[],uint256[],uint64)",
                IRouterGovernanceEvents::ProposalCreated::SIGNATURE_HASH,
            ),
            // RouterGovernance.sol:176
            (
                "VoteCast",
                b"VoteCast(uint256,address,uint256,uint256)",
                IRouterGovernanceEvents::VoteCast::SIGNATURE_HASH,
            ),
            // RouterGovernance.sol:183
            (
                "ProposalExecuted",
                b"ProposalExecuted(uint256,address)",
                IRouterGovernanceEvents::ProposalExecuted::SIGNATURE_HASH,
            ),
            // RouterGovernance.sol:189
            (
                "WeightsApplied",
                b"WeightsApplied(uint256,address[],uint256[])",
                IRouterGovernanceEvents::WeightsApplied::SIGNATURE_HASH,
            ),
            // PortfolioRouter.sol:122
            (
                "RouterDeposit",
                b"RouterDeposit(address,address,uint256,uint256,uint256)",
                IPortfolioRouterEvents::RouterDeposit::SIGNATURE_HASH,
            ),
            // PortfolioRouter.sol:133 — direct admin weight-set (demo-seed path)
            (
                "WeightsSet",
                b"WeightsSet(address[],uint256[])",
                IPortfolioRouterEvents::WeightsSet::SIGNATURE_HASH,
            ),
            // PortfolioRouter.sol:139 — default (fallback) weight-set by ADMIN_ROLE
            (
                "DefaultWeightsSet",
                b"DefaultWeightsSet(address[],uint256[])",
                IPortfolioRouterEvents::DefaultWeightsSet::SIGNATURE_HASH,
            ),
            // ERC-4626 standard events (OpenZeppelin ERC4626 — inherited by all vaults)
            (
                "Deposit",
                b"Deposit(address,address,uint256,uint256)",
                IVaultEvents::Deposit::SIGNATURE_HASH,
            ),
            (
                "Withdraw",
                b"Withdraw(address,address,address,uint256,uint256)",
                IVaultEvents::Withdraw::SIGNATURE_HASH,
            ),
            // InvestmentCommitteePolicy.sol:104,107,110 — issue #1053.
            // Stance ABI-encodes as uint8.
            (
                "AgentRegistered",
                b"AgentRegistered(address,string)",
                IInvestmentCommitteePolicyEvents::AgentRegistered::SIGNATURE_HASH,
            ),
            (
                "IC AgentRevoked",
                b"AgentRevoked(address)",
                IInvestmentCommitteePolicyEvents::AgentRevoked::SIGNATURE_HASH,
            ),
            (
                "VoteSubmitted",
                b"VoteSubmitted(uint256,address,address,uint8,uint16,uint8,string,bytes32,uint64)",
                IInvestmentCommitteePolicyEvents::VoteSubmitted::SIGNATURE_HASH,
            ),
            // ConsensusRecommendationReceipt.sol — issue #1247, docs/architecture.md §4.9.
            // Exactly two events; neither carries a signature parameter.
            (
                "ReceiptRecorded",
                b"ReceiptRecorded(bytes32,address,uint256,bytes32,string,uint64)",
                IConsensusRecommendationReceiptEvents::ReceiptRecorded::SIGNATURE_HASH,
            ),
            (
                "ReceiptReleased",
                b"ReceiptReleased(bytes32,address,uint64)",
                IConsensusRecommendationReceiptEvents::ReceiptReleased::SIGNATURE_HASH,
            ),
        ];

        for (name, sig, sol_hash) in checks {
            let canonical = keccak256(sig);
            assert_eq!(
                canonical, *sol_hash,
                "ABI drift detected for {name}: \
                 sol! declaration topic-0 ({sol_hash:?}) \
                 does not match canonical signature topic-0 ({canonical:?}). \
                 Update the sol! declaration in abi.rs to match the contract source."
            );
        }
    }
}
