# ADR — Vault Registry contract seams, event schema, and indexer integration points

> Scope: dev-scout decision record for the Vault registry phase of
> `Plan tracking issue #109` §"Phase: Vault registry". Resolves the four
> open questions that gate any VaultRegistry.sol code: standalone vs extension,
> access-control role, gateway-coupling model, and the canonical event +
> read-ABI shape. No contract bytecode or schema migrations are produced by this
> scout.
>
> Closes the open question gate listed under `Plan tracking issue #109`
> §"Phase: Vault registry" item 1.

---

## 1. Status

Accepted. Authored 2026-05-14 against `docs/architecture.md` §4.2, §10 and
`Plan tracking issue #109` §"Phase: Vault registry" on branch
`chore/292-dev-scout-map-vault-registry-contract-seams-even`.

---

## 2. Context

`docs/architecture.md` §10 marks the vault registry as "Resolved: on-chain
contract" and specifies that it must expose stable read methods and emit events
indexable by the explorer. `Plan tracking issue #109` names seven downstream
items that are blocked until this scout closes:

1. `VaultRegistry.sol` implementation
2. Deploy script (devnet + Base fork)
3. Explorer indexer extension (`VaultRegistered` / `VaultStatusChanged` ingestion)
4. Explorer API `GET /v1/vaults`
5. `rmpc get-vaults`
6. `rmpc get-vault <address>`
7. Fork e2e: register → list → status-change round-trip

The current codebase has `RobotMoneyGateway` with a single immutable
`vaultContract` pinned at construction time and three separated roles:
`ADMIN_ROLE`, `PAUSER_ROLE`, and `AGENT_ROLE` (see
`contracts/gateway/AccessRoles.sol`). There is no vault-list storage of any
kind — the gateway treats the vault address as a fixed immutable.

Four questions must be resolved before any implementation issue starts:

1. **Standalone vs extension.** Should `VaultRegistry` be a new top-level
   contract or extend an existing gateway surface?
2. **Access-control role.** Which role may call `registerVault` /
   `setVaultStatus`?
3. **Gateway coupling.** Does registration also update the gateway allowlist,
   or are they independent?
4. **ABI and event shape.** What are the minimum stable read methods and the
   exact event signatures?

---

## 3. Decisions

### 3.1 Standalone contract — **VaultRegistry.sol is a new, independent contract**

- **Decision.** `VaultRegistry.sol` is a standalone contract, not an extension
  of `RobotMoneyGateway` or any other existing surface. It inherits OpenZeppelin
  `AccessControl` (sharing the same role constants) but is deployed independently
  and has its own address.
- **Rationale.**
  - The gateway currently pins a *single* vault as an immutable. Extending it
    to hold a dynamic list would require a breaking storage layout change or a
    proxy upgrade — neither is compatible with the current non-proxy deployment
    model described in `docs/architecture.md` §4.1.
  - A standalone registry makes each concern independently upgradeable and
    independently pausable, consistent with the vault-level independence
    principle in §4.1: "Each vault is independently observable and independently
    pausable."
  - The Portfolio Router (`Plan tracking issue #109` §"Phase: Portfolio
    Router contract") reads `listVaults()` from the registry. If the registry
    were embedded in the gateway, the router would depend on the gateway
    internals — tight coupling that does not appear anywhere in the architecture.
  - `rmpc` protocol-scope reads (`get-vaults`, `get-vault`) require only chain +
    registry config, no signer key (§5.1). A standalone address satisfies that
    config requirement cleanly.
- **Rejected alternatives.**
  - *Extension of `RobotMoneyGateway`.* Breaking change to a live non-proxy
    contract; couples router to gateway; violates vault independence.
  - *Embedded in `PortfolioRouter`.* Circular dependency: the router needs the
    registry to enumerate deposit destinations; the registry cannot depend on
    the router.

### 3.2 Access-control role — **ADMIN_ROLE grants registration rights**

- **Decision.** `registerVault(address vault, VaultMeta calldata meta)` and
  `setVaultStatus(address vault, VaultStatus status)` are restricted to
  `ADMIN_ROLE`. The registry inherits `AccessControl` from OpenZeppelin and
  uses the same `ADMIN_ROLE` bytes32 constant defined in
  `contracts/gateway/AccessRoles.sol`:
  `keccak256("ADMIN_ROLE")`.
- **Rationale.**
  - `docs/architecture.md` §5.2 explicitly lists "add vaults, change mandates,
    alter router weights, or bypass disabled vaults" as actions an agent *cannot*
    take; therefore no `AGENT_ROLE` holder may call these functions.
  - The existing `ADMIN_ROLE` semantics ("grants/revokes other roles, sets
    policy, unpauses") already cover protocol-configuration operations. Vault
    registration is a protocol-configuration operation, not a per-depositor
    operation.
  - A new `REGISTRY_ROLE` would add complexity without adding security value at
    this phase. A future ADR may introduce it if a multi-sig or time-lock
    structure requires finer separation.
  - The depositor-sole-authority model (`docs/architecture.md` §5.2 and issue
    #269) is unaffected: depositors govern their own agent policies; they do not
    govern the protocol vault list.
- **Rejected alternatives.**
  - *New `REGISTRY_ROLE`.* Premature — adds a fourth key to the separation
    invariant in `AccessRoles._grantRole` without a clear multi-party governance
    need at this phase.
  - *`DEFAULT_ADMIN_ROLE` only.* Too permissive — would allow any `DEFAULT_ADMIN`
    holder even if the role separation evolves.

### 3.3 Gateway coupling — **independent; gateway updated separately after registration**

- **Decision.** `registerVault` on the registry does **not** automatically
  update the gateway allowlist. The two contracts are independent. After a vault
  is registered in `VaultRegistry`, a second ADMIN transaction must update the
  gateway to allow that vault as a deposit destination. The deploy script for
  each vault handles both steps explicitly.
- **Rationale.**
  - The current gateway has a single immutable vault; moving to a dynamic
    allowlist requires a separate gateway extension issue (already planned in
    `Plan tracking issue #109` §"Phase: Portfolio Router contract": "Gateway:
    extend allowed destinations to include the Portfolio Router"). That extension
    should happen in its own scoped PR.
  - Automatic coupling via a callback or shared storage would create a circular
    dependency and introduce a re-entrancy surface: the registry calling the
    gateway during registration, or the gateway querying the registry during
    deposit validation.
  - Observability: two separate transactions emit two separate events, making
    the audit trail clearer than a single cross-contract call that triggers
    state changes in two contracts atomically.
- **Hot-file coupling summary.** The only files that change when a new vault is
  registered are:
  - `contracts/VaultRegistry.sol` (registry state)
  - The deploy/admin script (two transactions: `registerVault` + gateway update)
  - Explorer indexer (ingests `VaultRegistered` event)
  - `rmpc` config (registry address; vault addresses are discovered on-chain)
  - The gateway's allowlist storage (separate ADMIN transaction, not atomically
    coupled)
- **Rejected alternatives.**
  - *Registry calls gateway on registration.* Circular dependency; re-entrancy
    surface; couples deploy order.
  - *Gateway queries registry on every deposit.* Adds a cross-contract call to
    the hot deposit path; increases gas; makes the gateway's behavior depend on
    the registry's pause/availability state.

### 3.4 Minimum stable read ABI

> **SUPERSEDED BY THE SHIPPED CONTRACT — do not bind a client to the `getVault`
> shape below.** `VaultRegistry.sol` as implemented in #329 returns **two**
> top-level outputs, `(VaultMetadata metadata, VaultStatus status)`, where
> `VaultMetadata` is `{ string name; address asset; uint256 registeredAt; }`.
> The nine-field `VaultRecord` in this section was never implemented: there is
> no on-chain `riskLabel`, `mandate`, `receiptToken`, `depositCap` or
> `exitFeeBps` anywhere in the registry. Clients that copied it could not decode
> a real response — the dapp's copy was fixed in #1348 and rmpc's in #1362.
> `listVaults()` and `vaultCount()` below are accurate as written. The rest of
> this section is kept as the historical record of the decision, not as a
> contract.

The following three view functions constitute the stable read surface for `rmpc`,
the Portfolio Router, the explorer indexer, and the dapp protocol layer. These
signatures must not be modified without a superseding ADR.

```solidity
/// @notice Returns the stored metadata for `vault`.
/// @dev Reverts with `VaultNotRegistered(vault)` if the address is unknown.
function getVault(address vault) external view returns (VaultRecord memory);

/// @notice Returns the addresses of all registered vaults regardless of status.
/// @dev Includes active, paused, and retired vaults. Callers filter by status.
function listVaults() external view returns (address[] memory);

/// @notice Returns the number of registered vaults (all statuses).
function vaultCount() external view returns (uint256);
```

Supporting types:

```solidity
enum VaultStatus { Active, Paused, Retired }

struct VaultRecord {
    address vault;          // ERC-4626 contract address
    string  name;           // human-readable label (e.g. "RobotMoney USDC Vault")
    string  riskLabel;      // e.g. "stable-yield", "protocol-asset"
    string  mandate;        // short mandate text matching docs
    VaultStatus status;
    address receiptToken;   // == vault address for ERC-4626; explicit for clarity
    uint256 depositCap;     // max total assets; 0 = no cap
    uint16  exitFeeBps;     // basis points; matches vault.exitFeeBps()
    uint64  registeredAt;   // block.timestamp of registration
}
```

`rmpc get-vaults` and `rmpc get-vault <address>` augment these chain-read fields
with TVL (from `vault.totalAssets()`), cap headroom, and explorer-indexed
historical data per `docs/architecture.md` §5.1 and the
`rmpc-read-output-contract.md` provenance rule (`source: "json_rpc"` for
safety-critical fields).

### 3.5 Event signatures

The following two events are the canonical indexable events for the Vault
registry phase. They must appear verbatim in `VaultRegistry.sol`.

```solidity
/// @notice Emitted when a vault is added to the registry for the first time.
/// @param vault          The registered ERC-4626 vault address. Indexed for log filtering.
/// @param name           Human-readable label stored in the registry.
/// @param riskLabel      Risk category string (e.g. "stable-yield").
/// @param depositCap     Maximum total-assets cap at registration time; 0 = no cap.
/// @param registeredAt   block.timestamp of the registration transaction.
event VaultRegistered(
    address indexed vault,
    string  name,
    string  riskLabel,
    uint256 depositCap,
    uint64  registeredAt
);

/// @notice Emitted when an admin changes a vault's operational status.
/// @param vault          The affected ERC-4626 vault address. Indexed for log filtering.
/// @param oldStatus      The status before this call.
/// @param newStatus      The status after this call.
/// @param changedAt      block.timestamp of the status-change transaction.
event VaultStatusChanged(
    address indexed vault,
    VaultStatus     oldStatus,
    VaultStatus     newStatus,
    uint64          changedAt
);
```

**Indexer field mapping** (extends `docs/technical/explorer-schema-decisions.md`
§2 minimum tables — specifically the `vaults` table that must be added):

| Event field          | Indexer column            | Notes                                      |
|----------------------|---------------------------|--------------------------------------------|
| `vault` (topic 1)    | `vaults.address`          | Primary key                                |
| `name`               | `vaults.name`             | Non-indexed ABI data                       |
| `riskLabel`          | `vaults.risk_label`       | Non-indexed ABI data                       |
| `depositCap`         | `vaults.deposit_cap`      | `NUMERIC(78,0)` per explorer ADR §3.1      |
| `registeredAt`       | `vaults.registered_at`    | Unix timestamp                             |
| `oldStatus`          | `vaults.status` (prev)    | For status history; upsert on `VaultStatusChanged` |
| `newStatus`          | `vaults.status`           | Current operational status                 |
| `changedAt`          | `vaults.status_changed_at`| Unix timestamp of last status change       |
| block_number         | `vaults.registered_block` | From log metadata, not ABI data            |
| tx_hash              | `vaults.registered_tx`    | From log metadata                          |

Idempotency key for ingestion follows the same pattern as `agent_deposits`
(explorer ADR §3.4): `(chain_id, block_number, log_index)`.

---

## 4. Downstream unblocked issues and sequencing

All items below are in `Plan tracking issue #109` §"Phase: Vault registry".
They are **all unblocked** by this scout and may begin in parallel except where
noted.

| Issue description | Unblocked? | Depends on |
|---|---|---|
| `VaultRegistry.sol` implementation | Yes — ABI and event shape are now fixed | This ADR |
| Deploy script (devnet + Base fork) | Yes | `VaultRegistry.sol` |
| Explorer indexer extension | Yes — event schema is fixed | This ADR (for schema); `VaultRegistry.sol` for ABI import |
| Explorer API `GET /v1/vaults` | Yes | Explorer indexer extension |
| `rmpc get-vaults` | Yes — read ABI is fixed | `VaultRegistry.sol` deployed |
| `rmpc get-vault <address>` | Yes | `VaultRegistry.sol` deployed |
| Fork e2e: register → list → status-change | Yes | `VaultRegistry.sol` + Deploy script |

The Portfolio Router phase (`Plan tracking issue #109` §"Phase: Portfolio
Router contract") depends on `listVaults()` being available on a deployed
registry. It is blocked on the Vault registry phase completing, not on this
scout alone.

No items in the Vault registry phase require sequencing relative to each other
beyond the deploy dependency: `VaultRegistry.sol` must compile before the deploy
script can reference its ABI, and the deploy script must produce an address
before `rmpc` and the fork e2e can run against it.

---

## 5. Integration risks and open questions deferred to implementation

The following risks were discovered during scouting. They are not blockers for
implementation issues to begin, but the assigned implementer must address each.

1. **Gateway allowlist extension is in Portfolio Router phase, not Vault
   registry phase.** The current gateway pins one immutable vault. Until the
   gateway is extended (Portfolio Router phase), `rmpc deposit` will continue to
   hit only the immutable vault even after a second vault is registered. The
   deploy script must document this limitation clearly.

2. **`listVaults()` return size.** Returning an unbounded `address[]` is safe
   for the current registry size (O(10) vaults), but a future implementation
   issue must add pagination or a cursor pattern if vault count grows beyond
   ~200. Add a `TODO` comment in `VaultRegistry.sol` flagging this.

3. **`exitFeeBps` in `VaultRecord`.** The value is stored at registration time
   and may drift from `vault.exitFeeBps()` if the vault owner updates the fee.
   The `rmpc get-vault` implementation must read `vault.exitFeeBps()` live from
   the chain and compare to the registry snapshot. Explorer display may use the
   registered snapshot, labelled with its block number.

4. **ERC-4626 `asset()` check at registration.** `registerVault` should call
   `IERC4626(vault).asset()` and revert if it does not equal the configured
   USDC address, mirroring the `AssetMismatch` guard in `RobotMoneyGateway`.
   This prevents non-USDC vaults from appearing in the USDC registry view.

5. **`string` fields in events cost more gas than `bytes32`.** If gas is a
   concern at registration time, `name` and `riskLabel` can be stored as
   `bytes32` (truncated to 31 bytes) and emitted as `bytes32` in events. The
   current decision uses `string` for human readability; a future ADR may
   supersede this if gas analysis shows it matters at realistic vault counts.
