# Geth Gas Estimation: Why the Devnet Harness Must Buffer Gas

> Canonical: `testing/smoke-test/src/lib.rs` (`Fixture::cast_send`,
> `Fixture::estimate_gas_buffered`).
> Related history: issue #897 (dapp-e2e mock-wallet gas buffer),
> issues #882 / #885 (four-vault seeding share invariants).

This document records a class of **silent out-of-gas failures** that the Geth
smoke-test devnet produces, why they look like contract bugs but are not, and
the buffering policy the test harness applies to match production wallet
behaviour.

---

## Symptom

The `simulated_depositors_fund_all_four_vaults` test in
`testing/smoke-test/tests/demo_seeding.rs` panicked intermittently with:

```
depositor 0x1f66…6296 holds no primary-vault shares
```

The same test passed on most runs. The job
`smoke-test-devnet-cli_meta` was flaky on `dev` itself (failure / failure /
success), so it was not introduced by any single PR.

## Root cause

The seeding flow funds each depositor and then, **concurrently across
depositors**, signs a router deposit plus four direct vault deposits. Because
the depositors run on independent threads with independent nonces, the chain
mines several of their deposits **into the same block**.

Tracing the failing run (devnet block `0x3f`, three primary-vault deposits):

| order in block | depositor | gasUsed | status |
| --- | --- | --- | --- |
| index 0 | `0xBc22…` | 1,125,400 | success |
| index 1 | `0x44D2…` | 1,222,209 | success |
| index 2 | `0x1f66…` | 1,292,424 | **revert (out-of-gas)** |

Three identical `deposit(uint256,address)` calls into the **same** primary
`RobotMoneyVault`, yet gas usage **rises with position in the block** and only
the last one reverts — having consumed essentially its entire gas cap.

Two compounding effects produce this:

1. **Same-block state growth.** The primary vault is not a passthrough; it
   routes USDC into a real strategy adapter (`AaveV3Adapter` /
   `CompoundV3Adapter` / `MorphoAdapter`). `RobotMoneyVault._routeDeposit`
   computes allocation targets from `totalAssets()`, and the adapter's `deploy`
   touches storage whose cost depends on prior in-block deposits. A deposit that
   executes *after* two earlier same-block deposits genuinely costs more gas
   than one executing first.

2. **Estimation against pre-block state.** `eth_estimateGas` runs against the
   latest *mined* block, where **none** of the three pending deposits has
   applied. All three therefore receive roughly the same estimate — the cost of
   being *first*. When `cast send` forwards that bare estimate as the gas limit,
   the deposit that ends up last in the block needs more gas than it was given
   and reverts out-of-gas.

This is the same family of failure documented in issue #897, where Geth
under-estimated an ERC-4626 `redeem` because storage-clearing gas refunds are
not reflected in `eth_estimateGas`. The mechanism differs (refunds there,
in-block growth here) but the fix is identical: **do not trust the bare
estimate**.

### Why it was silent

`cast send` exits `0` as soon as the transaction is *mined*, regardless of its
execution `status`. An out-of-gas revert produces a receipt with
`status: "0x0"` and a transaction hash — which the old `cast_send` returned as
`Ok(tx_hash)`. The seeding loop saw "success" for a deposit that minted zero
shares, and the failure only surfaced later as the misleading
"holds no primary-vault shares" assertion.

## Why this is not a contract bug

In production, deposits are submitted by real wallets (MetaMask and friends),
which **always pad** the node's gas estimate before signing — typically by
~1.5x. Under those wallets the same concurrent deposits succeed: the padding
absorbs the in-block growth. The contract behaviour is correct; the only thing
diverging from production was the test harness sending **unbuffered** gas
limits. Serialising the deposits to dodge the collision would have *hidden*
real production concurrency, not reproduced it — so the harness was changed to
behave like the wallets it stands in for.

## The fix

`Fixture::cast_send` now mirrors a production wallet:

1. **Estimate, then buffer.** `estimate_gas_buffered` calls `cast estimate`
   (from the real sender address) and scales the result by **1.5x** — the same
   buffer the dapp-e2e mock wallet adopted in #897. The send forwards this as an
   explicit `--gas-limit`.
2. **Fail loudly on revert.** After the send, the receipt's `status` is checked;
   anything other than `0x1` (including out-of-gas) returns an `Err` instead of
   an `Ok(tx_hash)`. A failed deposit can no longer masquerade as a successful
   one.
3. **Reject doomed transactions early.** A failing `cast estimate` means the
   transaction would revert on-chain; that surfaces as an `Err` before anything
   is signed.

The 1.5x buffer is generous relative to the observed in-block growth (the
reverting deposit needed only ~6% more gas than the first), and on the devnet —
where the gas price is a few hundred thousand wei — the larger gas *limit* costs
nothing meaningful against each depositor's 0.05 ETH float.

## Why a bare estimate has no margin at all (issue #1388)

The two cases above are both "state moved between estimate and execution". There
is a second, sharper reason never to forward a bare estimate, and it holds even
when nothing moves.

`eth_estimateGas` binary-searches for the **smallest outer limit at which the
transaction succeeds**. Under EIP-150 a nested call receives only 63/64 of the
gas remaining at its depth, and those reserved 1/64 slices compound down the
call stack — they are counted against the outer limit but are unreachable by the
frames below. So the estimator's answer is, by construction, the value at which
the *deepest* frame has exactly zero left over, no matter how much apparent
headroom the outer limit shows.

Measured against the committed fork fixture
(`testing/fixtures/fork-state/CURRENT.anvil-state`, anvil, no network) for the
5 USDC `RobotMoneyVault.deposit` that `registry-receipt-rows.spec.ts` and
`multi-vault-withdrawal.spec.ts` sign:

| quantity | value |
| --- | --- |
| `eth_estimateGas` | 1,415,397 |
| smallest limit that succeeds (manual binary search) | 1,415,397 |
| gas actually consumed on success | 1,225,785 |
| difference | 189,612 (13.4% of the limit) |

Those 189,612 gas look like a comfortable buffer and are not spendable. Send the
same transaction with 1,225,785 — the amount it demonstrably uses — and the
`MetaMorpho.totalAssets()` staticcall three frames inside `_routeDeposit` runs
out of gas.

The practical consequence: **`estimate` being much larger than `gasUsed` is a
symptom of a deeply nested call, not of headroom.** Interleave anything at all
between the estimate and the send — in CI, the redeem at
`multi-vault-withdrawal.spec.ts` — and the transaction reverts out of gas. On
the fixture that reproduced 10/10 with the bare estimate and 0/10 with the 1.5x
buffer, same state, same calldata, only the gas limit differing.

`gasUsed ≈ gas` on the receipt is the unambiguous signature of this failure;
`docs/development/false-green-shapes.md` covers why it must never be silent.

## Guidance for harness authors

- Any helper that signs a devnet transaction whose cost is **state-dependent**
  (vault deposits/withdrawals, adapter rebalances, router splits) must buffer
  gas. Use `Fixture::cast_send`; do not call `cast send` without a buffered
  `--gas-limit`.
- In the dapp e2e suite there are two signing paths and both buffer by 1.5x:
  the injected mock wallet (`clients/dapp/tests/e2e/helpers/wallet.ts`) for
  browser-driven flows, and `sendBufferedTransaction`
  (`clients/dapp/tests/e2e/helpers/gas.ts`) for specs that sign setup
  transactions directly. A bare `walletClient.sendTransaction()` in a
  `tests/e2e/**/*.spec.ts` file is a lint error, because viem fills the missing
  `gas` field with the unbuffered estimate.
- Never treat a `cast send` exit code as proof of execution success — always
  assert `status == 0x1`.
- If a devnet test is flaky only under concurrency, suspect gas estimation
  before suspecting the contract. Serialising to "fix" it trades a real
  production signal for a green check.
