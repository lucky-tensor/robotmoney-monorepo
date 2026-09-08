/**
 * Buffered gas for e2e transactions signed DIRECTLY with a viem wallet client.
 *
 * Canonical: `docs/testing/geth-gas-estimation.md` — "Any helper that signs a
 * devnet transaction whose cost is state-dependent (vault deposits/withdrawals,
 * adapter rebalances, router splits) must buffer gas."
 *
 * viem fills a missing `gas` field by calling `eth_estimateGas` and forwarding
 * the result verbatim. That is NOT a safe limit, and the reason is not merely
 * "state might move": a bare estimate has zero *usable* margin by construction.
 *
 * Measured on the committed fork fixture (`testing/fixtures/fork-state/
 * CURRENT.anvil-state`, anvil, no network) for the 5 USDC `vault.deposit`
 * both `registry-receipt-rows.spec.ts` and `multi-vault-withdrawal.spec.ts`
 * sign — see issue #1388:
 *
 *   eth_estimateGas            = 1,415,397
 *   gas consumed on success    = 1,225,785
 *   difference                 =   189,612  (13.4% of the limit)
 *
 * That 189,612 looks like headroom and is not. Under EIP-150 every nested call
 * receives only 63/64 of the gas remaining at its depth, so the reserved 1/64
 * slices compound down the call stack and are unreachable by the frame that
 * actually needs them. `eth_estimateGas` binary-searches for the smallest
 * OUTER limit that succeeds, which means it returns exactly the value at which
 * the DEEPEST frame has nothing left over. Manual binary search confirms it:
 * 1,415,397 is the smallest limit that succeeds — one gas less and the
 * `MetaMorpho.totalAssets()` staticcall inside `MorphoAdapter` runs out of gas
 * three frames down, which is the trace #1385's decoder reported from CI.
 *
 * So any perturbation between estimate and execution tips it. With the redeem
 * at `multi-vault-withdrawal.spec.ts` interleaved between the two (the last
 * on-chain action before `registry-receipt-rows.spec.ts` deposits), the same
 * transaction on the same state failed 10/10 with the bare estimate and 0/10
 * with the 1.5x buffer below.
 *
 * 1.5x is not a guess — it is the buffer the other two harness paths already
 * apply, so all three now agree rather than diverging for no reason:
 *   - the dapp-e2e mock wallet, `helpers/wallet.ts` (issue #897)
 *   - `Fixture::estimate_gas_buffered`, `testing/smoke-test/src/lib.rs`
 *
 * NOTE: this is test robustness, not a fix for the underlying cost. The reason
 * the deposit sits near 1.4M gas at all is issue #1391 — `_targetBpsFor()`
 * returns `MAX_BPS / active`, which floors to 3333 for three adapters, so the
 * targets sum to 9999 and `_routeDeposit`'s pass 1 is a structural no-op for a
 * balanced fully-deployed vault, making every deposit pay for two rounds of
 * adapter staticcalls. Buffering the limit lets the tests survive that; it does
 * not remove it.
 */
import type { Account, Address, Chain, Hex, PublicClient, Transport, WalletClient } from "viem";

/**
 * Multiplier applied to `eth_estimateGas`, as an exact rational so there is no
 * float rounding. Matches `helpers/wallet.ts` and `Fixture::cast_send`.
 */
export const GAS_BUFFER_NUMERATOR = 3n;
export const GAS_BUFFER_DENOMINATOR = 2n;

/** Scale a raw `eth_estimateGas` result by the 1.5x harness buffer. */
export function bufferGas(estimate: bigint): bigint {
  return (estimate * GAS_BUFFER_NUMERATOR) / GAS_BUFFER_DENOMINATOR;
}

/** The subset of a transaction whose cost `eth_estimateGas` needs. */
export interface DirectTxRequest {
  to?: Address;
  data?: Hex;
  value?: bigint;
}

/** `eth_estimateGas` from `from`, scaled by the 1.5x buffer. */
export async function estimateGasBuffered(
  publicClient: PublicClient<Transport, Chain | undefined>,
  from: Address,
  tx: DirectTxRequest,
): Promise<bigint> {
  const estimated = await publicClient.estimateGas({
    account: from,
    to: tx.to,
    data: tx.data,
    value: tx.value,
  });
  return bufferGas(estimated);
}

/**
 * Sign and broadcast `tx` with a buffered gas limit.
 *
 * Use this instead of `walletClient.sendTransaction()` in any e2e spec that
 * signs a devnet transaction directly — omitting the `gas` field there is the
 * defect this helper exists to make unrepresentable. `.eslintrc.cjs` bans a
 * bare `.sendTransaction(` in `tests/e2e/**\/*.spec.ts` so the omission cannot
 * reappear silently.
 *
 * A transaction that would revert still throws here, from the estimate, exactly
 * as it does when viem estimates internally.
 */
export async function sendBufferedTransaction(
  walletClient: WalletClient<Transport, Chain | undefined, Account>,
  publicClient: PublicClient<Transport, Chain | undefined>,
  tx: DirectTxRequest & { chain?: Chain | null },
): Promise<Hex> {
  const gas = await estimateGasBuffered(publicClient, walletClient.account.address, tx);
  return walletClient.sendTransaction({
    chain: tx.chain ?? null,
    to: tx.to,
    data: tx.data,
    value: tx.value,
    gas,
  });
}
