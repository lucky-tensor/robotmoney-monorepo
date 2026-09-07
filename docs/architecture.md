# Robot Money Architecture

> Canonical sources: `docs/prd.md`, `docs/technical/definitions.md`,
> `docs/technical/adapter-architecture.md`,
> `docs/technical/smart-contracts.md`, and accepted ADRs under
> `docs/technical/`. This document describes how Robot Money is built.
> Product promises and user workflows live in the PRD; delivery order
> lives on the Plan tracking issue (#109).

## 1. Overview

Robot Money is a USDC treasury system for human depositors, autonomous
agents, and governance voters. The product architecture has three on-chain
allocation layers: the Portfolio Router at the outer product layer,
individual Robot Money vaults at the exposure layer, and vault adapters
inside each vault for venue-specific strategy execution. Agent access has
a separate permission and safety layer: the gateway. Human and agent
clients share the same read-before-write safety model: chain state is the
authority for signing and execution, while indexed data is used only for
display, history, and public observability.

Current governance uses admin-assigned voting power: `ADMIN_ROLE` assigns
each voter's power and controls proposal creation. Token-holder voting
(RM-balance-weighted) is a future goal and is not active in the current
deployment. See §2.3 and `docs/prd.md` §"Allocation Governance".

A fourth surface, the agentic **Investment Committee**, sits upstream of
governance: admin-allowlisted agents register an on-chain identity and
submit signed per-vault allocation tilts that feed weight governance as a
**signalling-only** input — they never move funds or set router weights.
Canonical scope:
`docs/product/20260623-product-proposal-investment-committee-v0.md`,
`docs/prd.md` §"Committee", and issue #1044. See §2.4, §4.8, §5.1, §5.3,
§5.4, and §5.5.

## 2. Core Model

### 2.1 Allocation Layers

```text
Human wallet
  -> direct vault deposit
  -> or Portfolio Router deposit

Agent client / rmpc
  -> gateway permission and safety checks
  -> allowed vault or Portfolio Router action

Portfolio Router
  -> active underlying Robot Money vaults by RM-governed weights

Robot Money vault
  -> internal strategy adapters by vault-controlled routing/caps

Vault adapter
  -> external venue or strategy
```

The Portfolio Router allocates across vaults. Vaults allocate internally
through adapters. Users, agents, and the Portfolio Router consume vault
surfaces; they do not call adapters directly.

The gateway is not an allocation layer. It is the on-chain permission and
agent-safety layer in front of agent-initiated writes. It answers whether
the agent may act, for how much, until when, on behalf of which depositor,
to which share receiver, and into which allowed destination. After those
checks pass, the gateway forwards the permitted action to a vault or the
Portfolio Router.

Human wallets and agent clients use different permission paths, but they
terminate at the same product surfaces: vaults, the Portfolio Router,
governance reads/writes, and public observability. Architecture should
avoid parallel product semantics for humans and agents; the difference is
who is allowed to sign and which safety checks run before the product
surface is called.

### 2.2 Receipts and Portfolio Positions

Every vault issues its own receipt token. Direct vault deposits and
Portfolio Router deposits both leave users with underlying vault
receipts. The Portfolio Router does not issue an outer share token in the
current product definition.

A portfolio position is therefore a reporting concept computed from a
user's vault receipt balances, vault values, and current router weights.
The composite view in the dapp, CLI, and agent-readable output must
preserve drill-down into each vault, receipt balance, valuation, fee,
weight, and unavailable leg.

### 2.3 Governance Boundary

**Current MVP:** Governance voting power is admin-assigned.
`RouterGovernance.sol` is the shipped governance contract. `ADMIN_ROLE`
assigns each voter's weight and creates proposals; there is no automatic
RM-balance snapshot. Governance controls Portfolio Router target weights
across active vaults and nothing else. It does not govern vault
onboarding, vault retirement, per-vault asset selection, per-vault
strategy internals, adapter selection, adapter caps, fees, or agent
permissions.

**Future goal:** Token-holder voting weighted by RM-balance snapshot
(ERC-20 Votes / EIP-5805) replaces admin assignment once the RM token's
historical-balance interface is confirmed and a real token distribution
exists. The quorum, cadence, execution-delay, and setWeights call-path
decisions are recorded in
`docs/technical/governance-decisions.md`. Until that phase ships,
admin-assigned voting power remains the only active governance model.

The governance read surface must expose proposal state, vote tallies,
cadence metadata, execution state, and the resulting router weights. Those
surfaces are required for both the dapp and programmatic read clients.
See `docs/technical/governance-decisions.md` for the accepted parameters.

### 2.4 Investment Committee Boundary

The Investment Committee is a **distinct mechanism from RouterGovernance**,
and the two must not be conflated. RouterGovernance is the only contract
that sets Portfolio Router target weights (§2.3). The IC policy contract
(§4.8) holds a different thing: a registry of admin-allowlisted agents and
their signed per-vault allocation tilts. IC output is an **upstream,
signalling-only input** to weight governance — aggregated tilts inform a
RouterGovernance weight proposal, but the IC contract never calls
`setWeights`, never moves funds, and never holds an asset. Application to
live weights stays admin-applied through RouterGovernance's existing path.

Three boundary properties are load-bearing and are enforced architecturally:

- **Signalling-only.** The IC contract grants no treasury-spend and no
  auto-apply authority (custody invariant INV-4, `docs/prd.md` §12). It is
  metadata-only storage.
- **Gateway-routed.** Committee registration and vote submission are
  agent-initiated writes and route through `RobotMoneyGateway` like every
  other agent action (§5.2) — there is no committee side channel.
- **Admin-gated membership.** Only `ADMIN_ROLE` allowlists an agent;
  membership is registry state, not a code variant (§7.3), mirroring
  `VaultRegistry.isRouterEligible`.

## 3. Technology Stack

| Layer | Choice | Rationale | Source |
| --- | --- | --- | --- |
| Chain | Base mainnet, chain id 8453; forked Base for integration tests | Current verified deployments and test strategy are Base-oriented. | `docs/technical/smart-contracts.md` §2; `docs/development/testing-strategy-ethereum.md` § Forked Base mainnet harness |
| Smart contracts | Solidity 0.8.24, EVM Cancun, Foundry | Existing vault, gateway, adapter, and tests use this toolchain. | `foundry.toml`; `docs/technical/smart-contracts.md` §1 |
| Contract libraries | OpenZeppelin v5 ERC-4626, ERC-20, AccessControl, Pausable, ReentrancyGuard | Standardizes vault accounting, role separation, pause behavior, and reentrancy protection. | `docs/technical/smart-contracts.md` §3.1 |
| Primary asset | USDC, 6 decimals | Product accepts USDC as the treasury input asset. | `docs/prd.md` §1; `docs/technical/smart-contracts.md` §1 |
| Vault standard | ERC-4626 for individual vaults | Standard deposit, withdraw, redeem, preview, conversion, and `totalAssets()` surface. | `docs/technical/adapter-architecture.md` §1 |
| Stable-yield venues | Morpho Gauntlet USDC Prime, Aave V3, Compound V3 through vault adapters | Current deployed stable-yield vault normalizes these venues behind adapters. | `docs/technical/adapter-architecture.md` §4; `docs/technical/smart-contracts.md` §4 |
| IC policy contract | Solidity 0.8.24, OpenZeppelin AccessControl + admin-floor, same Foundry toolchain | Signalling-only registry of committee agents and signed tilts; mirrors `RouterGovernance`/`VaultRegistry` role and event conventions; routed via the gateway. | `docs/prd.md` §"Committee", §12 INV-4; proposal doc; issue #1044 |
| Committee vote schema | Fixed-shape JSON schema committed to the repo, validated in CI | Committee votes are the core auditable signal; a valid fixture must pass and an invalid fixture must fail a CI schema job. | `docs/prd.md` §"Committee" (constraints); issue #1044 |
| Committee agent plugin | Skill/plugin extending `robotmoney-analyst` | Reuses the analyst's regime/market datasources, adds form-tilt → sign → submit-vote; proprietary methods stay out of the published surface. | `plugins/robotmoney-analyst/`; proposal doc §3 |
| Agent command client | Rust binary `rmpc` | Builds known calldata, signs through constrained backends, performs direct JSON-RPC reads, and emits stable JSON. | `docs/technical/rmpc-read-output-contract.md` §3 |
| Rust workspace | Cargo workspace, Tokio, reqwest, Alloy, sqlx where applicable | Existing Rust clients, indexer, tests, and shared logging use this stack. | root `Cargo.toml`; client and service `Cargo.toml` files |
| Human dapp | React 18, Vite, TypeScript, wagmi/viem, TanStack Query, Tailwind, Playwright | Current dapp package and ADRs target wallet signing, calldata preview, config export, and browser tests. | `clients/dapp/package.json`; `docs/technical/dapp-credential-decisions.md` §3 |
| Explorer API | Rust Axum service over Postgres | Read-only HTTP API for indexed history and display data. | `clients/explorer-api/Cargo.toml`; `docs/technical/explorer-schema-decisions.md` §3 |
| Explorer indexer | Rust poller, JSON-RPC canonical, Postgres storage | Derives events and snapshots from chain, never from `rmpc` output. | `services/explorer-indexer/Cargo.toml`; `docs/technical/explorer-schema-decisions.md` §3.5 |
| Database | Postgres for explorer/indexer environments | One DB engine for indexed data; no SQLite path. | `docs/technical/explorer-schema-decisions.md` §3.1 |
| Queue / async processing | None in the current architecture | Indexing is poll-based; there is no message queue commitment. | `docs/technical/explorer-schema-decisions.md` §3.2 |
| Auth / identity | Wallet signatures, gateway-enforced agent policies, and on-chain roles | The gateway is the permissions and agent-safety layer; depositors authorize their own agents; protocol roles are narrow and separated. | `docs/prd.md` §3, §5, §9; `docs/technical/security-model.md` §10 |
| File / object storage | Local config, audit logs, build artifacts; no product object store | Current flows use TOML config export and local audit artifacts, not an object-storage service. | `docs/technical/dapp-credential-decisions.md` §3.4 |
| Email / notifications | Unspecified | No canonical doc selects an email or notification provider. | Open decision |
| Payment processing | On-chain USDC only | Fiat on/off ramps are out of scope. | `docs/prd.md` §8 |
| Observability | On-chain events, direct JSON-RPC reads, explorer indexer/API, structured `rmpc` JSON | Every state change must be observable; safety-critical reads stay live-chain. | `docs/prd.md` §2, §5, §7; `docs/technical/explorer-schema-decisions.md` §3.5 |
| Infrastructure / hosting | Base, JSON-RPC providers, Docker devnet, CI-managed services | Production hosting is not fully specified; tests use Base forks and local Geth/Lighthouse devnet. | `docs/development/testing-strategy-ethereum.md`; `docs/development/smoke-test-design.md` |
| CI/CD | GitHub Actions quality gates for contracts, Rust, dapp, fork tests, docs validators | Test suites are documented as separate CI gates. | `docs/development/ci-suites.md` |

## 4. On-Chain Architecture

### 4.1 Vault Family

A Robot Money vault is an individual strategy container with a mandate,
accepted asset, receipt token, caps, fees, risk label, and status. Each
vault is independently observable and independently pausable. Retiring a
vault stops new deposits while preserving redemption rights — the full
deprecation/retirement lifecycle (registry status, vault shutdown/restore,
and the authority tier that gates each transition) is canonical in §4.7.

The current production-deployed source-backed vault is
`RobotMoneyVault`, an ERC-4626 USDC vault with rmUSDC shares,
OpenZeppelin access control, pause support, reentrancy protection,
caps, an exit fee ceiling, adapter routing, rebalance controls, and
emergency shutdown. It is a direct non-proxy deployment on Base.

The source tree also contains the basket-vault family — an abstract
`BasketVault` base with hybrid NAV model: Uniswap V3 TWAP pricing for
protocol-level guards (NAV growth limit, ORA-4 deviation, TVL cap), while
user share value = market price of underlying assets. See
`docs/technical/asset-valuation-hybrid.md` for the hybrid design rationale.
Concrete `ProtocolAssetVault` (wETH/cbBTC/wSOL exposure) and
`AgentTokenVault` (admin-curated agent-economy tokens) subclasses.
Router eligibility for any vault — basket-vault or otherwise — is
registry state expressed by `VaultRegistry.isRouterEligible(vault)`,
set by ADMIN_ROLE via `setRouterEligible`. Basket vaults stay
ineligible by default; ADMIN_ROLE flips the flag once the subclass
certifies pool cardinality, per-asset TWAP windows, and an intra-vault
rebalancing model (`docs/development/open-questions.md` §3.15). The
same contracts ship into test, demo, and mainnet — only the registry
flag's value differs across environments. See
`docs/development/single-production-codebase.md` for the principle.

The source tree also contains `RwaVault`, a shipped ERC-4626 USDC vault
for tokenised real-world assets. The current deployment holds deSPXA
(Centrifuge / Janus Henderson / Anemoy tokenised S&P 500 on Base).
Key architectural characteristics:

- **Entry and exit via Aerodrome secondary market only.** NAV (TWAP) prices
  protocol-level guards (growth limit, ORA-4 deviation, TVL cap); user share
  value = market price of held deSPXA tokens. See
  `docs/technical/asset-valuation-hybrid.md` for the hybrid design rationale.
  Primary NAV redemption through the Centrifuge V3 ERC-7540 epoch operator
  is a permanent non-goal: a permissionless smart-contract vault cannot
  satisfy the KYC requirement. All deposits and withdrawals swap
  USDC↔deSPXA through an Aerodrome CL pool.
- **Chronicle push oracle for NAV pricing.** Aerodrome DEX TWAP is
  unsuitable for thin RWA liquidity. `ChronicleOracleAdapter` prices NAV
  and slippage floors via a Chronicle on-chain signed price feed.
  `RwaVault` enforces a heartbeat window (default 24 h); price-sensitive
  operations revert with `StalePriceFeed` if the feed is stale. NAV used
  for system guards only; user position value via separate market pricing.
- **Issuer freeze-control risk.** The deSPXA issuer may freeze token
  transfers at any time, blocking all Aerodrome swaps and therefore all
  vault deposits and withdrawals. Existing rmRWA holders retain their
  shares; no funds are confiscated. Admin should pause the vault when a
  freeze is detected to surface user-facing messages instead of opaque
  ERC-20 reverts. See `docs/adr/ADR-0006-despxa-rwa-vault-design.md` §4.
- **Single-asset basket.** `RwaVault` holds exactly one basket asset
  (deSPXA). `maxAssets()` returns 1 to enforce this constraint.
- **Router eligibility.** `RwaVault` is a `BasketVault` subclass.
  Router eligibility follows the same `VaultRegistry.isRouterEligible`
  registry-flag model as other basket vaults. The flag is flipped by
  ADMIN_ROLE once pool cardinality, oracle freshness, and the intra-vault
  rebalancing model are certified. `RwaVault` is marked Active — real
  asset, seeded, Router-eligible per `docs/prd.md` §11.4.

#### Target architecture (ADR-0010, Proposed)

[ADR-0010](adr/ADR-0010-unified-vault-architecture.md) (status:
Proposed) unifies the two vault families described above into a single
`Vault` contract composed with an `IPositionAdapter` interface — the
`RobotMoneyVault` adapter architecture taken as the general case. Under
that model the abstract `BasketVault` base and its
`ProtocolAssetVault`/`AgentTokenVault`/`RwaVault` subclasses stop being
contract subclasses: each basket asset becomes a per-asset
`AssetPositionAdapter` that custodies the token, executes swaps through
the existing `IBasketSwapAdapter` venue seam, and self-prices via TWAP
or Chronicle. Themes (stable-yield, protocol-asset, agent-token, RWA)
become deployments plus configuration, not subclasses. Migration
follows [ADR-0009](adr/ADR-0009-vault-retirement-no-assisted-migration.md):
deploy v2, shift router weights, retire v1 — the v1 contracts described
in this section stay untouched, so the current-state text above remains
accurate for the deployed contracts. Once v2 ships, the description of
a distinct basket-vault subclass family (and per-subclass certification
framing) becomes historical; the registry-eligibility model
(`isRouterEligible`), the lifecycle in §4.7, and the
single-production-codebase principle carry over unchanged. Canonical
spec: `docs/technical/unified-vault-spec.md`.

### 4.2 Portfolio Router

The Portfolio Router is the outer allocation contract. It accepts USDC
deposits and splits them across active underlying Robot Money vaults by
the current RM-governed router weights.

Router requirements:

- destinations are vaults, not adapters or raw DeFi venues;
- deposits expose a preview with destination vaults, weights, estimated
  receipts, fees, and unavailable legs;
- a deposit with any unavailable leg reverts in full; the preview
  surfaces unavailable legs before signing so the user can decide
  whether to proceed or wait;
- receipt tokens remain visible as underlying vault receipts;
- router caps and vault caps both apply;
- router state, weights, governance execution, and history are publicly
  observable.

The source tree contains `contracts/PortfolioRouter.sol`, a dedicated
router contract that backs the requirements above. It integrates with
`VaultRegistry` for eligibility — both lifecycle status
(Active/Paused/Retired) and the registry-backed router-eligibility
flag (`VaultRegistry.isRouterEligible(vault)`) that expresses
production-readiness as state set by ADMIN_ROLE. The router applies
per-vault withdrawal caps over a fixed window and depends on
`RouterGovernance` for weight execution. The single registry flag
replaces the previous in-contract `isPrototype()` /
`prototypeOverride` / `nonPrototypeAttested` machinery (issue #475) so
the same contracts ship into every environment with no per-environment
code variant. The router is not yet on the production mainnet
deployment manifest; the contract surface is in place, audit and
mainnet onboarding remain planned work on the Plan tracking issue (#109).

### 4.3 Vault Adapters

Adapters are internal to one vault. They normalize venue-specific
deposit, withdrawal, and valuation behavior behind `IStrategyAdapter`:

- `deploy(uint256 amount)`;
- `withdraw(uint256 amount) returns (uint256 actual)`;
- `totalAssets() returns (uint256)`;
- `sweepForeignToken(address token)`.

The value-moving functions (`deploy`, `withdraw`) are callable only by the
owning vault. Adapter selection and caps are privileged vault-management
operations and expand the audit surface of that vault.

`sweepForeignToken` is the permissionless foreign-token quarantine sweep
(custody invariants INV-1/INV-2, see §6 and `docs/prd.md` §12): anyone may
call it, but it moves only NON-protected tokens to a single hardcoded
quarantine address — never a caller-supplied recipient. There is no
arbitrary-recipient `rescueTokens(token,to)` (deleted, INV-1).

Current stable-yield adapters (for `RobotMoneyVault`):

- `MorphoAdapter` deposits USDC into the Morpho Gauntlet USDC Prime
  ERC-4626 vault.
- `AaveV3Adapter` supplies USDC to Aave V3 on Base and holds aToken
  exposure.
- `CompoundV3Adapter` supplies USDC to Compound V3 Comet on Base and
  forwards withdrawn USDC back to the vault.

Current basket-vault swap adapters (implement `IBasketSwapAdapter` for
`BasketVault` subclasses including `RwaVault`, `ProtocolAssetVault`, and
`AgentTokenVault`):

- `AerodromeSwapAdapter` routes USDC↔asset swaps through the Aerodrome
  Finance Router on Base (concentrated-liquidity CL pools). NAV and
  slippage floors are priced via an Aerodrome CL pool TWAP (arithmetic-mean
  tick over a configurable window, using the same `observe()` ABI as
  Uniswap V3). Only Aerodrome CL pools are supported; classic stable/volatile
  pools do not expose `observe()`.
- `UniswapV4SwapAdapter` routes USDC↔asset swaps through the Uniswap V4
  Router (`exactInputSingle`) on Base. TWAP reads use the V4 pool's
  `observe()` method (EIP-7680 compatible, identical ABI to V3). Tick
  spacing is derived from the fee tier using Uniswap V4's standard mapping
  (500→10, 3000→60, 10000→200). Pools with hooks or custom tick spacings
  require a bespoke adapter.
- `ChronicleOracleAdapter` routes USDC↔asset swaps through the Aerodrome
  Router (same swap path as `AerodromeSwapAdapter`) but prices NAV via a
  Chronicle on-chain push oracle instead of a DEX TWAP. Used by `RwaVault`
  for deSPXA where DEX liquidity is insufficient for a manipulation-resistant
  TWAP. Staleness enforcement (heartbeat check) is delegated to the owning
  vault, keeping the adapter stateless.

#### Target architecture (ADR-0010, Proposed)

Under [ADR-0010](adr/ADR-0010-unified-vault-architecture.md) (Proposed),
the adapter seam described above becomes the general case for every
vault: a single `Vault` contract routes through `IPositionAdapter`
implementations, collapsing the current split between stable-yield
`IStrategyAdapter`s (vault-internal lending positions) and basket-vault
`IBasketSwapAdapter`s (vault-held tokens priced by the vault). Key
deltas from the current state:

- Basket assets are held by per-asset `AssetPositionAdapter` contracts
  that custody the token, execute swaps via the existing
  `IBasketSwapAdapter` venue seam (Aerodrome, Uniswap V4,
  Chronicle-priced Aerodrome), and self-price via TWAP or Chronicle —
  custody and pricing move out of the vault and into the adapter.
- The vault mints on realized NAV delta for **protocol-level guards only**
  (NAV growth limit, ORA-4 deviation, TVL cap, withdrawal liveness). User
  share VALUE = market price of underlying assets held; NAV is system
  accounting. See `docs/technical/asset-valuation-hybrid.md` for the hybrid
  design rationale.
  The vault's realized NAV delta degenerates to the exact-amount accounting
  the lending adapters already exhibit (at protocol level, not user level).
- `withdraw()` is permitted iff every active adapter reports
  `isExact()`; otherwise the vault is redeem-only.
- Governance controls apply uniformly per adapter: allowlist plus
  codehash pinning, `capBps`, ADP-2 NAV exclusion, rebalance throttles,
  and registry retire/unretire.

`IBasketSwapAdapter` (ADR-0005) survives as the venue seam inside
`AssetPositionAdapter`. The adapter descriptions above remain accurate
for the deployed v1 contracts (which stay untouched per ADR-0009); the
statement that swap adapters serve "`BasketVault` subclasses" becomes
historical once v2 ships. Canonical spec:
`docs/technical/unified-vault-spec.md`.

### 4.4 Synchronous Redemption

Synchronous redemption is a product promise. A vault included in router
allocations must support one-transaction withdrawal or be excluded until
the product promise changes. Adapter liquidity failures, upstream venue
pauses, and withdrawal shortfalls are therefore first-order risks, not
background implementation details.

### 4.5 Protocol Admin Authority

All five protocol contracts (`RobotMoneyVault`, `RobotMoneyGateway`,
`VaultRegistry`, `PortfolioRouter`, `RouterGovernance`) use OpenZeppelin
`AccessControl` with an `ADMIN_ROLE` that governs privileged operations:
adapter add/remove, cap and fee changes, vault registration and
deregistration, pause-role grants, governance parameter changes, and
`ADMIN_ROLE` membership changes.

On-chain enforcement requirement: `ADMIN_ROLE` on all five contracts
must be held by a deployed `TimelockController`. The existing Safe
multisig (`0x88bA…75A0`) holds `PROPOSER_ROLE` and CANCELLER_ROLE on
the controller. EXECUTOR_ROLE should be open (`address(0)`) so any
address can execute an already-authorized operation after the delay; if
execution is restricted, the executor must also be a Safe with threshold
≥ 2 and the liveness tradeoff must be documented. No EOA may hold
`ADMIN_ROLE` directly in production. All high-risk admin operations must
pass through the schedule → delay → execute flow. The minimum delay is
configurable per operation class.

The `TimelockController` address, proposer set, executor policy, min
delay, canceller set, and pending operation hashes must be observable
on-chain and surfaced by `rmpc get-timelock` and the dapp timelocked
proposals panel.

This constraint does not apply to depositor-owned agent policies, which
remain under sole depositor authority. Router-weight votes and post-vote
weight execution use the `RouterGovernance` module's own voting period
and execution delay. RouterGovernance administration, including voting
power assignment and cadence/quorum parameter changes, remains a
protocol-admin operation and must route through the admin timelock in
production.

See `docs/technical/security-model.md` §4 and issue #414.

### 4.6 Fees, Revenue, and Buybacks

The PRD defines three fee classes per vault or Portfolio Router path:
management fee, swap-fee share, and exit fee. The current deployed
`RobotMoneyVault` source implements an exit fee only.

**Current phase:** only exit fees are in scope. Management fee,
swap-fee-share, protocol revenue collection, and buyback-and-burn are
deferred to a future phase and require explicit contract design before
implementation.

Architecture requirements for exit fees (current phase):

- exit fee bounds are explicit per vault or router path before a user
  or agent signs;
- previews show gross amount, fee amount, and net amount;
- fee recipient changes are protocol-admin operations and observable.

Architecture requirements for deferred fee surfaces (future phase):

- management-fee and swap-fee-share mechanisms require dedicated
  contract design and a separate ADR before implementation;
- protocol revenue and buyback-and-burn execution must have observable
  on-chain events and indexed history when implemented.

### 4.7 Vault Deprecation/Retirement Lifecycle

Retiring a vault is a deliberate, multi-step, multi-contract sequence,
not a single switch. This section is the canonical model that ties
together the two enforcement layers — the `VaultRegistry` lifecycle
status and the `RobotMoneyVault` deposit shutdown — and the authority
tier that gates each transition. The mechanism formerly documented per
contract ("two separate emergency switches") is superseded by this one
source of truth. Depositor redemption rights are never revoked at any
stage: every Robot Money vault is ERC-4626, so `redeem` stays callable
by share holders throughout. There is no on-chain assisted or forced
migration — see
[ADR-0009](adr/ADR-0009-vault-retirement-no-assisted-migration.md).

**Lifecycle states and transitions.** The lifecycle runs
`Active → Retired (draining) → empty → deregistered`, with an
independent emergency `shut-down` overlay and a `Paused` halt. The
states map to real code: the registry lifecycle states are the
`VaultRegistry.VaultStatus` enum values (`Active`, `Paused`, `Retired`,
in `contracts/VaultRegistry.sol`); the `shut-down` overlay is the
`RobotMoneyVault.shutdown` flag.

| State / transition | Layer | Mechanism (at HEAD) | Trigger role | Effect |
|---|---|---|---|---|
| **Active** | registry | `VaultStatus.Active` | — | Router routes new deposits (if also router-eligible); direct deposits open. |
| **Paused** | registry / vault | `VaultStatus.Paused`; vault `pause()` | `setVaultStatus`: governance · `pause()`: emergency (hot key) | Reversible halt. Router stops routing; vault `pause()` halts deposits and withdrawals. `unpause()` is governance. |
| **Active → Retired** (unified) | registry + vault | `VaultRegistry.retire(vault)` | governance (`ADMIN_ROLE` = timelock) | Atomic in one call: sets registry status `Retired` **and** halts **direct** vault deposits (`IRetirableVault.retire()`, sets the vault `retired` flag → `VaultRetired()`). Withdraw-only thereafter; existing depositors keep unconditional `redeem`. Emits `VaultStatusChanged` + `Retired`. The two enforcement layers can no longer drift. **Precondition (#1173):** reverts `RetireWhileRouterEligible` if the vault is still router-eligible — drop it from `routerEligibleCount` first (see the retire strand invariant below). |
| **shut-down** (overlay) | vault | `shutdownVault()` (sets `shutdown = true`, zeroes `tvlCap`) | emergency (`EMERGENCY_ROLE`, hot key) | Hard-stops **direct** vault deposits (`VaultShutdown()`); withdrawals continue. Vault-level only — makes no lifecycle/registry decision. Emits `Shutdown`. |
| **shut-down → reopened** | vault | `restoreVault(newTvlCap)` | governance (`ADMIN_ROLE`) | Clears `shutdown`, sets a fresh `tvlCap`, re-opens deposits. Emits `VaultRestored`. Deliberately asymmetric with the fast emergency shutdown. |
| **Retired → Active** (abort) | registry + vault | `VaultRegistry.reactivate(vault)` | governance (`ADMIN_ROLE` = timelock) | Atomic abort: sets registry status `Active` **and** re-opens direct vault deposits (`IRetirableVault.unretire()`). The vault returns to normal routing. Emits `VaultStatusChanged` + `Unretired`. |
| **Retired → empty** | — | depositors `redeem` | depositor only | Holders drain at their own pace; no protocol action moves their funds (ADR-0009). |
| **empty → deregistered** | registry | eventual removal from the registry vault set | governance (`ADMIN_ROLE`) | Conceptual terminal state once TVL has fully drained. No deregistration function exists at HEAD; this is the planned end of the lifecycle, not a shipped mechanism. |

**How the two layers relate.** Two enforcement layers exist: the
registry `Retired`/`Paused` status (stops the **router** from sending new
deposits) and the vault deposit-halt (stops **direct** deposits on the
vault contract itself). `setVaultStatus(vault, …)` now drives **both**
layers in one call (LIFE-1, #968): any non-`Active` status (`Retired` or
`Paused`) calls the vault's `IRetirableVault.retire()` deposit-halt leg
and `Active` calls `unretire()`, so the registry status and the vault flag
can never drift on this path either — closing the former back-door where
the bare `setVaultStatus(vault, Retired)` flipped only the router layer.
The vault calls are guarded (empty-code skip + try/catch) so a registered
address that does not implement the leg, or is not linked to this
registry, does not brick the status change; a vault linked to this
registry always stays in sync. The remaining drift case is the emergency
`shutdownVault`, which deliberately flips only the vault layer while the
registry still reads `Active` (it is an emergency overlay, not a lifecycle
decision). The unified `VaultRegistry.retire(vault)` action (below)
remains the atomic governance lifecycle decision that flips both layers in
one timelock-gated call.

**Unified retire (per decision #925, graduated-authority model).** The
graduated-authority decision closes the "`Retired` in the registry but
still directly depositable" gap with a single deliberate governance
`retire` action that sets registry `Retired` **and** halts vault
deposits in one timelock-gated call, so the two layers cannot drift.
This is implemented as `VaultRegistry.retire(vault)` (gated to
`ADMIN_ROLE`, held by the `TimelockController` in production): it flips
`_status[vault]` to `Retired` and, in the same transaction, calls the
vault's deposit-halt leg `IRetirableVault.retire()`, which sets the
vault's `retired` flag (distinct from the emergency `shutdown` flag so
the two paths never alias). The vault gates `retire()`/`unretire()` to
its linked registry only (set once via `setRegistry`), so the registry's
authority over the vault is narrow (deposit-halt only, not full
`ADMIN_ROLE`). The abort path is `VaultRegistry.reactivate(vault)`
(status → `Active` + `IRetirableVault.unretire()`). Emergency
`shutdownVault` stays `EMERGENCY_ROLE`, vault-only, with no
registry/lifecycle change — the hot key never makes a lifecycle decision.
A direct hot-key `ADMIN_ROLE` EOA call to `retire()` reverts; it is
reachable only via the timelock schedule → delay → execute path
(`contracts/test/DeployTimelock.t.sol`).

**Retire strand invariant (make-ineligible-before-retire, #1173).** A
`Retired` vault must never remain counted in `routerEligibleCount`. The
`PortfolioRouter` default weight vector must span exactly
`routerEligibleCount` legs, all Active and router-eligible (ADR-0002); a
still-eligible vault flipped to `Retired` can never again be an Active leg,
so no valid default vector could be set — the default vector would be
*stranded*. Both doors to the `Retired` state therefore enforce the ordering
in-contract rather than by convention: `VaultRegistry.retire(vault)` **and**
`setVaultStatus(vault, Retired)` revert `RetireWhileRouterEligible(vault)`
while `isRouterEligible(vault)` is true. Governance must first drop the vault
from `routerEligibleCount` — `setRouterEligible(vault, false)`, or atomically
with a re-set default vector via `migrateEligibility(vault, false, …)` — then
retire. (`Paused` is transient and reversible via `unpause`, so it is not
gated; only the terminal `Retired` transition is.)

**Authority tier.** Transitions follow the graduated-authority model
(see §4.5 and the authority tier below): permissionless actions need no
role; reversible halts and asset de-risking sit on the `EMERGENCY_ROLE`
hot key; every deliberate value-/lifecycle-changing action — including
`restoreVault`, `retire`/`reactivate`, and adapter management — is gated
behind the governance multisig + `TimelockController`; and moving
depositor principal is the depositor's own signed action alone.

| Action | Tier |
|---|---|
| sweep foreign token, trigger harvest | permissionless |
| `pause`, `shutdownVault`, `emergencyWithdraw` (→ vault only), `forceRemoveAdapter` | emergency (hot key, `EMERGENCY_ROLE`) |
| `unpause`, `restoreVault`, `retire`, `reactivate`, `setFeeRecipient`, adapter add/allowlist/caps, quarantine set + recover | governance (multisig + timelock) |
| `redeem` / move depositor principal | depositor only |

**No assisted migration.** At no point does the protocol move a
depositor's position on her behalf. A retired vault is a *redeemable
archive*: holders exit via standard `redeem`, and any "migration" to a
successor vault is a user-initiated, user-signed redeem-then-deposit at
the dapp/app layer. This is the fixed policy of
[ADR-0009](adr/ADR-0009-vault-retirement-no-assisted-migration.md); no
line of this lifecycle authorises forced or admin-driven migration.

### 4.8 Investment Committee Policy Contract

The IC policy contract is a **signalling-only registry** — a sixth
protocol contract alongside the five in §4.5 — separate from
`RouterGovernance` (see the boundary in §2.4).

**State.** Metadata only — no USDC, share tokens, or basket assets. Two
records: an allowlist of registered committee agents (address → org name,
registration block, active flag), and each agent's latest **vote
commitment** — `rationale_uri`, a `vote_digest` (the keccak256 of the
canonical vote JSON), `prompt_hash`, `inputs_digest`, `timestamp`, and
`schema_version`. The structured tilt itself — per-vault stance,
target-weight bps, and confidence — lives off-chain in the hash-committed
memo at `rationale_uri`, not in contract storage (the minimal-on-chain
split, §7.4). A new commitment supersedes the agent's prior one; the prior
commitment survives in event history, so the registry is the live state
and the event log is the immutable track record.

**Signalling-only enforcement (INV-4).** The contract must not implement a
payable `receive`/`fallback`, must never call a vault or
`PortfolioRouter.setWeights`, and must never grant any agent a
treasury-moving role on another contract. Its only outputs are storage
writes and events. This is the on-chain expression of `docs/prd.md` §12
INV-4 and is mandatory (§8).

**Roles and authority.** Follow the conventions the five protocol
contracts already use: OpenZeppelin `AccessControl` with a self-administered
`ADMIN_ROLE`, the last-admin floor (`AdminFloorAccessControl`, so the sole
admin cannot be removed), and role separation (an address that allowlists
agents must not also be a voting agent). `ADMIN_ROLE` is held by the
`TimelockController` in production (§4.5) — agent allowlisting and any IC
parameter change route through schedule → delay → execute. Casting a vote
is a registered agent's own action and is not timelocked.

**Identity model.** A committee agent's identity is its registered EOA: it
submits its own vote through the gateway, so `msg.sender` is the
authorization, the same identity model the gateway uses for agent actions
(§5.2). On-chain signature recovery and EIP-712 structured-vote domains are
not used — the codebase has no EIP-712 path, and `msg.sender`-via-gateway
is sufficient when the agent submits its own vote. (Structured-data
signatures would only be needed to authenticate a vote authored by one
party and relayed by another, which the committee does not do.)

**Events for observability.** Emit `AgentRegistered`, `VoteSubmitted`
(indexed by agent, carrying `vote_digest`, `rationale_uri`,
`schema_version`, and timestamp), and `AgentRevoked`, mirroring
`VaultRegistry`'s event conventions (§4.1). The explorer indexer
enumerates registered agents and vote commitments from these events, then
fetches and hash-verifies each memo to surface per-vault tilts and
per-agent track record (§5.4) — never reading `rmpc` output.

**Governance linkage.** Committee output feeds RouterGovernance as an
upstream signal only, with **no on-chain coupling**: the IC contract never
calls and is never called by RouterGovernance, which stays the sole
`setWeights` caller (§2.4). An admin (via timelock) reads the committee's
tilts — aggregated off-chain over the hash-verified memos — and proposes
router weights through RouterGovernance's existing path, informed by but
not driven by committee output. Because the link is off-chain and
admin-applied, RouterGovernance is unchanged and no governance-interface
refactor is required.

### 4.9 Consensus Recommendation Receipt Contract

`contracts/gateway/ConsensusRecommendationReceipt.sol` is a **seventh protocol
contract** and a **signalling-only commitment register** for the swarm's
consensus recommendation receipts. It anchors the `keccak256` of a receipt's
canonical bytes on chain, next to the public URI serving those exact bytes.

**Why the anchor exists.** A signed receipt published only at an
RM-controlled URL can still be silently suppressed by RM, which is exactly
the property the record exists to provide (product proposal §2.1). The
on-chain commitment is the trust story, not an enhancement to it.
**That property lands at mainnet deployment, not here** — v0.1 proves the
mechanism on a devnet, and no surface may describe the record as
tamper-proof or censorship-resistant in the present tense before then.

**Payload contract.** The bytes are pinned by
`tests/fixtures/consensus-receipt.schema.json` and
`consensus-receipt.canonicalization.json`, which are **byte-identical** to
`contract/src/__fixtures__/` in `robotmoney-frontend`; that byte identity
is the cross-repo pin. The preimage is UTF-8
`robotmoney:consensus-receipt:v1\n` + compact JSON + a trailing newline.
`tests/fixtures/consensus-receipt.anchor-digest.json` is a **core-only
sidecar** — deliberately outside the shared set — recording the `keccak256`
of each committed golden. `ConsensusRecommendationReceiptTest` **derives** those
digests by hashing the golden files and compares them with the sidecar
constants, so changing either side alone turns the test red.

#### 4.9.1 The three interface answers (issue #1247 task 4.0)

The earlier design in the product proposal §2.1 assumed multiple analysts
each calling `consensusSubmitSignature` under their own EOA. That model was
rejected; its entrypoint set and its 7-day deadline were **re-derived, not
ported**. The three answers are recorded here before any contract code:

**1. `consensusSubmitSignature` does not survive. A one-shot
`recordReceipt` replaces it.** One submitter attests for the committee and
the analysts' ed25519 signatures ride inside the payload as data verified
off-chain. The EVM has no ed25519 precompile and ADR-0012 §5 closes that
seam, so a per-analyst on-chain signature call could never verify anything
it accepted. There is no second agent registry either: submitters are gated
by `COMMITTEE_AGENT_ROLE` on the shipped `InvestmentCommitteePolicy`, so
role administration stays on one contract.

*Accepted cost.* The chain proves the committee produced the
recommendation and that one submitter attested to it. It does **not** prove
each named analyst signed. Verifying that is an off-chain check against the
payload, and every surface must say so plainly — the dapp renders the
payload signature count labelled as **off-chain analyst signatures**, never
as on-chain approvals. Because a compromised submitter could otherwise
publish a receipt the analysts never agreed to, that verification is
load-bearing: `rmpc` refuses to submit a receipt whose digest or whose
embedded signatures do not verify, and the indexer stores the verification
state it independently recomputed.

**2. The 7-day window collapses to zero. Staleness becomes a property of
the recommendation, computed off-chain.** The `deadline = firstSignatureAt +
WINDOW_SECONDS` design bounded a **multi-party signature-collection
window**. With one submitter there is nothing to wait for, so the window is
deleted rather than repurposed: the contract has no deadline field, no
expiry, and no keeper. Staleness is derived from the payload's `created_at`
by the reader — the dapp labels a recommendation stale, and the worker
never drafts governance from a stale one. No timeout changes on-chain
state; `testUnreleasedReceiptNeverExpires` asserts this.

**3. A receipt that is never released stays an immutable public record.**
Recording is the point; release is a separate, discretionary human act.
An unreleased receipt is never deleted, never expires, and never mutates.
The **dapp** renders it as *recorded, not released*, alongside its
verification state and its applied/not-applied state, so a recommendation
that governance declined is visible as such rather than absent. The
**worker** treats it as non-actionable: it watches `ReceiptReleased` only,
and never drafts a `RouterGovernance.propose` from an unreleased or stale
receipt. There is no automatic release threshold (D5) — release follows
human review of the judge's safety opinion and the verified embedded
signatures.

#### 4.9.2 Entrypoint set

| Call | Site | Authority |
|---|---|---|
| `setConsensusReceipt(address)` | `RobotMoneyGateway` | `ADMIN_ROLE` on the gateway |
| `consensusRecordReceipt(bytes32 receiptId, bytes32 payloadDigest, string payloadUri)` | `RobotMoneyGateway` | `AGENT_ROLE` on the gateway **and** `COMMITTEE_AGENT_ROLE` on the IC policy |
| `recordReceipt(address submitter, …)` | `ConsensusRecommendationReceipt` | `onlyGateway` |
| `releaseReceipt(bytes32 receiptId)` | `ConsensusRecommendationReceipt` | `ADMIN_ROLE`, held by the `TimelockController` |

`receiptId` is
`keccak256("robotmoney:consensus-receipt-id:v1\n" + session_id + "\n" + subject_id)`;
refusing an existing id enforces **one receipt per session per subject**.
`payloadUri` must be the stable backend route
`/api/swarm/receipts/{session_id}` on the configured frontend origin.

**Why release is not a gateway entrypoint.** Routing release through the
gateway would require granting the gateway `ADMIN_ROLE` on the receipt
contract — the pattern `committeeRegister` uses. INV-3 requires that role
to be held by the `TimelockController`, and a second holder would defeat
it. The timelock therefore calls `releaseReceipt` directly, and
`ADMIN_ROLE` on the receipt contract has exactly one holder.
`testAdminRoleHeldByTimelock` and
`testReleaseRevertsUnlessRoutedThroughTimelock` assert both halves.

**Signalling-only enforcement (INV-4).** No payable `receive`/`fallback`,
no ERC-20 surface, no call into any vault, `PortfolioRouter`, or
`RouterGovernance`. Recording appends a row; releasing flips a boolean.
`ConsensusRecommendationReceiptTest.testSignallingOnlyBoundary` mirrors
`InvestmentCommitteePolicyTest.testSignallingOnlyBoundary`.

**Events.** `ReceiptRecorded(bytes32 indexed receiptId, address indexed
submitter, uint256 indexed index, bytes32 payloadDigest, string payloadUri,
uint64 recordedAt)` and `ReceiptReleased(bytes32 indexed receiptId, address
indexed releasedBy, uint64 releasedAt)`. **No event carries a signature
parameter, indexed or otherwise** — a `uint8[64] indexed` signature would
exceed the EVM's 3-topic non-anonymous limit, and analyst signatures are
payload data in every case.

**Submitter custody.** The submitter EOA is an operational secret with real
authority over the public record. Its custody, rotation, and compromise
runbook are in
`docs/technical/consensus-receipt-submitter-runbook.md`, which must be
satisfied before any production submission.

## 5. Off-Chain Architecture

### 5.0 Read Surface Taxonomy

All client surfaces — dapp, `rmpc`, and explorer API — expose data in
two scopes. The scope determines what address (if any) is required and
which data source is authoritative.

**Protocol scope** — no address required. Shows the state of the
protocol as a whole: all registered vaults, vault statuses, caps, fees,
risk labels, adapter breakdowns, Portfolio Router weights, governance
proposals, and aggregate metrics (total TVL, number of active vaults).
This is the data a landing page, a public API consumer, or an agent
with no depositor relationship needs to decide whether and where to
deposit. Sources: live chain reads for current vault state and weights;
explorer indexer for historical activity and aggregate metrics.

**Account scope** — an address is required. Shows the state of a
specific depositor or agent address: receipt token balances across all
vaults, USDC value of each position, combined portfolio value, agent
policy details, gateway cap usage, and full transaction history.
Sources: live chain reads for balances, receipt supply, and policy
state; explorer indexer for history and aggregated fee data.

Both scopes are read-only and require no signing. The account scope
requires only an address, not a signature — a watched address is
sufficient. Signing is required only for writes (deposits, withdrawals,
policy management, governance votes).

Safety-critical values used for signing (fee bounds, cap headroom,
policy state, allowances, code hash) must always come from live chain
reads regardless of scope. Explorer data may annotate display but must
not be the source of values presented in a signing prompt.

### 5.1 `rmpc`

`rmpc` is the constrained Rust command client for agents and operators.
Its signing path builds only known calldata for configured contracts on a
configured chain. It performs direct JSON-RPC preflight reads before any
write and emits stable JSON envelopes for read commands:

- `chain_id`;
- `block_number`;
- `source`;
- `partial`;
- `errors`;
- `data`.

Large integer fields are serialized as decimal strings. For
safety-critical flows, JSON-RPC is the source of truth; explorer/indexer
data may be used only as an explicitly labeled non-authoritative source
if a future ADR adds that path.

`rmpc` read commands cover both scopes defined in §5.0:

**Protocol-scope reads** (no address argument required):

- `get-vaults` — vault registry: all registered vaults, their name,
  risk label, mandate, status (active/paused/retired), TVL, caps, exit
  fee, and receipt token address.
- `get-vault <address>` — single vault: all of the above plus adapter
  breakdown (address, balance, cap, active flag) and rebalance state.
- `get-router` — Portfolio Router: active vault addresses, current
  weight bps per vault, pending governance proposal if any, and router
  cap.
- `get-governance` — governance state: active proposal, vote tallies if
  available, cadence, quorum threshold, execution delay, and last
  applied weights.

**Account-scope reads** (address argument required):

- `get-position <address>` — positions across all registered vaults:
  receipt token balance, USDC value, share of vault TVL, and composite
  portfolio total. Suitable for an agent checking its treasury exposure.
- `get-agent <address>` — agent policy: valid-until, max per payment,
  max per window, window usage to date, allowed destinations, share
  receiver, and asset recipient.
- `get-balance <address>` — USDC and receipt token balances for the
  address, plus USDC allowance to each configured contract.

Protocol-scope reads require only the chain and registry configuration;
they do not require a signer key. This allows agent runtimes to run
protocol reads from a read-only deployment without any key material.

**Committee commands.** `rmpc` exposes committee write subcommands that
follow the same write-command path as `deposit`,
`propose`, and `vote` (issue #632): load config → enforce the
production-signer gate (software keystores rejected on Base mainnet;
HSM/KMS required) → build known calldata for the configured IC contract →
sign the EIP-1559 envelope through the `AgentSigner` backend → route the
call through `RobotMoneyGateway` → broadcast → decode the event → emit a
stable JSON envelope.

- `committee register` — one-time on-chain registration of the agent
  identity.
- `committee vote-submit` — submit a signed per-vault tilt (stance,
  `target_weight_bps`, `confidence`, `rationale_uri`, `prompt_hash`,
  `inputs_digest`).

The IC contract address is a new optional config field, present only when
these commands are used (same pattern as the optional governance address);
the commands fail closed if it is absent. Committee-specific failures map
to named `RmpcError` variants and to the stable product reason codes in
§7.2. Committee read commands (e.g. listing an agent's registered votes)
use the same protocol/account read envelopes as §5.0.

**Consensus receipt commands.** `rmpc receipt` handles the consensus
recommendation receipt of §4.9 (issue #1247).

- `receipt verify` — fetch (or read) a receipt, canonicalize it per
  `tests/fixtures/consensus-receipt.canonicalization.json`, print the
  derived `payload_digest` and `receipt_id`, and report per-analyst
  Ed25519 verification. Read-only: no signer, no nonce lock, no RPC.
- `receipt submit` — the same checks, then
  `RobotMoneyGateway.consensusRecordReceipt(receiptId, payloadDigest,
  payloadUri)` sent **to the gateway address** (the receipt contract is
  `onlyGateway`).

The canonicalizer lives in `clients/rust-payment-client/src/consensus_receipt.rs`
and is a second implementation of bytes the frontend already produces, so it
is proved equal to the committed goldens byte for byte —
`consensus-receipt.escaping.canonical.txt` in particular, because an
ASCII-only comparison passes for a serializer that escapes non-ASCII,
U+2028, or the HTML-sensitive characters (Go's `encoding/json` and Python's
`json.dumps` each do one of those by default).

`submit` **refuses to broadcast** when the derived digest disagrees with an
operator-supplied `--expected-digest`, or when any embedded analyst
signature fails — before the signer is loaded, before the nonce lock is
taken and before any RPC call. That refusal is what §4.9.1 names
load-bearing: the chain cannot verify Ed25519, so without it a compromised
submitter could anchor a receipt the analysts never agreed to.

### 5.2 Agent Permissions Gateway

The gateway is the permissions and agent-safety layer for autonomous
access. It is not a vault, not the Portfolio Router, and not an adapter.
It sits between `rmpc`/agent keys and product write surfaces so an agent
can only execute allowed actions under a depositor-owned policy.

The depositor owns the policy: valid-until, max per payment, max per
window, share receiver, and allowed destinations. The Robot Money team
does not manage individual depositor agent policies at runtime.

The current gateway implementation gates agent deposits into a vault. The
product architecture uses the same safety boundary for agent deposits and
agent withdrawals across single-vault and Portfolio Router paths:

- the agent can call only gateway-approved verbs;
- the agent cannot choose its own share receiver;
- the agent cannot choose its own withdrawal recipient;
- the agent cannot raise caps or expand destinations;
- the agent cannot add vaults, change mandates, alter router weights, or
  bypass disabled vaults;
- the gateway enforces amount, expiry, window usage, destination,
  idempotency, pause, receiver, and recipient constraints on-chain;
- the client must read registry, vault status, router weights, policy,
  allowance, balance, and projected cap usage before signing.

For deposits, the gateway pulls USDC from the agent, enforces policy, and
forwards the allowed deposit to a vault or the Portfolio Router. The
resulting vault receipts are minted to the policy-configured share
receiver.

For withdrawals, the gateway is the only agent-callable redemption
spender. The depositor or configured receipt owner grants the gateway the
needed vault-receipt allowance, or uses an owner contract that exposes
the same policy boundary. The agent submits a gateway withdrawal request;
the gateway verifies policy, cap usage, allowed source vault/router path,
receipt allowance, receipt balance, previewed assets out, pause state,
and recipient, then calls the vault or Portfolio Router redemption path.
Withdrawn USDC is sent only to the policy-configured asset recipient.
The agent cannot redirect proceeds to itself.

Because the Portfolio Router does not issue an outer share token,
router-position withdrawals resolve to underlying vault receipts. A
router withdrawal helper may orchestrate proportional underlying
redemptions, but it must preserve the same gateway permission checks and
must not create hidden custody or an unobservable outer claim.

### 5.3 Human Dapp

The dapp is the human command and observability surface. It covers both
scopes defined in §5.0 and is organized into three view layers.

**Protocol layer (no wallet required)**

The protocol layer is the first contact for any visitor. It must be
fully functional without a connected wallet and must load from the
explorer API plus live chain reads for vault state. It contains:

- Vault registry view: all registered vaults listed with name, risk
  label, TVL, current APY estimate, exit fee, deposit cap headroom, and
  status (active/paused/retired). The list is derived from the on-chain
  vault registry so new vaults appear automatically.
- Vault detail view: single-vault breakdown — adapter allocations and
  their individual TVL, rebalance state, fee schedule, caps, receipt
  token address, and historical TVL and activity charts from the
  explorer. A Composition section answers "if I deposit here, what do I
  get back?": it shows the receipt token plus the underlying basket
  assets — live `shortlist()` entries for VOLATILE and active-SPECULATIVE
  basket vaults, and static labels for STABLE_YIELD and inactive
  SPECULATIVE vaults.
- Portfolio Router view: active vaults, current target weights, pending
  governance proposal (if any), and historical weight changes.
- Protocol stats: total TVL across all active vaults, number of unique
  depositor addresses (indexed), and a recent activity feed of deposits
  and withdrawals across all vaults.

**Account layer (wallet connected or watched address)**

The account layer shows the state of a specific address. It activates
on wallet connection but must also be accessible by entering any address
for read-only portfolio inspection (watched address mode).

- Portfolio position: receipt token balances across all registered
  vaults, USDC value of each position using live vault share price, and
  composite portfolio total. Positions from direct vault deposits and
  Portfolio Router deposits are both shown, broken down by vault.
- Transaction history: chronological list of deposits, withdrawals, fee
  events, and governance votes for the address, sourced from the
  explorer indexer.
- Agent policies: all active agent policies the address owns — each
  showing allowed destinations, max per payment, max per window, window
  usage, share receiver, asset recipient, and expiry.

**Action layer (wallet required for signing)**

Actions are available only with a connected wallet. Every action must
render a preview before invoking the wallet.

- Deposit: vault selection or Portfolio Router path, amount entry,
  preview (destination weights, estimated receipts, fees, net amount,
  unavailable legs), and sign.
- Withdrawal: position selection, amount or share entry, preview
  (source vault or router path, estimated USDC, fee, net amount), and
  sign.
- Agent policy management: authorize a new agent, update or revoke an
  existing policy, and export the resulting `rmpc` config file.
- Governance: review active weight proposal, cast vote, and view
  execution state.

Credential boundary:

- the dapp registers agent public addresses and policy settings;
- it does not persist production private keys;
- browser-generated software credentials are fork/devnet-only,
  feature-gated, immediately exported, clearly labeled unsafe for
  production, and rejected by `rmpc` for Base mainnet write commands.

Every admin or policy signing prompt must decode target, function,
arguments, role/policy effect, and risk class before invoking the
wallet.

Signing prompts for deposits and withdrawals must also show the concrete
product effects: destination or source vaults, router weights when
applicable, gross amount, fees, net amount, receipt owner, recipient,
slippage/quote bounds where relevant, and whether execution is
all-or-revert or an explicitly previewed partial fill.

**Committee and regime-feed surfaces**

Two surfaces sit alongside the dashboard, built with the same three-layer
model and the same data sources (explorer API for indexed history, live
chain reads for current state):

- **Regime feed** — a protocol-layer, read-only surface rendering the
  canonical shared market read that committee agents consume. No wallet,
  no action layer.
- **Committee** — spans all three layers. The protocol layer renders
  registered agents, their per-vault tilts, aggregated committee tilt per
  vault, and per-agent track record (sourced from the explorer index of IC
  events, §5.4). The account layer shows a connected committee agent its
  own vote history. The action layer is the vote-submission flow
  (select vault → stance/weight → `rationale_uri` → preview → sign),
  routed through the gateway like every other write. Committee votes are
  signalling-only: the preview must make clear the vote records a tilt and
  does not move funds or set weights.

**Faucet UX (testnet/devnet only)**

A testnet/devnet-only Faucet tab lets operators provision fresh accounts
end-to-end without backend cheats. It drips canonical USDC, RM governance
tokens, and native Base ETH for gas. Each drip is a real signed transfer
from the smoke-test harness holder EOA — the same EOA that receives the
USDC, RM initial supply, and 1000 ETH at genesis — broadcast through the
user's injected EIP-1193 provider. No anvil cheats, no impersonation.
The tab is hidden on mainnet (chain-ID classifier) and additionally
fails closed when the build-time harness key is absent. The deployed
RmToken address is threaded into the dapp build via
`VITE_RM_TOKEN_ADDRESS` at every smoke-test env-injection site (issue
#466) so the RM balance read and RM drip point at the real contract
instead of the compose `0x0` fallback. After a single faucet flow
(Get Base ETH → Get RM tokens), a fresh account can immediately submit
a governance vote.

### 5.4 Explorer Indexer and API

The explorer stack exists for public history, dashboards, and display. It
does not authorize actions and does not replace live `rmpc` preflight.

The explorer API exposes both scopes defined in §5.0. It is the primary
data source for the dapp protocol layer and account history, and for
integrators who need activity feeds without running their own indexer.

**Protocol-scope endpoints** (no address parameter):

- Vault list: all registered vaults with current indexed TVL, status,
  fee, and receipt token. Updates on every indexer tick.
- Vault detail: single vault with adapter allocation history, TVL over
  time, deposit and withdrawal event log, and fee collection history.
- Router state: current weights, weight change history, and governance
  proposal log.
- Protocol stats: aggregate TVL across all active vaults, unique
  depositor count, total deposits and withdrawals by volume and count,
  and a global activity feed of recent events across all vaults.

**Account-scope endpoints** (address parameter required):

- Account positions: receipt token balances and USDC values per vault
  for a given address, derived from indexed transfer events and current
  share price.
- Account history: chronological event log for the address — deposits,
  withdrawals, fee events, policy changes, and governance votes.
- Account agent policies: all gateway policy states for policies owned
  by the address, including window usage history.

**Committee and regime indexing.** The indexer ingests the IC contract's
`AgentRegistered`/`VoteSubmitted`/`AgentRevoked` events through the same
poll-based JSON-RPC path as every other event (never from `rmpc` output),
into Postgres tables keyed by chain and event identity and subject to the
same reorg-rewrite handling. Because the structured tilt is off-chain
(minimal-on-chain split, §7.4), on each `VoteSubmitted` the indexer fetches
the memo at `rationale_uri`, verifies `keccak256(memo) == vote_digest`, and
stores the per-vault tilts only when the hash matches — a memo that is
missing or fails the digest check is recorded as an unverified commitment,
never as tilt data. The API then exposes protocol-scope committee reads
(registered agents, recent votes across all agents, per-vault tilt
aggregate, per-agent track record) and an account-scope read (the votes
submitted by a given agent address). The regime feed is exposed as a
protocol-scope read (latest snapshot plus history). All committee/regime
reads are display-only and non-authoritative for signing, like the rest of
the explorer surface.

**Consensus receipt indexing (§4.9, issue #1247).** The indexer ingests
`ReceiptRecorded` and `ReceiptReleased` through the same poll-based path,
into `consensus_receipts` keyed by `(chain_id, receipt_id)`. On each
`ReceiptRecorded` it re-fetches the payload at `payload_uri` and stores
whether `keccak256(body) == payload_digest` — the same
commitment-on-chain / verification-off-chain shape the vote path already
uses. A receipt whose payload is unreachable or whose digest disagrees is
stored **unverified, never dropped**: a hole in the record is precisely the
failure the anchor exists to prevent, so an unverifiable receipt must be
visible as unverifiable rather than absent. Only digest verification happens
here; per-analyst Ed25519 verification is `rmpc`'s job at submit time
(§5.1), because the refusal has to bite *before* the transaction is signed.

The reorg cascade covers `consensus_receipts` in two ways, because a receipt
carries two block numbers. Rows above the fork root are deleted by
`block_number` like every other event table; release is additionally
**un-flipped in place** for rows whose `released_block_number` is above the
root, since a release is an in-place mutation of a row that may itself sit
below the root and would otherwise survive a rollback that erased the event
causing it.

Read scopes follow §5.0. Protocol scope: `GET /v1/consensus-receipts` and
`GET /v1/consensus-receipts/:receipt_id`. Account scope:
`GET /v1/accounts/:address/consensus-receipts`, the receipts anchored by
that submitter. Every entry carries its verification state, its released
state, and the digest and URI a reader needs to check the record
independently.

Architecture constraints:

- Postgres is the database for every environment that runs the indexer.
- The indexer polls JSON-RPC; it does not use `eth_subscribe`.
- Indexed rows are keyed by chain and event/state identity.
- Reorg handling rewrites rows at or above the safe head.
- `rmpc` outputs are never ingested by the indexer.
- The API is read-only and scoped to one configured chain.
- Explorer data is non-authoritative for signing. The dapp must
  re-fetch balances, caps, fees, and policy state from live chain
  before presenting any signing prompt, even if the explorer was
  used to populate the preceding display view.

### 5.5 Agent Runtime Integration

OpenCode, OpenClaw, and other agent harnesses invoke `rmpc` as a
process-per-call command. **Robot Money does not support MCP — in this repo or
any other** (org-wide decision, 2026-07; see `docs/technical/mcp-decision.md`
and robotmoney-frontend `docs/decisions.md` D21). There is no MCP surface and
none is planned: agents always shell out to `rmpc`, and the Investment
Committee is reached over its REST API directly. An earlier posture *deferred*
the question with re-evaluation triggers; that door is now closed.

**Committee agent skill.** The published committee-agent
skill/plugin is an extension of `robotmoney-analyst`: it reuses the
analyst's regime/market datasources and adds "form a per-vault tilt → post
the rationale memo to a public link → sign and submit the vote via `rmpc
committee vote-submit`." It is a thin skill over the same `rmpc`
process-per-call boundary — it has no signing authority of its own, and
proprietary allocation methods stay out of the published surface. Like the
analyst skill it fails closed (missing IC config, unregistered agent, or a
`rationale_uri` that is not reachable all abort before any on-chain write).

### 5.6 Mint/Burn Watchdog

`services/watchdog` is the automated circuit-breaker monitor required by
`docs/technical/security-model.md` §9 ("No anomaly detection on mint/burn
rate"). It was shipped in PR #787 (issue #658). It is a standalone Rust
service that closes the security-model gap by watching gateway mint and burn
flow and reacting without a human in the loop.

Its role is rate monitoring and automated containment. On each poll cycle
(default `--poll-interval-secs 12`, also configurable through
`WATCHDOG_POLL_INTERVAL_SECS`) it aggregates rolling per-block and per-hour
mint and burn volume from the explorer indexer database and compares each
total against the TOML-configured global limits
(`global.per_{block,hour}_{mint,burn}_limit_usdc`). Optional
`vault.<address>` entries override those limits per vault. The service reads
the indexed event history; it is not an authoritative signer for normal
operations.

On a threshold breach the watchdog follows `action.mode`: `alert`, `pause`,
or `pause_and_alert`. The alert path dispatches PagerDuty-compatible
structured JSON through `src/alert.rs` to `action.webhook_url`. The pause
path constructs and submits a `gateway.pause()` EIP-155 transaction through
`src/pause.rs`, using `action.gateway_rpc_url`,
`action.gateway_address`, and the funded PAUSER_ROLE key from either
`WATCHDOG_PAUSER_KEY_HEX` (preferred in deployments; it overrides the file)
or the `action.pauser_private_key_hex` literal (local dev). Whichever source
supplies it, the daemon consumes the raw key exactly once at startup, derives
the signing key, and drops the hex — the poll loop signs from that derived
state and no code path re-reads a raw pauser secret from the config. The
pauser is distinct from `ADMIN_ROLE`:
it can pause but cannot unpause, matching the guardian/quorum separation in
security-model.md §9. Unpause still requires `ADMIN_ROLE` through the
timelock.

The configured maximum response-time SLA is
`sla.max_response_secs = 300` (five minutes) from breach detection to
pause/alert dispatch; startup rejects a zero value. The service is exercised
by CI suite-20 (`tests/threshold_breach.rs`, `tests/alert_webhook.rs`) and is
described in `docs/development/ci-suites.md`.

## 6. Data and Trust Boundaries

### 6.1 Authoritative Data

Authoritative sources for safety decisions:

- on-chain contract storage read through JSON-RPC;
- transaction receipts and logs from the configured chain;
- locally configured contract addresses, chain id, and runtime-code
  hashes;
- wallet signatures or configured signer backends.

Non-authoritative sources:

- explorer API responses;
- cached indexer snapshots;
- dapp-rendered summaries;
- agent planner text;
- docs and static config examples.

### 6.2 Custody

Robot Money does not custody user private keys. Vault assets are held by
vaults or adapters. Vault receipts are held by the depositor or the
depositor's configured share receiver. The Portfolio Router does not
custody an outer share position under the current product definition.

### 6.3 Role Separation

Protocol authority is limited to contract upgrade where applicable,
configuration of protocol-level controls, pause, and permanent shutdown.
Depositor-owned agent policies are controlled by the depositor. Agent
keys must not hold admin or pause authority.

## 7. Interface and Execution Contracts

### 7.1 Previews

Every write surface that can move assets must have a preview path before
signature:

- direct vault deposit and withdrawal preview;
- Portfolio Router deposit and withdrawal preview;
- gateway-mediated agent deposit and withdrawal preview;
- governance execution preview for router-weight changes;
- fee and net-out preview for any path with a fee.

Preview data must be derived from live chain reads for safety-critical
fields. Cached explorer data may annotate history or display context, but
it cannot be the source of values used for signing.

### 7.2 Execution Results

Write results must emit and report enough structured data for the dapp,
`rmpc`, explorer, and agent clients to agree on what happened:

- transaction hash, block number, and chain id;
- destination/source vaults and router path;
- gross amount, fees, net amount, receipts minted/burned, and recipient;
- policy id or agent address for gateway-mediated actions;
- whether execution was complete or partial;
- per-leg result for Portfolio Router actions;
- product-level refusal reason when execution did not proceed.

Contract reverts can stay technical at the EVM boundary, but client and
API surfaces must map known failures to stable product reason codes such
as `paused`, `vault_disabled`, `cap_exceeded`, `expired_policy`,
`insufficient_allowance`, `insufficient_balance`, `unavailable_leg`,
`fee_cap_exceeded`, and `slippage_bound_exceeded`.

### 7.2.1 Client Stability Integration Seams

The client-stability integration seams reserved here are now shipped; this
section documents the stable surfaces rather than pending work.

The stable dapp contract is `ProductReasonCode` in
`clients/dapp/src/lib/productReasonCode.ts`. It includes the nine product
codes above plus `unknown_revert` as the only catch-all. The mapping layer
translates contract custom errors, JSON-RPC error data, and preview refusals
into that union, inspecting structured revert data before message text;
provider-specific messages are diagnostic context, not stable API values.

The Rust boundary is `RmpcError` in
`clients/rust-payment-client/src/errors.rs`. Product failures use named
variants whose `Display` prefixes are operator-visible contracts. The
explicit `RmpcError`-to-product-code mapping is in place; RPC transport,
decode, and unknown server errors are kept distinct from known contract
refusals and are never misclassified as such.

Router deposits are exposed through `DepositDestination` in
`clients/rust-payment-client/src/cli.rs`. `rmpc deposit --destination router`
routes the router variant through `depositTo`, while the default remains the
vault-only path. Deadlines on the shared deposit path are computed from the
EVM block timestamp, not wall-clock.

Confirmation policy is enforced through `OperationClass`, `RequiredFinality`,
and `ConfirmationDepthPolicy` in `clients/rust-payment-client/src/config.rs`,
with per-operation-class defaults, TOML integration, `get-tx` enforcement,
and dapp status copy all shipped. These structures coexist with the
multi-endpoint `rpc_urls` failover config in the same module.

Policy-state reads have two distinct authorities. `rmpc get-agent` and the
`get-position` command use live chain reads for signing and treasury
decisions. The Explorer API owner lookup
(`GET /v1/accounts/:address/policies`) is an indexed, historical account view
and remains explicitly non-authoritative. The dapp timelocked-proposals panel
and the RWA-vault issuer freeze-control risk disclosure are shipped on the
dapp surface, isolated from the Rust hot-file lane.

## 7.3 Single Production Codebase

There is one production codebase. The same compiled artifacts deploy
unchanged into every environment — local devnet, fork tests, demo, and
mainnet. Environments differ only by **deployment configuration** and
**seeded application data**. No environment-substituting code variants
are permitted: no test-only subclasses that alter readiness, no
build-time branches, no `if (env == "test")` logic, no spoofed users
or state.

Production-readiness for a vault — whether the Portfolio Router may
weight it and route USDC into it — is expressed as **registry state**:
`VaultRegistry.isRouterEligible(vault)`, flipped by ADMIN_ROLE via
`VaultRegistry.setRouterEligible(vault, eligible)`. There is no
on-vault `isPrototype()` flag, no `prototypeOverride`, no
`nonPrototypeAttested`, and no hardened test subclass; the single
registry flag replaces all of them (issue #475). The same contract
ships into every environment; only the registry flag's value differs.

The `rmpc` rule (one client, no env-specific behavior, never spoof
users) is the same principle applied to the daemon. The detailed
rationale, historical lineage (configuration management, Continuous
Delivery, Twelve-Factor App, immutable infrastructure, mainnet-fork
testing), and the operator checklist live in
`docs/development/single-production-codebase.md`; the environment
modes it governs live in `docs/development/environments.md`.

## 7.4 Committee Vote Contract

A committee vote is a **fixed-shape** record. Its field set — `agent_id`,
`vault`, `stance`, `target_weight_bps`, `confidence`, `rationale_uri`,
`prompt_hash`, `inputs_digest`, `timestamp`, `schema_version` — is defined
by a JSON schema committed to the repo and enforced in CI: a valid fixture
must pass the schema job and a deliberately invalid fixture must fail it.

The split is **minimal-on-chain**. The full structured vote — the per-vault
stance, target-weight bps, confidence, and narrative rationale — lives
off-chain in the memo at `rationale_uri`. On-chain, the IC contract stores
only a commitment: the `rationale_uri`, the `vote_digest`
(keccak256 of the canonical vote JSON), `prompt_hash`, `inputs_digest`,
`timestamp`, and `schema_version` (§4.8). The on-chain digest binds the
off-chain memo, so the memo is tamper-evident — a reader recomputes the
hash and compares — while keeping gas low and the contract host-agnostic
about where the memo lives (§9). The committed schema and the on-chain
commitment stay in lock-step through `vote_digest` and `schema_version`.

Vote submission follows the standard preview → sign → execute path (§7.1):
the preview reads the agent's live registration status before signing.
Committee-specific failures map to the existing stable product reason codes
(§7.2) — no new codes are required.

### 7.5 Consensus Recommendation Receipt Contract

A consensus receipt follows the same preview → verify → sign → execute boundary
as a committee vote, while preserving the extra off-chain signature check that
the EVM cannot perform. `rmpc receipt verify` canonicalizes the receipt and
verifies every embedded analyst Ed25519 signature before reporting the derived
digest and receipt id (§5.1). `rmpc receipt submit` repeats those checks before
loading a signer, taking a nonce lock, or calling
`RobotMoneyGateway.consensusRecordReceipt`; a tampered or unsupported receipt
therefore never reaches the chain.

The receipt is a protocol-scope public record once indexed (§5.4). It exposes
the distinction between recorded, verified, released, and applied states rather
than implying that an on-chain anchor proves every analyst approved it or that a
release moves funds. A released receipt may cause a worker to draft a
`RouterGovernance.propose` item for human review, but it never authorizes an
unattended governance submission. The normal failure surface is explicit:
canonicalization or signature failure refuses submission; an unreachable URI or
digest mismatch remains visible as an unverified indexed record; and an
unreleased or stale receipt is non-actionable.

## 8. Security Constraints

These constraints are mandatory for implementation plans derived from
this architecture:

- Users and agents must call vaults or the Portfolio Router, not
  adapters or raw underlying venues.
- Adapters must restrict mutating functions to their owning vault.
- Custody invariants INV-1/INV-2/INV-3 (see `docs/prd.md` §12) are
  mandatory: no admin/role/vault function may route a protocol or depositor
  asset to a caller-supplied recipient (INV-1, the arbitrary-recipient
  `rescueTokens`/`rescueUsdc` functions are deleted); every protocol or
  depositor asset is redeemable or absorbed into NAV, with non-whitelisted
  foreign tokens getting a permissionless `sweepForeignToken` to a single
  hardcoded quarantine address (INV-2); and the fee recipient, fee
  parameters, and quarantine address change only through the
  `TimelockController` (INV-3).
- The permissionless `sweepForeignToken` must move only NON-protected
  tokens (never USDC, the share token, a basket asset, or a protected
  receipt token) and only to the fixed `ForeignTokenQuarantine.QUARANTINE`
  address — never a caller-supplied recipient.
- Vaults and router legs must enforce caps before accepting deposits.
- Any router leg with slippage, oracle, liquidity, or quote-freshness
  risk must surface bounds before signing.
- Withdrawals and redemptions are synchronous unless a vault is clearly
  labeled out of router eligibility.
- Gateway-mediated withdrawals must verify receipt owner, receipt
  allowance, receipt balance, allowed source, maximum amount, minimum net
  assets out, and policy-configured recipient before signing and before
  execution.
- `rmpc` must verify chain id, configured addresses, code hash, role,
  policy, cap usage, allowance, balance, and fee caps before signing.
- The dapp must decode transaction effects before wallet invocation.
- Explorer data must not be used as the source of truth for signing or
  safety decisions.
- Software-backed credentials are development or low-value fallbacks and
  must be explicit in config and UI.
- `ADMIN_ROLE` on all protocol contracts must be held by the deployed
  `TimelockController` in production; no EOA may hold `ADMIN_ROLE`
  directly. All high-risk admin operations must pass through the
  schedule → delay → execute flow. See §4.5.
- The IC policy contract (§4.8) is signalling-only (custody invariant
  INV-4, `docs/prd.md` §12): it must hold no asset, expose no payable
  path, never call a vault or `PortfolioRouter.setWeights`, and grant no
  agent a treasury-moving role. Committee registration and vote submission
  must route through `RobotMoneyGateway`, not a side channel. Agent
  allowlisting and IC parameter changes are `ADMIN_ROLE` operations and
  must route through the admin timelock in production.

## 9. Vendor Selections

| Vendor / project | Category | Selection status | Source |
| --- | --- | --- | --- |
| Base | Production chain | Current chain for verified deployments and fork tests. | `docs/technical/smart-contracts.md` §2 |
| Circle USDC | Asset | Current accepted treasury asset. | `docs/prd.md` §1 |
| OpenZeppelin | Contract library | Used for ERC-4626, AccessControl, Pausable, and ReentrancyGuard. | `docs/technical/smart-contracts.md` §3.1 |
| Morpho Gauntlet USDC Prime | Stable-yield venue | Current adapter target. | `docs/technical/adapter-architecture.md` §4 |
| Aave V3 | Stable-yield venue | Current adapter target. | `docs/technical/adapter-architecture.md` §4 |
| Compound V3 Comet | Stable-yield venue | Current adapter target. | `docs/technical/adapter-architecture.md` §4 |
| Postgres | Explorer database | Accepted for every environment that runs the indexer. | `docs/technical/explorer-schema-decisions.md` §3.1 |
| JSON-RPC providers | Chain data transport | Required; specific production provider is not selected. | `docs/technical/explorer-schema-decisions.md` §3.5 |
| HSM / Secure Enclave / TPM / KMS | Production signer class | Preferred signer classes; exact vendor not selected. | Plan tracking issue #109 §0 (git history) |
| GitHub Actions | CI/CD | Existing documented CI environment. | `docs/development/ci-suites.md` |
| Committee memo store | Off-chain rationale store | Host-agnostic by design: the IC contract stores any public `rationale_uri` and binds it with an on-chain `vote_digest` (§4.8, §7.4), so a GitHub gist, IPFS, or any reachable URL is acceptable — tamper-evidence comes from the hash, not the host. | `docs/prd.md` §"Committee"; proposal doc §4 |

## 10. Open Decisions

| Decision | Tradeoff | Recommended default |
| --- | --- | --- |
| Portfolio Router contract design | Resolved: `contracts/PortfolioRouter.sol` is shipped. Execution model is all-or-revert; contract API, preview call signatures, cap enforcement across legs, and weight-execution path are all implemented. `VaultRegistry.isRouterEligible` expresses production readiness as registry state (see §4.2). The router is not yet on the production mainnet deployment manifest; mainnet onboarding remains planned work on the Plan tracking issue (#109). | — |
| Vault registry contract | Resolved: `contracts/VaultRegistry.sol` is shipped with stable read methods and event history, indexed by the explorer. Router eligibility is expressed as `setRouterEligible(vault, eligible)` on the registry. | — |
| Router-weight governance implementation | Resolved (MVP shipped): `contracts/RouterGovernance.sol` is deployed with admin-assigned voting power. `ADMIN_ROLE` assigns voter weights and creates proposals. Quorum, voting period, and execution delay are `ADMIN_ROLE`-configurable storage variables, not fixed in the contract: `setQuorumThreshold`, `setVotingPeriod`, and `setExecutionDelay` adjust them, bounded only by the constant floors `MIN_QUORUM_THRESHOLD` (1), `MIN_VOTING_PERIOD` (1 hour), and `MIN_EXECUTION_DELAY` (1 hour). There is no `cadenceWindow` variable and quorum is an absolute voting-power threshold, not a 5 %-of-`RM.totalSupply()` denominator. `contracts/script/DeployRouterGovernance.s.sol` defaults to a 1-hour voting period, 1-hour execution delay, and quorum 1. The 5 %/7-day/5-day/48 h figures are deferred token-holder-governance targets, not shipped contract constants. Token-holder voting (RM-balance snapshot via ERC-20 Votes) is a future goal; the snapshot-mechanism risk is documented in `docs/technical/governance-decisions.md` §6.1. | Current admin-assigned MVP is the active model; do not build RM-snapshot voting until `docs/technical/governance-decisions.md` §6.1 is resolved and a real token distribution exists. |
| Protocol-asset and agent-token vault execution | Resolved (contracts shipped): `contracts/vaults/ProtocolAssetVault.sol` (wETH/cbBTC/wSOL) and `contracts/vaults/AgentTokenVault.sol` (admin-curated agent-economy tokens) are in the source tree. Router eligibility for each vault remains ADMIN_ROLE-gated via `VaultRegistry.setRouterEligible`: both vaults stay ineligible by default until pool cardinality, per-asset TWAP windows, and the intra-vault rebalancing model are certified (see `docs/development/open-questions.md` §3.15). | Flip `isRouterEligible` only after TWAP windows, pool cardinality, and the rebalancing model are certified per §4.1. |
| Management fee and swap-fee-share mechanism | Resolved: deferred to a future phase. Current phase ships exit-fee-only disclosure. | Require a separate ADR and contract design before management fee or swap-fee-share are implemented. |
| Protocol revenue and buyback-and-burn execution | Resolved: deferred to a future phase alongside management fee and swap-fee-share. | Require a separate ADR; when implemented, add a narrow revenue collector plus buyback executor with indexed events and admin bounds. |
| On-chain admin timelock | Resolved: required. `docs/technical/security-model.md` §4 deferred this until bucket-B/C governance landed; VaultRegistry, PortfolioRouter, and RouterGovernance are now in the codebase. All five protocol contracts must transfer `ADMIN_ROLE` to an OZ `TimelockController` before mainnet scale. | Deploy `TimelockController`; transfer `ADMIN_ROLE` on all five contracts to it; configure existing Safe as proposer and canceller; prefer open execution unless a restricted Safe executor is explicitly justified. See §4.5 and issue #414. |
| Production JSON-RPC provider | Resolved: automatic failover shipped in issue #667 through the ordered `rpc_urls` array in `clients/rust-payment-client/src/config.rs` and endpoint rotation in `clients/rust-payment-client/src/rpc/mod.rs`. Safety-critical reads depend on provider correctness and availability; cross-provider consensus checking remains a separate, deferred decision. | Configure the ordered `rpc_urls` list so `rmpc` rotates to the next endpoint on transport failure. Multi-RPC consensus comparison for high-value reads stays deferred until a specific risk justifies it. |
| Production signer vendor | Architecture requires a production-grade HSM/KMS/device-bound signer for Base mainnet writes, but no vendor is chosen. | Keep signer backend trait stable; refuse software-keystore signing on Base mainnet until a production operator picks HSM/KMS. |
| Dapp hosting and CSP | Resolved: strict CSP shipped in PR #735 via `clients/dapp/src/lib/csp.ts` Vite plugin and `clients/dapp/scripts/check-csp.sh` CI check. | Maintain strict CSP policy; enforce via CI `check-csp.sh`; require static hosting with pinned dependencies and release provenance before public mainnet use. |
| Email/notification provider | No product or technical doc selects one. | Leave out until a concrete notification workflow is specified. |

## 11. Source Coverage

| Source doc | Rules applied | Rules not applicable |
| --- | --- | --- |
| `docs/prd.md` | Problem statement, success metrics, user roles, user stories, workflows, entity lifecycles, integration needs, constraints, out-of-scope boundaries, and the Investment Committee capability (roles, Committee Vote workflow, lifecycle, constraints, INV-4). | Implementation sequencing. |
| `docs/product/20260623-product-proposal-investment-committee-v0.md` | Investment Committee scope: extend `rmpc`/analyst/dapp, a signalling-only IC policy contract feeding RouterGovernance, gateway-routed signed votes, admin-gated membership, and local-devnet consensus receipt anchoring. Used for §2.4, §4.8/§4.9, §5.1/§5.3/§5.4/§5.5, §7.4/§7.5. | Product positioning and GTM framing; committee capabilities the proposal excludes from scope (inter-agent debate, retail conversion, network-effect mechanics, engineered Sybil resistance). |
| `docs/technical/definitions.md` | Canonical meanings for vault, underlying vault, adapter, receipt, router, portfolio position, composite view, router weights, governance, and agent policy. | None. |
| `docs/technical/adapter-architecture.md` | Adapter interface, vault flow, implemented adapters, adapter controls, risk model, router-vs-adapter separation. | Portfolio Router implementation details; the doc explicitly excludes router design. |
| `docs/technical/smart-contracts.md` | Current Base deployments, ERC-4626 vault behavior, roles, caps, fees, emergency paths, adapter source behavior, share-scale mitigation, VaultRegistry, PortfolioRouter, RouterGovernance, and basket-vault family (BasketVault base class and ProtocolAssetVault/AgentTokenVault/RwaVault subclasses). | None. |
| `docs/technical/security-model.md` | Role separation, live-chain safety decisions, dapp/web2 risks, upstream protocol risks, infrastructure risks, triage backlog. | Exhaustive attack table details; kept in the security model. |
| `docs/technical/rmpc-read-output-contract.md` | Stable JSON envelope, JSON-RPC source lock, partial-read contract, decimal-string integer serialization. | Per-command flag spelling and future indexer source variant. |
| `docs/technical/explorer-schema-decisions.md` | Postgres, JSON-RPC-only ingestion, poll cadence, reorg handling, single-chain scoping, read-only API boundary. | Optional later tables and future multi-chain expansion. |
| `docs/technical/dapp-credential-decisions.md` | Dapp credential boundary, wallet-signing previews, config export, unsafe software credential marker. | Frontend framework choice was later resolved by the existing dapp package. |
| `docs/technical/dapp-browser-keygen-review.md` | Fork/devnet-only browser keygen gate and no-go conditions. | Mainnet production credential generation. |
| `docs/technical/mcp-decision.md` | MCP is **not supported** in Robot Money (org-wide decision, 2026-07); agent harnesses invoke `rmpc` as process-per-call. | — (decision closed, not deferred; no MCP implementation). |
| `docs/development/testing-strategy-ethereum.md` and the testing docs under `docs/development/` (ci-suites, smoke-test-design, suite-05-audit, headless-opencode-tests) | Devnet, fork, smoke, CI, and dapp test boundaries. | Product behavior and vendor selection beyond tests. |
| Plan tracking issue #109 (GitHub) | Shipped component status and phase sequencing used as implementation context only. Delivery order is intentionally not reproduced here; the live plan is the canonical source. | Implementation-plan file references; those were retired 2026-06-09. |
