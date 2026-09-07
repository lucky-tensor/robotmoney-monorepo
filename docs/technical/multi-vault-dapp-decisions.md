# ADR — Multi-vault dapp component seams, live-chain vs explorer read split, and hot-file coupling

> Scope: dev-scout decision record for the Multi-vault dapp phase of
> `Plan tracking issue #109` §"Phase: Multi-vault dapp". Resolves the
> open questions that gate every multi-vault dapp implementation issue:
> which shared data-fetching seam prevents N+1 chain reads, how reads are
> classified as live-chain vs explorer, which existing components require
> hot-file edits, and which new components are fully additive. No component
> code changes are produced by this scout.
>
> Closes the open question gate listed under `Plan tracking issue #109`
> §"Phase: Multi-vault dapp" item 1 (dev-scout).

---

## 1. Status

Accepted. Authored 2026-05-15 against `docs/architecture.md` §5.3 and
`Plan tracking issue #109` §"Phase: Multi-vault dapp" on branch
`chore/317-dev-scout-map-multi-vault-dapp-component-seams-l`.

Companion ADRs:

- `docs/technical/dapp-credential-decisions.md` — locks the credential model,
  custody boundary, and calldata preview UX that the action layer inherits.
- `docs/technical/vault-registry-decisions.md` — fixes the `VaultRegistry.sol`
  read ABI (`listVaults`, `getVault`) that the shared seam queries.
- `docs/technical/portfolio-router-decisions.md` — fixes `previewDeposit` return
  shape and the gateway extension model that the action layer uses.
- `docs/technical/explorer-schema-decisions.md` — establishes the explorer as a
  non-authoritative display source; this ADR enforces that boundary.

---

## 2. Context

The current dapp is a single-vault tool. `main.tsx` reads exactly one vault
address from `VITE_VAULT_ADDRESS` and threads it as a `vaultAddress: Address`
prop through `AgentsPanel` → `AdminFlow` → `DepositWithdrawTab` /
`useAgentRegistration`. No concept of a vault list, vault registry, or router
exists in the dapp today.

`docs/architecture.md` §5.3 specifies three view layers for the multi-vault
product:

- **Protocol layer** — no wallet required; sources data from the on-chain vault
  registry and the explorer API.
- **Account layer** — watched address or connected wallet; shows portfolio
  positions, transaction history, and agent policies.
- **Action layer** — wallet required; vault-selector deposit, multi-vault
  withdrawal, and governance voting.

Before any implementation issue begins, five questions must be resolved:

1. **Shared data-fetching seam.** What prevents N+1 chain reads when multiple
   components need the active vault list, router weights, or governance state?
2. **Live-chain vs explorer.** Which dapp reads must go to JSON-RPC (safety-
   critical) and which may use the explorer API (display only)?
3. **Hot-file identification.** Which existing files must be edited and which
   new components are fully additive?
4. **Component extension vs additive.** Which existing components can be
   extended vs. which require replacement or parallel new components?
5. **Serialization constraints.** Which downstream issues must be ordered and
   which are safe to develop in parallel?

---

## 3. Current single-vault hardcoding audit

### 3.1 `main.tsx`

`main.tsx` reads `VITE_VAULT_ADDRESS` from `import.meta.env` and passes the
result as a single `vault: Address` scalar to `StatusHeader` and `AgentsPanel`.
It also reads `VITE_GATEWAY_ADDRESS` and `VITE_GATEWAY_EXPECTED_CODE_HASH` as
scalars. There is no concept of a vault list in the entry point.

> **Update (issue #1356).** These reads no longer happen in `main.tsx` and no
> longer come from `import.meta.env`. The entry point is now a thin wrapper
> around `src/bootstrap.tsx`, which fetches `/config.json` at startup and
> derives the same scalars from the resulting runtime config
> (`deriveDappConfig`). The audit below still describes the shape of the
> problem accurately — only the source of the values changed.

**Hot-file edit required.** `main.tsx` must be changed to:

- Remove the `VITE_VAULT_ADDRESS` scalar read.
- Provide the `VaultRegistryContext` (see §4.1) to the component tree so that
  all downstream components obtain vault metadata from a single source.
- Accept `VITE_VAULT_REGISTRY_ADDRESS` as the new required env var.

### 3.2 `abi.ts`

`abi.ts` contains three ABIs: `gatewayAbi`, `erc20Abi`, and `vaultAbi`. All
three are correct for the multi-vault world. The vault registry ABI and the
portfolio router ABI are missing and must be added.

**Hot-file edit required.** `abi.ts` must be extended with:

- `vaultRegistryAbi` — `listVaults()`, `getVault(address)`, `vaultCount()` per
  `docs/technical/vault-registry-decisions.md` §3.4.
- `portfolioRouterAbi` — `deposit(...)`, `previewDeposit(uint256)`,
  `activeVaults()` per `docs/technical/portfolio-router-decisions.md` §3.1–3.2.
- `routerGovernanceAbi` — `propose(...)`, `castVote(...)`, `execute(...)`,
  `getProposal(uint256)` for the governance voting action.

No existing ABI in `abi.ts` needs to be changed. The edit is purely additive.
Risk of merge conflict with other in-flight issues: moderate (other issues may
also append to `abi.ts`); resolve by coordinating the ABI additions as a
prerequisite step for each downstream issue.

### 3.3 `DepositWithdrawTab.tsx`

`DepositWithdrawTab` accepts `vaultAddress: Address` as a single required prop.
It issues four `useReadContract` calls (`allowance`, `balanceOf`, `previewDeposit`,
`previewRedeem`) plus three `useSimulateContract` calls, all scoped to the one
address. There is no vault-selector logic.

**Hot-file edit required for the action layer vault-selector deposit and
multi-vault withdrawal flows.** Two upgrade paths:

- Option A: Extend `DepositWithdrawTab` with a vault picker prop that replaces
  the single `vaultAddress` with a `selectedVault: VaultRecord | null`. Simpler
  if the tab semantics stay "one vault at a time."
- Option B: Replace `DepositWithdrawTab` with a new `VaultSelectorDepositTab`
  (direct vault) and a separate `RouterDepositTab` (Portfolio Router path), and
  a new `MultiVaultWithdrawalTab`, leaving the existing component intact for the
  current single-vault path.

**Decision (§4.3 below):** Option B — new additive components for the Router
deposit and multi-vault withdrawal paths; `DepositWithdrawTab` extended minimally
(vault prop becomes `VaultRecord`) for the direct vault-selector case. This
avoids a full rewrite of a working component while enabling clean parallel
development of the Router path.

### 3.4 `StatusHeader.tsx`

`StatusHeader` displays a single `vaultAddress: Address` scalar in a stat card.
It issues two `useReadContract` calls against the gateway (`paused`, `usdc`).
The vault address is display-only (no on-chain reads against it).

**Hot-file edit required.** The `vaultAddress` prop becomes either a list
summary ("N active vaults") or is removed in favor of a vault registry link.
This is a small UI-only change with no read logic risk.

### 3.5 `AgentsPanel.tsx`

`AgentsPanel` passes `vaultAddress` to `useAgentRegistration` (which calls
`vault.balanceOf(address)` on the single vault) and to `AdminFlow`. In the
multi-vault world, registration detection must check share balances across all
registered vaults, not just one.

**Hot-file edit required.** `AgentsPanel` must pass the list of active vault
addresses to `useAgentRegistration` instead of a single address. The hook
iterates `balanceOf` across all vaults using a batched read (see §4.1 on query
key design). This is a behavioral change but contained within the hook.

### 3.6 `useVaultRegistration.ts` (`useAgentRegistration` hook)

Currently accepts one `vaultAddress: Address` and issues one `useReadContract`
for `balanceOf`. Must be updated to accept `vaultAddresses: readonly Address[]`
and use `useContractReads` (wagmi multi-call) or a loop of `useReadContract`
calls. The `RegistrationStatus` type and `markRegistered` function are
unchanged.

**Hot-file edit required.** Isolated to this one hook file; no component API
change beyond the prop type widening.

---

## 4. Decisions

### 4.1 Shared data-fetching seam — TanStack Query keys with a single `VaultRegistryContext`

**Decision.** The dapp introduces a single `VaultRegistryContext` (React
context) that holds:

- `vaults: readonly VaultRecord[]` — the list of all registered vaults from a
  single `listVaults()` + per-vault `getVault()` batch read.
- `isLoading: boolean`, `error: Error | null` — standard loading state.
- A `refresh()` function to invalidate and re-fetch on demand.

The context is provided once near the root (wrapping `App` below `WagmiProvider`
and `QueryClientProvider`). All downstream components that need vault metadata
consume this context rather than issuing their own `useReadContract` calls
against the registry.

**TanStack Query key design.** The `VaultRegistryContext` implementation uses
a single TanStack Query key `['vault-registry', registryAddress, chainId]` for
the batched vault list fetch. Per-vault metadata uses
`['vault', vaultAddress, chainId]`. This allows:

- A single refetch point when the registry is invalidated.
- Per-vault cache entries that can be invalidated independently (e.g. on a
  `VaultStatusChanged` event).
- Downstream components to call `useQueryClient().invalidateQueries(...)` with
  the scoped key rather than re-fetching everything.

**Router and governance state.** A `RouterContext` holds active vault weights,
pending governance proposal (if any), and the router address. It is provided
separately from `VaultRegistryContext` because its update cadence is different
(governance proposals are rare; vault registration is rarer still). The
`RouterContext` uses query key `['router', routerAddress, chainId]`.

**Rationale.** wagmi's `useReadContract` instances do not deduplicate by
default when called from sibling components. Without a shared context, the
protocol layer's vault list view, the account layer's position view, and the
action layer's vault selector would each issue independent `listVaults()` calls.
At three layers with five active vaults each, that is 15+ redundant RPC calls on
every render cycle. The shared context collapses this to one batched read.

**Rejected alternatives.**

- *Prop-drilling `vaults` from `main.tsx`.* Works but creates a long prop chain
  through `App` → `StatusHeader` + `AgentsPanel` → `AdminFlow` → all tabs. Any
  new component at an intermediate layer must forward the prop. React context is
  the standard solution for this shape.
- *Independent `useReadContract` per component, rely on wagmi's internal
  deduplication.* wagmi deduplicates identical reads within a single React tree
  render, but the registry `listVaults()` + `getVault()` fan-out pattern does
  not produce identical `useReadContract` call signatures across components.
  Deduplication is not guaranteed in this case.
- *SWR or other data-fetching library.* TanStack Query is already in
  `package.json` (via wagmi's peer dependency) and already powers wagmi's
  `useReadContract`. Adding a second caching layer conflicts with wagmi's
  internal query client.

### 4.2 Live-chain vs explorer read classification

The split between live-chain (JSON-RPC) and explorer (HTTP API) reads follows
`docs/architecture.md` §1 and the principle in
`docs/technical/dapp-credential-decisions.md` §3.1: "live chain state still
goes through RPC per implementation-plan.md §12."

**Live-chain reads (JSON-RPC, safety-critical, never substituted by explorer):**

| Data | Source | Used for |
|---|---|---|
| `gateway.paused()` | `useReadContract` | Block signing if paused; safety gate |
| `gateway.usdc()` | `useReadContract` | Approve target address; wrong address = lost funds |
| `gateway.agentOwner(agent)` | `useReadContract` | Policy read-back; controls revoke access |
| `gateway.hasRole(role, account)` | `useReadContract` | Role display in AdminFlow |
| `vault.balanceOf(address)` (all vaults) | `useReadContract` / multi-call | Registration check; portfolio position value |
| `vault.previewDeposit(assets)` | `useReadContract` | Deposit preview; slippage safety |
| `vault.previewRedeem(shares)` | `useReadContract` | Withdrawal preview; slippage safety |
| `vault.maxRedeem(owner)` | `useReadContract` | Withdrawal cap; guards submit button |
| `vault.exitFeeBps()` | `useReadContract` | Fee disclosure in preview block |
| `registry.listVaults()` | `useReadContract` | Authoritative vault list for deposit routing |
| `registry.getVault(address)` | `useReadContract` | Per-vault status (active/paused/retired) |
| `router.activeVaults()` | `useReadContract` | Weight routing for deposit split |
| `router.previewDeposit(amount)` | `useReadContract` | Router deposit preview with per-leg splits |
| USDC `allowance(owner, spender)` | `useReadContract` | Approve gate on deposit |
| Governance `getProposal(id)` | `useReadContract` | Voting UI: quorum, deadline, execution state |

**Explorer reads (HTTP API, display only, may be stale by one indexer tick):**

| Data | Explorer endpoint | Used for |
|---|---|---|
| Historical deposits / withdrawals | `GET /v1/agents/:address/deposits` | History pane |
| Indexed vault TVL snapshots | `GET /v1/vaults` | Protocol layer TVL display |
| Historical weight changes | `GET /v1/governance` | Router view weight history chart |
| Unique depositor count | `GET /v1/stats` | Protocol stats display |
| Global activity feed | `GET /v1/stats/activity` | Protocol stats recent activity |

**Classification rule.** A read is live-chain if its result gates a signing
prompt, a submit button, or a balance check. A read is explorer if its result
is purely for display and a one-block lag has no safety consequence. Any
ambiguous read defaults to live-chain. This rule is a codification of the
existing `explorerApi.ts` comment: "the dapp calls this only from the optional
history pane; live chain state still goes through RPC."

### 4.3 Component extension vs additive classification

**Existing components that require hot-file edits:**

| File | Change | Risk of merge conflict |
|---|---|---|
| `main.tsx` | Remove `VITE_VAULT_ADDRESS`; provide `VaultRegistryContext` and `RouterContext` | High — central entry point |
| `lib/abi.ts` | Add `vaultRegistryAbi`, `portfolioRouterAbi`, `routerGovernanceAbi` | Moderate — other issues append here |
| `components/StatusHeader.tsx` | Replace single vault stat card with registry summary | Low — display-only change |
| `components/AgentsPanel.tsx` | Widen `vaultAddress` to `vaultAddresses` for registration check | Low — one prop change |
| `lib/useVaultRegistration.ts` | Accept `vaultAddresses: readonly Address[]`; use multi-call | Low — isolated hook file |
| `components/DepositWithdrawTab.tsx` | Widen `vaultAddress: Address` to `vault: VaultRecord` | Moderate — tested in existing unit tests |
| `components/buildAdminTabs.tsx` | Widen `vaultAddress` in `BuildAdminTabsArgs`; add Router/governance tabs | High — assembles all tabs |

**New components that are fully additive (no edits to existing files):**

| New file | Purpose | Layer |
|---|---|---|
| `lib/VaultRegistryContext.tsx` | Provides `VaultRecord[]` from registry; deduplicates chain reads | Shared |
| `lib/RouterContext.tsx` | Provides router weights and governance state | Shared |
| `components/ProtocolLayer.tsx` | Root container for protocol layer (no wallet) | Protocol |
| `components/VaultRegistryView.tsx` | List of all vaults with TVL, risk, status | Protocol |
| `components/VaultDetailView.tsx` | Single-vault adapter breakdown, charts | Protocol |
| `components/PortfolioRouterView.tsx` | Active weights, governance proposal, history | Protocol |
| `components/ProtocolStats.tsx` | Aggregate TVL, depositor count, activity feed | Protocol |
| `components/AccountLayer.tsx` | Root container for account layer | Account |
| `components/PortfolioPositionView.tsx` | Receipt balances across all vaults | Account |
| `components/TransactionHistory.tsx` | Chronological events from explorer indexer | Account |
| `components/AgentPoliciesPanel.tsx` | All active policies for the address | Account |
| `components/VaultSelectorDepositTab.tsx` | Deposit into a chosen vault (direct ERC-4626) | Action |
| `components/RouterDepositTab.tsx` | Deposit via Portfolio Router with per-leg preview | Action |
| `components/MultiVaultWithdrawalTab.tsx` | Redeem from chosen vault or router position | Action |
| `components/GovernanceVoteTab.tsx` | Review proposal, cast vote | Action |
| `lib/routerPreview.ts` | Preview pipeline for `RouterDepositTab` (mirrors `vaultPreview.ts`) | Shared |
| `lib/usePortfolioPosition.ts` | Multi-vault `balanceOf` + share price batch hook | Shared |

### 4.4 Hot-file coupling: which issues must serialize and which are parallel-safe

**Strict serial dependency chain:**

```
This scout (317)
  └─> VaultRegistryContext + abi.ts additions (prerequisite for all below)
        ├─> Protocol layer: vault registry view (#318) — reads registry context
        ├─> Protocol layer: router + governance views (#318) — reads router context
        └─> Account layer: portfolio position view (#319) — reads registry context + balanceOf
              └─> Action layer: vault-selector deposit (#320) — needs VaultRecord from context
                    └─> Action layer: withdrawal (#321) — needs position from #319
              └─> Action layer: governance vote (#322) — reads governance state from RouterContext
```

**Issues that are parallel-safe (no shared hot-file edits):**

| Issue pair | Why safe |
|---|---|
| Protocol layer views (#318) and account layer (#319) | Neither edits the other's new additive components |
| `VaultRegistryView` and `VaultDetailView` | Both additive; only dependency is `VaultRegistryContext` |
| `PortfolioRouterView` and `ProtocolStats` | Both additive; read from separate query keys |
| `RouterDepositTab` and `GovernanceVoteTab` | Both additive; no shared hot files |
| `routerPreview.ts` and `usePortfolioPosition.ts` | Both new files; no overlap |

**Issues that must serialize (shared hot-file edits):**

| Bottleneck file | Must be resolved before |
|---|---|
| `main.tsx` — VaultRegistryContext provision | Any component that consumes the context can merge |
| `abi.ts` — registry/router/governance ABI additions | Any component that imports the new ABIs |
| `buildAdminTabs.tsx` — new tab slots | Action layer tab implementations (#320, #321, #322) |
| `DepositWithdrawTab.tsx` — vault prop widening | Vault-selector deposit (#320) |
| `AgentsPanel.tsx` / `useVaultRegistration.ts` — multi-vault registration | Account layer (#319) needs correct registration logic |

**Practical serialization sequence:**

1. Land `VaultRegistryContext` + `RouterContext` + `abi.ts` additions in a single
   preparatory PR (prerequisite for all downstream issues).
2. Protocol layer views (#318) and account layer (#319) may develop in parallel
   after step 1 merges.
3. Action layer issues (#320, #321, #322) may develop in parallel after step 1
   merges; they must not be merged until `buildAdminTabs.tsx` hot-file edits are
   coordinated.
4. `DepositWithdrawTab` widening (§3.3) should land with or before #320.

---

## 5. Integration risks and open questions deferred to implementation

1. **`VaultRegistryContext` initial load latency.** On a cold load, `listVaults()`
   returns an array of addresses and the context must fan out to N `getVault()`
   calls. At five vaults this is five sequential or batched RPC calls. The
   implementation must use wagmi's `useContractReads` (batched) rather than N
   independent `useReadContract` hooks to stay within a single eth_call batch.
   If wagmi's multicall support is unavailable for the target chain, the
   implementation falls back to sequential reads with a loading skeleton.

2. **`VITE_VAULT_REGISTRY_ADDRESS` deployment coordination.** Removing
   `VITE_VAULT_ADDRESS` is a breaking change for any existing deployment that
   sets it. The `main.tsx` change must be coordinated with a devnet re-deploy
   that has the registry live. The smoke-test fixture must be updated in the same
   PR as the `main.tsx` change.

3. **`VaultRecord.status` in context vs live `registry.getVault`.** The
   `VaultRegistryContext` caches `VaultRecord.status` as of its last fetch. If
   a vault is paused between context refreshes, the deposit action layer may
   briefly show the vault as available. The `VaultSelectorDepositTab` must always
   call `registry.getVault(vaultAddress)` live (via `useSimulateContract`) before
   enabling the submit button — it must not rely on the cached status from the
   context for safety-critical gating.

4. **Router `previewDeposit` + `deposit` leg ordering stability.** As noted in
   `docs/technical/portfolio-router-decisions.md` §5 item 2, the active vault
   list may change between `previewDeposit` and `deposit`. The `RouterDepositTab`
   must include a `vaultListHash` check or call `activeVaults()` immediately
   before signing and compare it to the preview's vault list before enabling the
   submit button.

5. **No outer share token — position display must aggregate per-vault balances.**
   `docs/architecture.md` §2.2 is explicit: the Portfolio Router does not issue
   an outer share token. `PortfolioPositionView` must iterate across all
   registered vaults and call `vault.balanceOf(address)` for each. At five
   vaults this is five live-chain reads per page load. The `usePortfolioPosition`
   hook must batch these with `useContractReads`.

6. **Explorer API surface for the multi-vault phase.** Several protocol layer
   display fields (indexed TVL, unique depositor count, activity feed) depend on
   explorer endpoints that do not exist yet (they are planned in
   `Plan tracking issue #109` §"Phase: Multi-vault explorer"). Protocol layer
   components must degrade gracefully when these endpoints return 404 or error.
   The `resolveExplorerApiUrl` / `fetchAgentDeposits` pattern in `explorerApi.ts`
   is the established model — extend it with typed functions per new endpoint.

7. **Governance voting action.** `GovernanceVoteTab` depends on the
   `RouterGovernance.sol` contract and its `castVote` / `getProposal` surface.
   That contract is deployed as of the governance phase issues (#341, #342). The
   tab is fully additive but must import `routerGovernanceAbi` from `abi.ts`
   (added in step 1 of the serialization sequence).

---

## 6. Downstream unblocked issues and sequencing

All items below are in `Plan tracking issue #109` §"Phase: Multi-vault dapp".

| Issue | Unblocked by this ADR? | Must serialize after |
|---|---|---|
| `VaultRegistryContext` + `RouterContext` + `abi.ts` additions | Yes | This ADR |
| Protocol layer: vault registry view, vault detail, router view, protocol stats (#318) | Yes | Context additions |
| Account layer: portfolio position, transaction history, agent policies (#319) | Yes | Context additions |
| Action layer: vault-selector deposit (#320) | Yes | Context additions + `DepositWithdrawTab` widening |
| Action layer: multi-vault withdrawal (#321) | Yes | Context additions + position view (#319) |
| Action layer: governance vote (#322) | Yes | Context additions |
| Dapp Playwright E2E: multi-vault flows | Yes | All action layer issues merged |
