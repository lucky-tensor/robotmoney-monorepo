# Read commands

`rmpc` exposes direct JSON-RPC reads for vault, gateway, agent, and ERC-20
state. No explorer API is used or required for safety-critical reads. Output
is stable JSON suitable for agents and shell scripts.

Common flags (every read command):

- `-c, --config <CONFIG>` — path to the operator config TOML (required).
- `--pretty` — pretty-print JSON output.

Per the implementation plan §9 read-output contract:

- All large integers are emitted as **decimal strings** (never JSON numbers).
- Every read response includes `chain_id`, `block_number`, and
  `source: "json_rpc"`.
- Commands that combine multiple reads include a `partial` flag and a
  per-field error list when some reads fail.

Exit code is 0 on success. Failures emit a structured JSON error on stderr
and a non-zero exit.

---

## `self-check`

```bash
rmpc self-check --config ./config.toml [--pretty]
```

Prints the signer-backend self-check report (v0 §9.2 JSON shape). Use this
**before any write** to confirm the encrypted-keystore is reachable, the
backend kind matches operator policy, and `allow_software_fallback` is set
correctly. No RPC reads are required for this command beyond chain-id checks
performed by the signer module on startup.

---

## `get-vault`

```bash
# Config-vault mode (legacy): read the single vault pinned in the operator config.
rmpc get-vault --config ./config.toml [--pretty]

# Registry mode: look up a specific vault by address in the VaultRegistry.
rmpc get-vault --config ./config.toml --address <0x...> [--pretty]
```

Reads a `RobotMoneyVault` (ERC-4626) directly from chain.

- **Config-vault mode** (no `--address`): reads the vault address pinned in
  the operator TOML config. Returns vault address, asset and share metadata,
  total assets and supply, share price. Fields not exposed on-chain are returned
  as `not_onchain`.
- **Registry mode** (`--address <0x...>`): looks up the vault in the
  `VaultRegistry` contract (requires `registry_address` in the operator config),
  then augments with live ERC-4626 state. Returns registry metadata (name,
  status, registered_at) plus live accounting (asset, decimals, total_assets,
  total_supply, share_price). Exits non-zero when the address is not registered.
  The registry stores only `VaultMetadata { name, asset, registeredAt }` and a
  status, so there is no risk label, mandate, deposit cap, exit fee or separate
  receipt token to return — caps and exit fees are live reads on the vault
  contract, and every vault is its own ERC-4626 receipt token (issue #1362).

---

## `get-vaults`

```bash
rmpc get-vaults --config ./config.toml [--pretty]
```

Lists all vaults registered in the `VaultRegistry` contract (requires
`registry_address` in the operator config). Returns a `vaults` array with
registry metadata and live `total_assets` for each registered vault (active,
paused, and retired). An empty registry returns `vaults: []` with exit code 0.

Use it to discover all available deposit destinations programmatically.

---

## `get-router`

```bash
rmpc get-router --config ./config.toml [--pretty]
```

Reads the configured `PortfolioRouter` contract (requires `router_address` in
the operator config). Returns the router address, the ordered list of vault
addresses with their weight in basis points (max 10 000 = 100%), and the
global `routerCap` in USDC base units (0 = uncapped).

All reads are pinned to a single `eth_blockNumber` snapshot. If any sub-read
fails the response is a partial envelope (`partial: true`) with per-field
errors; exit code is still 0.

Use it before a deposit to confirm current allocation weights.

---

## `get-governance`

```bash
rmpc get-governance --config ./config.toml [--pretty]
```

Reads the configured `RouterGovernance` contract (requires `governance_address`
in the operator config). Returns:

- `active_proposal` — `null` when no proposal is pending; otherwise the
  proposal id, proposed vault addresses, proposed weight bps, vote tallies
  (`votes_for`, `votes_against`), and expiry timestamp.
- `cadence` — quorum threshold (decimal string), execution delay in seconds,
  and minimum cadence between proposals in seconds.
- `current_weights` — the last applied weight vector (vault addresses + bps).

All reads are pinned to a single `eth_blockNumber` snapshot. Partial envelopes
are supported; exit code is 0 even for partial results.

---

## `get-gateway`

```bash
rmpc get-gateway --config ./config.toml [--pretty]
```

Reads the configured `RobotMoneyGateway`. Returns gateway address, chain id,
configured USDC and vault addresses, the runtime code hash (compared against
`gateway_runtime_hash` in config), and the pause flag. A code-hash mismatch is
a hard refusal at write time (see `references/safety.md`).

---

## `get-agent`

```bash
rmpc get-agent --config ./config.toml --agent <0x...> [--pretty]
```

Reads `agents[address]` policy: `active`, `validUntil`, `maxPerPayment`,
`maxPerWindow`, `shareReceiver`, plus the current
`agentWindowGross[address][windowId]` for the live window. Use it before any
deposit to confirm the agent is authorized and has remaining cap.

---

## `get-roles`

```bash
rmpc get-roles --config ./config.toml --address <0x...> [--pretty]
```

Reports membership of `ADMIN_ROLE`, `PAUSER_ROLE`, and `AGENT_ROLE` on the
gateway for the supplied address. The gateway enforces an invariant that an
`AGENT_ROLE` holder must not also hold `ADMIN_ROLE` or `PAUSER_ROLE`; this
command is the agent-side check.

---

## `get-balance`

```bash
rmpc get-balance --config ./config.toml --address <0x...> [--pretty]
```

Reads an ERC-20 balance for `address` on the configured USDC. Output includes
the raw smallest-unit string, the token address, and decimals so callers can
format without re-deriving constants.

---

## `get-allowance`

```bash
rmpc get-allowance --config ./config.toml \
  --owner <0x...> --spender <0x...> [--pretty]
```

Reads `allowance(owner, spender)` on the configured USDC. The agent must
ensure `allowance(self, gateway) >= amount` before `rmpc deposit` — the
client mirrors this in preflight (§4.4) and refuses with a structured error
otherwise.

---

## `get-router`

```bash
rmpc get-router --config ./config.toml [--pretty]
```

Reads the Portfolio Router state. Returns the current vault weight vector
(`weights` array of `{vault, bps}` entries) and the global `router_cap`
(decimal string; `"0"` means uncapped). Requires `router_address` in the
operator config.

---

## `get-governance`

```bash
rmpc get-governance --config ./config.toml [--pretty]
```

Reads the RouterGovernance state. Returns the current proposal id,
cadence parameters (`voting_period_secs`, `execution_delay_secs`,
`quorum_threshold`, `total_voting_power`), the current router weight vector
as seen by governance, and the active proposal details (if any). Requires
`governance_address` in the operator config.

---

## `get-timelock`

```bash
rmpc get-timelock --config ./config.toml [--pretty]
```

Reads the deployed `TimelockController` contract state (issue #414). Returns:

- `min_delay_secs` — minimum delay in seconds before a scheduled operation
  can be executed.
- `proposers` — list of addresses holding `PROPOSER_ROLE` on the timelock.
- `executors` — list of addresses holding `EXECUTOR_ROLE` on the timelock.
- `pending_ops` — array of scheduled operations that have not yet been
  executed or cancelled. Each entry includes `operation_id` (0x-hex bytes32)
  and `ready_timestamp` (unix seconds after which execution is allowed).

Requires `timelock_address` in the operator config.

All reads are pinned to a single `eth_blockNumber` snapshot. Partial envelopes
are supported; exit code is 0 even for partial results.

---

## `get-deposit`

```bash
rmpc get-deposit --config ./config.toml --deposit-id <0x...> [--pretty]
```

Looks up a gateway deposit by its `paymentId`. `paymentId` is the keccak hash
of `(chain_id, gateway, agent, orderId, amount, idempotencyKey)` and is
returned by `rmpc deposit` and `rmpc status`. Use this to confirm an
on-chain record after a successful broadcast.

---

## `get-tx`

```bash
rmpc get-tx --config ./config.toml --tx-hash <0x...> [--pretty]
```

Returns the transaction receipt status (success/reverted), block number,
gas used, and any decoded `AgentDeposit` event from the gateway log set.
Useful to confirm that a broadcast tx was actually mined and not just
accepted into the mempool.

---

## Recommended call order before a write

```text
rmpc self-check         --config ./config.toml
rmpc get-gateway        --config ./config.toml
rmpc get-vault          --config ./config.toml
rmpc get-agent          --config ./config.toml --agent <self>
rmpc get-balance        --config ./config.toml --address <self>
rmpc get-allowance      --config ./config.toml --owner <self> --spender <gateway>
```

Only proceed to `rmpc deposit` if every read returns a healthy state and the
agent has remaining `maxPerPayment` and `maxPerWindow` capacity for the
intended amount.
