# ADR — `rmpc` read-command stable JSON output contract

> Scope: dev-scout decision record for Phase 3 (Direct Chain-Read Query Tooling) of `Plan tracking issue #109` §9. Locks the shared output envelope every `rmpc get-*` command emits, the seam multi-read commands use to aggregate partial results, and the type-level rules that prevent JSON-number drift on large integers. No real read commands are produced by this scout — only the shared module they will all consume.

---

## 1. Status

Accepted. Authored 2026-05-06 against `Plan tracking issue #109` §9 on branch `feat/51-dev-scout-rmpc-read-commands-stable-json-output-`. Closes the open question gate in §9 ("Output contract"). No prior ADR exists for this phase. Stub code lives in `clients/rust-payment-client/src/read_output.rs`.

## 2. Context

`Plan tracking issue #109` §9 prescribes a family of direct-chain-read commands (`rmpc get-vault`, `get-balance`, `get-agent`, `get-gateway`, `get-deposit`, `get-tx`, `get-allowance`, `get-roles`) and binds them all to one output contract:

- All large integers are decimal strings.
- Every command includes `chain_id`, `block_number`, and `source: "json_rpc"`.
- Multi-read commands include a `partial` flag and a per-field error list when any sub-read fails.

The plan does not say which crate owns the envelope, what the partial-aggregation seam looks like, or how the "no JSON numbers for `u256`/`u128`" rule is enforced. Without a single anchored answer, each read-command batch (`get-vault`, `get-balance`, …) would re-implement the envelope locally and drift — exactly the failure mode §9 names ("must be enforced as one shared module before the read-command batches diverge").

The deferred existing `rmpc status` (issue #15) command predates §9 and emits a flatter shape with no envelope. It is **not** retrofit by this scout; migration is left as a follow-up.

## 3. Decisions

### 3.1 One shared module, in-crate — `rust_payment_client::read_output`

- **Decision.** Ship the envelope, partial-aggregator, and decimal-string newtypes as a single module inside the existing `rust-payment-client` crate, not a separate workspace crate.
- **Rationale.** The envelope is consumed only by `rmpc` subcommands today; the only other §9-named consumer (the Phase 5 explorer indexer) lives outside this repo's CLI tree and will deserialize the JSON wire format, not link the Rust module. A separate crate would add a workspace member, publish friction, and a stable-API obligation for zero short-term gain. If a second in-tree crate ever needs the types, promotion to `crates/rmpc-read-output` is a mechanical refactor.

### 3.2 Envelope shape — `chain_id`, `block_number`, `source`, `partial`, `errors`, `data`

- **Decision.** Field order on the wire is exactly the source-declaration order in `Envelope<T>`: `chain_id`, `block_number`, `source`, `partial`, `errors`, `data`. Snapshot tests assert on it.
- **Rationale.** Stable order makes diff review against snapshot fixtures readable. `partial` and `errors` are always present (never `Option`-stripped) so consumers can branch on `errors.length` without an `Option` check — the §9 acceptance criterion "Multi-read commands surface partial: true and a per-field error list" means the shape must exist on every command, with `errors: []` and `partial: false` for clean responses.
- **Why `errors` is always emitted.** Single-read commands cannot produce per-field partials by definition (the whole command either succeeded or returned an error exit code), but the contract is stronger if every read response has the same top-level keys. Agents and shell scripts can `jq '.errors[]'` against any read command without first probing for the field's existence.

### 3.3 `source` is locked to `"json_rpc"` for §9

- **Decision.** The `source` enum has exactly one variant, `JsonRpc`, serialized as `"json_rpc"`. Any future variant (e.g. an `Indexer` source for cross-checking) requires a new dev-scout that re-opens this ADR.
- **Rationale.** §9's "Principles" subsection is explicit: "Direct JSON-RPC reads only for canonical chain state. Explorer APIs may be optional enrichment later, but must not be the source of truth." Hard-coding the variant at the type level prevents a future read command from quietly setting `source: "etherscan"` when an RPC fails.

### 3.4 Partial aggregation via `PartialBuilder<T>`

- **Decision.** Multi-read commands construct a `PartialBuilder::new(chain_id, block_number, T::default_or_zero())`, drive sub-reads against `builder.data_mut()` on success and `builder.record_err(field, message)` on failure, and call `.finish()` to produce the final `Envelope<T>`. The builder owns the `partial` decision: `partial = !errors.is_empty()`.
- **Rationale.** The aggregation seam is the seam where the §9 "partial flag and per-field error list" rule becomes a single line of code. Per-command implementations cannot forget to set `partial`, cannot disagree on how it's computed, and cannot serialize `errors` in a different shape — the builder is the only path to an `Envelope`.
- **Field paths are dotted, JSON-shaped.** `record_err("vault.totalAssets", …)` and `record_err("adapters[2].balance", …)`. This matches how a consumer would name the field in `jq` and is what the §9 criterion "per-field error list" requires for usefulness.
- **Block-drift handling.** Sub-reads MUST share a single `block_number` (the builder is initialized with one). If a multi-read command issues sub-reads serially and the chain reorgs underneath, the implementer SHOULD record a synthetic `_meta.block_drift` field error and mark the envelope partial. The builder does not detect this on its own — the read-command batch issue that adds the first multi-read is responsible for choosing the pinning strategy (single `eth_blockNumber` up front, or `blockTag: "latest"` and tolerate drift).

### 3.5 Large integers are typed, not just documented — `DecimalU256` / `DecimalU128`

- **Decision.** Per-command `data` payloads use `DecimalU256` and `DecimalU128` newtypes for any value that exceeds the JSON-safe integer range. Their `Serialize` impl emits a JSON string in decimal form via `Display`; the wrapped raw `U256`/`u128` is never reachable through the public Serde path.
- **Rationale.** The §9 acceptance criterion "All large integers are decimal strings, never hex or numbers" is brittle if it lives only in code review checklists. Alloy's `U256` has multiple Serde impls in flight (decimal vs hex string vs the `quantity` representation); a payload struct that holds a raw `U256` field can serialize differently depending on which trait import the file picked up. Wrapping at the type level closes that gap — a payload that compiles is one that serializes per the contract.
- **Why two distinct widths.** `u128` and `U256` are physically different on-chain widths. Some Robot Money fields (per-window caps stored as `uint128`, basket amounts) are deliberately sub-`U256`. Distinct wrappers prevent a downstream batch from silently widening a `uint128` field into `U256` and bloating the JSON.
- **Type-check criterion in §9 test plan.** The test plan asks "Type-check that no field uses a JSON number for u256/u128 values." The mechanism: every per-command `data` struct uses these wrappers, so the unit tests for those structs (and the lib-level `decimal_u256_serializes_as_string_never_number` smoke test in `read_output.rs`) collectively prove the property.

### 3.6 Failed nullable sub-reads use `null`, not an in-domain sentinel

- **Decision.** A per-field `uint256` read that fails after the command has
  established its envelope is represented as `null` when the field's valid
  domain includes zero. A successful zero remains the decimal string `"0"`.
  The command still records the field in `errors` and sets `partial: true`.
- **Rationale.** A fabricated numeric default is data that a consumer can
  mistake for a successful answer. JSON `null` matches the existing
  `share_price` convention for "no answer" and is distinguishable from every
  successful `totalAssets()` result, including an empty vault.

### 3.7 Snapshot-test placement is per-command, not per-envelope

- **Decision.** The shared module ships smoke tests for `Source`, `Envelope`, `PartialBuilder`, and the decimal newtypes only. Snapshot tests over **rendered JSON for a real read command** live with each `commands/get_*.rs` module, one per read-command batch.
- **Rationale.** A snapshot test on the empty envelope alone tells you nothing about whether a real command's output drifts. The §9 acceptance criterion "Snapshot tests assert the envelope shape across commands" is satisfied by per-command snapshots, each of which exercises the shared module end-to-end. Centralizing them in `read_output` would couple the test fixture to whichever command was implemented first.

## 4. Surfaces locked for downstream issues

The following are the seams downstream read-command batches MUST consume. Adding new fields, renaming variants, or changing field order requires re-opening this ADR.

| Surface | Path | Purpose |
| --- | --- | --- |
| `Envelope<T>` | `read_output::Envelope` | Wire shape for every `get-*` command success output |
| `Source` | `read_output::Source` | Read provenance — `JsonRpc` only |
| `FieldError` | `read_output::FieldError` | One entry on the partial-read error list |
| `PartialBuilder<T>` | `read_output::PartialBuilder` | Aggregation seam for multi-read commands |
| `DecimalU256` | `read_output::DecimalU256` | `u256` field that serializes as a decimal string |
| `DecimalU128` | `read_output::DecimalU128` | `u128` field that serializes as a decimal string |

## 5. Stable RmpcError Reason-Code Variants

The four variants below are the stable, product-visible API surface of
`RmpcError` (`clients/rust-payment-client/src/errors.rs`).  Agents and
operator tooling **must** match on these variant names (the prefix of every
`Display` output) rather than on human-readable message text.  Renaming any
variant is a breaking change; see `docs/architecture.md` §7.2.

| Variant | Display prefix | Product reason code (arch §7.2) | EVM revert source |
|---|---|---|---|
| `ErrVaultDisabled` | `ErrVaultDisabled:` | `vault_disabled` | `PortfolioRouter.VaultNotRegistered` / `VaultNotActive` |
| `ErrPolicyExpired` | `ErrPolicyExpired:` | `expired_policy` | `RobotMoneyGateway.AgentPolicyExpired` |
| `ErrLegUnavailable` | `ErrLegUnavailable:` | `unavailable_leg` | `PortfolioRouter.VaultNotActive` / `VaultCapExceeded` |
| `ErrSlippageBoundExceeded` | `ErrSlippageBoundExceeded:` | `slippage_bound_exceeded` | `PortfolioRouter.SlippageExceeded` |

All four variants are unit-free (no struct fields).  Their full `Display`
strings are:

```
ErrVaultDisabled: target vault is not registered or has been disabled
ErrPolicyExpired: agent policy has expired (validUntil < block.timestamp)
ErrLegUnavailable: router leg vault is unavailable (paused, full, or de-listed)
ErrSlippageBoundExceeded: estimated shares per leg fall below the caller's minimum bound
```

These are the only `RmpcError` variants that map directly from contract
execution results rather than pre-flight checks.  Earlier variants such as
`ErrGatewayPaused`, `ErrAllowanceInsufficient`, and `ErrBalanceInsufficient`
are raised before the transaction is signed; these four are raised from the
on-chain revert data decoded from a mined or simulated transaction.

## 6. Newly discovered integration points and risks

- **`rmpc status` predates the envelope.** Migrating its output to `Envelope<StatusFound>` is a breaking change for any e2e test or downstream consumer that already parses the flat shape. Filed as an out-of-scope follow-up; not part of this scout. Recommend the migration land at the same time as the first `get-*` command, in a separate PR, so the operator-visible break is one event.
- **Pretty-printer is per-command today.** `commands/status.rs::emit` carries its own pretty-vs-compact branch. Each new `get-*` command will duplicate that branch unless we lift `emit` into `read_output`. Not part of this scout — fold into the first read-command batch if duplication shows up.
- **No CLI-flag commitment.** This ADR does not pick the `--block <n>` / `--block latest` flag spelling, the `--json` vs `--pretty` toggle, or whether `chain_id` is read from config or queried via `eth_chainId`. Those belong to the per-batch issues; the envelope is pinning-agnostic.
- **Reorg semantics are deferred.** `PartialBuilder` carries one `block_number` but does not enforce it across sub-reads. The first multi-read batch (`get-vault` is the most likely candidate) owns the choice between "pin once via `eth_blockNumber` then issue all sub-reads against that tag" and "issue against `latest` and surface drift via `_meta.block_drift`". Recommend the former — it's stricter, and §9's "block_number" is naturally a single value.
- **Indexer source variant.** §9 explicitly allows future explorer enrichment as long as JSON-RPC remains the source of truth. If/when that lands, `Source` gains an `Indexer` variant and consumers learn to ignore commands with `source: "indexer"` for safety-critical flows. Out of scope here; flagged as a known future ADR trigger.

## 7. References

- `Plan tracking issue #109` §9 — Phase 3 Direct Chain-Read Query Tooling, "Output contract"
- `clients/rust-payment-client/src/read_output.rs` — stub module implementing the surfaces above
- `clients/rust-payment-client/src/commands/status.rs` — predates the envelope; migration deferred
