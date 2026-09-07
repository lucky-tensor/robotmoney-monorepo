// Canonical: docs/development/false-green-shapes.md

/**
 * Deposit-failure diagnostics for the dapp e2e suite (issue #1380).
 *
 * WHY THIS EXISTS
 * `registry-receipt-rows.spec.ts` intermittently fails on `dev` itself with an
 * opaque `vault.deposit(...) REVERTED`. Gas accounting across three failing CI
 * runs proved the revert happens PAST every one of `RobotMoneyVault._deposit`'s
 * six guards — the reverted deposits burned 1.44M-1.55M gas against 1.205M for
 * a complete success, and all six guards run before `super._deposit`. So the
 * revert is deep inside `_routeDeposit`, where the vault hands USDC to the
 * Aave V3 / Compound V3 / Morpho adapters, which on this devnet call REAL Base
 * mainnet protocol code and storage ingested into the genesis alloc
 * (`testing/ethereum-testnet/config/fork-block.json`).
 *
 * Two candidates survive that evidence and a receipt cannot separate them:
 * an adapter- or protocol-internal custom error, or an out-of-gas. This module
 * is the instrument that separates them, so the NEXT occurrence names its own
 * cause in the job log instead of requiring a gas-forensics dive across
 * artifacts. It is diagnostic only — it fixes nothing and asserts nothing.
 *
 * ## Why a receipt is not enough, and how the revert data is recovered
 *
 * An Ethereum receipt carries `status: 0` and nothing else — the revert
 * payload is not stored on chain. To see it you must re-execute the call.
 * `diagnoseRevertedDeposit` therefore replays the transaction with `eth_call`,
 * rebuilt from `eth_getTransactionByHash` so the `from`, `value`, calldata AND
 * **gas limit** all match the original — passing the original gas limit is what
 * lets an out-of-gas reproduce at all, since `eth_call` otherwise runs with a
 * near-unlimited budget and would silently "succeed".
 *
 * ## Which block the replay is pinned to, and why it is never `latest`
 *
 * Primary pin: the receipt's OWN `blockNumber` — the block that contains the
 * failed transaction.
 *
 *   - State: geth evaluates `eth_call` against the state at the END of the
 *     named block. The deposit reverted, so it committed no state changes;
 *     end-of-block-N state is therefore the same state the transaction
 *     actually executed against.
 *   - Context: block N's header supplies the same `block.timestamp` and
 *     `block.number` the real execution saw. That matters here specifically —
 *     the leading hypothesis is time-accrued adapter balances crossing a
 *     `capBps` threshold, and Aave/Compound/Morpho interest accrual is a pure
 *     function of the block timestamp. Replaying at a different timestamp can
 *     fail to reproduce the very revert we are chasing.
 *
 * A cross-check replay at the PARENT block (N-1) is reported alongside it,
 * because end-of-block-N state only equals pre-transaction state when the
 * failing deposit was alone in its block (it was, in every observed failure —
 * that is what made the gas accounting possible). If the two replays ever
 * disagree, that disagreement is itself the finding.
 *
 * `latest` is deliberately NOT used anywhere in this module, for the deposit
 * replay or for the routing-state reads. A `latest`-pinned read against this
 * devnet is the documented defect class of issue #1367 and
 * `docs/testing/geth-state-lag.md`: it resolves against whatever state the
 * node has settled on now, not the state at the failure, which is exactly the
 * state under investigation.
 *
 * ## What the report says, and two things learned while proving it works
 *
 * The report ends in a single VERDICT line, so the job log answers the
 * question without a reader having to weigh the pieces. Two corrections came
 * out of exercising it against a real geth node with forced reverts:
 *
 *   1. "Empty revert data means out of gas" is true only for a TOP-LEVEL
 *      out-of-gas. A sub-call that runs out of gas leaves the caller its
 *      EIP-150 1/64 reserve, and OpenZeppelin's `Address` wrapper then reverts
 *      with a well-formed `FailedInnerCall()` selector. Reading the payload
 *      alone would report gas starvation as a custom error, so the VERDICT
 *      takes the call trace's `out of gas` frame as authoritative over it.
 *   2. `RobotMoneyVault`'s own `PerDepositCapExceeded` / `TVLCapExceeded` /
 *      `DepositsPaused` / `VaultShutdown` / `VaultRetired` / `NoActiveAdapters`
 *      guards are NOT reachable through ERC-4626 `deposit()` or `mint()`:
 *      `maxDeposit` (:565) already returns 0 or clamps to `perDepositCap` for
 *      every one of those conditions, so OpenZeppelin's max check fires first
 *      and the caller sees `ERC4626ExceededMaxDeposit(receiver, assets, max)`.
 *      They remain defence in depth for internal callers. For this
 *      investigation that is a useful exclusion: the flake's revert cannot be
 *      any of the six guards, which is independent confirmation of the gas
 *      accounting that put it past them.
 *
 * ## Safety
 *
 * Every entry point is total: each section is individually try/caught and
 * degrades to a "diagnostics unavailable" line. Nothing here throws, and the
 * spec calls it only after it has already observed `status !== "success"` — so
 * a happy-path deposit performs none of these reads and cannot be reddened by
 * them (issue #1380 AC4; the harm pattern of issues #1366 / #1374).
 */
import { decodeErrorResult, decodeFunctionResult, encodeFunctionData, type Abi } from "viem";
import { robotMoneyVaultAbiGenerated } from "../../../src/lib/abi.generated";
import { aaveV3ErrorCodes, cometErrorAbi, metaMorphoErrorAbi } from "./protocol-errors";

/**
 * Error fragments for the three strategy adapters the primary vault routes
 * through (`contracts/script/Deploy.s.sol:_approveAndRegisterAdapters` adds
 * exactly AaveV3Adapter, CompoundV3Adapter and MorphoAdapter, at 3334/3333/3333
 * capBps), plus the two shared `IPositionAdapter` errors they inherit.
 *
 * Hand-maintained rather than generated: `.github/scripts/generate_abi_bindings.sh`
 * emits no adapter binding, and this is a test-only decode table, not a call
 * surface — so it is deliberately NOT added to `src/lib/abi.ts`, whose every
 * entry must match a generated counterpart (`tests/unit/abi-parity.test.ts`).
 *
 * The adapters' own reverts are only half the story: on this devnet they call
 * real Base Aave V3 / Compound V3 / MetaMorpho code. Those protocols' error
 * tables live in `./protocol-errors` and are searched too — a custom error
 * raised in a nested protocol call bubbles up as a bare selector with no clue
 * as to which contract raised it, so a vault-only table would name the
 * unlikely causes and print "unknown" for the likely one. An unrecognised
 * selector is still reported verbatim with its full return data rather than
 * swallowed.
 */
export const strategyAdapterErrorAbi = [
  // contracts/interfaces/IPositionAdapter.sol
  { type: "error", name: "OnlyVault", inputs: [] },
  { type: "error", name: "SlippageExceeded", inputs: [] },
  // contracts/adapters/AaveV3Adapter.sol, CompoundV3Adapter.sol, MorphoAdapter.sol
  { type: "error", name: "ZeroAddress", inputs: [] },
  {
    type: "error",
    name: "WithdrawShortfall",
    inputs: [
      { name: "requested", type: "uint256" },
      { name: "actual", type: "uint256" },
    ],
  },
  // contracts/adapters/MorphoAdapter.sol
  {
    type: "error",
    name: "ExposureCapExceeded",
    inputs: [
      { name: "current", type: "uint256" },
      { name: "amount", type: "uint256" },
      { name: "cap", type: "uint256" },
    ],
  },
] as const;

/** Minimal `IStrategyAdapter.totalAssets()` fragment for the per-adapter dump. */
const adapterViewAbi = [
  {
    type: "function",
    name: "totalAssets",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "uint256" }],
  },
] as const;

/**
 * Ordered decode tables, searched innermost-repository-first. Each carries the
 * contract family it came from so a decoded name is reported with its origin:
 * `SupplyCapExceeded()` means something very different coming from Comet than
 * `PerDepositCapExceeded()` does from the vault, and the log should not make
 * the reader guess.
 *
 * The vault entry is the full generated ABI — 47 error entries, its own guards
 * plus the inherited OpenZeppelin ERC-20 / ERC-4626 / AccessControl / Pausable
 * / ReentrancyGuard errors.
 */
const decodeTables: { source: string; abi: Abi }[] = [
  { source: "RobotMoneyVault", abi: robotMoneyVaultAbiGenerated as unknown as Abi },
  { source: "strategy adapter", abi: strategyAdapterErrorAbi as unknown as Abi },
  { source: "Compound V3 Comet", abi: cometErrorAbi as unknown as Abi },
  { source: "MetaMorpho (Morpho)", abi: metaMorphoErrorAbi as unknown as Abi },
];

/** `Error(string)` — Solidity's `require(cond, "reason")` / `revert("reason")`. */
const SOLIDITY_ERROR_SELECTOR = "0x08c379a0";
/** `Panic(uint256)` — Solidity's assert / overflow / division-by-zero family. */
const SOLIDITY_PANIC_SELECTOR = "0x4e487b71";

const solidityBuiltinAbi = [
  { type: "error", name: "Error", inputs: [{ name: "reason", type: "string" }] },
  { type: "error", name: "Panic", inputs: [{ name: "code", type: "uint256" }] },
] as const;

/** How a revert payload was classified. */
export type RevertKind = "empty" | "named" | "unknown" | "malformed";

export interface RevertClassification {
  kind: RevertKind;
  /** Single-line human summary, safe to print straight into a job log. */
  summary: string;
}

/**
 * Classify raw revert data from an `eth_call` replay.
 *
 * This is the distinction the whole issue turns on:
 *   - EMPTY revert data  -> out of gas (or a bare `revert()` / failed assert
 *                           in a callee that propagated nothing)
 *   - a 4-byte selector  -> a custom error, resolved to a name here
 *
 * Pure and node-free so it is unit-testable without a chain.
 *
 * @param data       `data` field of the JSON-RPC error, if the node supplied one.
 * @param rpcMessage The node's own error message, quoted verbatim — geth says
 *                   "out of gas" there for the empty case, which corroborates
 *                   the classification instead of relying on it.
 */
export function classifyRevertData(
  data: string | null | undefined,
  rpcMessage?: string,
): RevertClassification {
  const said = rpcMessage ? ` (node said: ${JSON.stringify(rpcMessage)})` : "";
  if (!data || data === "0x") {
    return {
      kind: "empty",
      summary:
        `EMPTY revert data — out of gas, or a bare revert() that carried no ` +
        `payload. NOT a custom error${said}`,
    };
  }
  if (!/^0x[0-9a-fA-F]*$/.test(data) || data.length < 10) {
    return {
      kind: "malformed",
      summary: `malformed revert data ${data}, shorter than a 4-byte selector${said}`,
    };
  }
  const hex = data as `0x${string}`;
  const selector = data.slice(0, 10).toLowerCase();

  // Solidity's own envelopes first: decoding them against a contract ABI would
  // "succeed" and then mis-attribute them to that contract.
  if (selector === SOLIDITY_ERROR_SELECTOR || selector === SOLIDITY_PANIC_SELECTOR) {
    try {
      const decoded = decodeErrorResult({ abi: solidityBuiltinAbi, data: hex });
      const reason = String(decoded.args?.[0] ?? "");
      // Aave V3 reverts with require-strings whose "reason" is a bare number
      // from its Errors library; resolve it rather than printing an opaque code.
      const aave = aaveV3ErrorCodes[reason];
      return {
        kind: "named",
        summary:
          `${decoded.errorName}(${JSON.stringify(reason)})` +
          (aave ? ` — Aave V3 error code ${reason} = ${aave}` : ""),
      };
    } catch {
      // fall through to the per-contract tables
    }
  }

  for (const table of decodeTables) {
    try {
      const decoded = decodeErrorResult({ abi: table.abi, data: hex });
      const args = (decoded.args ?? []).map((a) => String(a)).join(", ");
      return {
        kind: "named",
        summary: `custom error ${decoded.errorName}(${args}) [${table.source}]`,
      };
    } catch {
      // try the next table
    }
  }

  return {
    kind: "unknown",
    summary:
      `custom error with selector ${selector} — not in the RobotMoneyVault, strategy-adapter, ` +
      `Comet or MetaMorpho error tables. Full return data: ${data}`,
  };
}

interface RpcError {
  code?: number;
  message?: string;
  data?: unknown;
}

interface RpcResponse<T> {
  result?: T;
  error?: RpcError;
}

async function rpcCall<T>(
  rpcUrl: string,
  method: string,
  params: unknown[],
): Promise<RpcResponse<T>> {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  return (await res.json()) as RpcResponse<T>;
}

/** Pull the revert payload out of a JSON-RPC error, wherever the node put it. */
function revertDataOf(err: RpcError | undefined): string | undefined {
  if (!err) return undefined;
  if (typeof err.data === "string") return err.data;
  // Some nodes nest it: { data: { data: "0x..." } }.
  if (err.data && typeof err.data === "object") {
    const nested = (err.data as { data?: unknown }).data;
    if (typeof nested === "string") return nested;
  }
  return undefined;
}

interface RawTransaction {
  from: string;
  to: string | null;
  gas: string;
  value: string;
  input: string;
}

/**
 * Replay the transaction with `eth_call` at `blockTag` and describe the result.
 * Returns a one-line description; never throws.
 */
async function replayAt(
  rpcUrl: string,
  tx: RawTransaction,
  blockNumber: bigint,
  label: string,
): Promise<{ line: string; classification?: RevertClassification }> {
  try {
    const call = {
      from: tx.from,
      to: tx.to,
      // The ORIGINAL gas limit — without it an out-of-gas cannot reproduce.
      gas: tx.gas,
      value: tx.value,
      data: tx.input,
    };
    const resp = await rpcCall<string>(rpcUrl, "eth_call", [call, `0x${blockNumber.toString(16)}`]);
    if (!resp.error) {
      return {
        line:
          `${label} at block ${blockNumber}: replay did NOT revert (returned ${resp.result ?? "0x"}). ` +
          `The failure is state- or ordering-dependent, not reproducible from this block's state.`,
      };
    }
    const data = revertDataOf(resp.error);
    const classification = classifyRevertData(data, resp.error.message);
    return { line: `${label} at block ${blockNumber}: ${classification.summary}`, classification };
  } catch (e) {
    return { line: `${label} at block ${blockNumber}: unavailable (${String(e)})` };
  }
}

interface CallFrame {
  type?: string;
  from?: string;
  to?: string;
  error?: string;
  revertReason?: string;
  output?: string;
  calls?: CallFrame[];
}

/**
 * Best-effort supplement to the `eth_call` replay: ask geth's `callTracer` to
 * replay the ACTUAL transaction and name the innermost frame that reverted.
 *
 * The replay above says WHAT the top-level call reverted with; this says WHICH
 * contract produced it — the difference between an unrecognised selector and
 * "MorphoAdapter -> MetaMorpho reverted with 0x…", which is what deliverable 3
 * (the job log alone identifies the cause) actually needs when the error comes
 * from protocol code this repository has no ABI for.
 *
 * It is also the only signal that catches an out-of-gas whose payload does NOT
 * arrive empty. A sub-call that runs out of gas leaves the caller with its
 * EIP-150 1/64 reserve, and an OpenZeppelin `Address`/`SafeERC20` wrapper then
 * reverts with its own `FailedInnerCall()` — a perfectly well-formed 4-byte
 * selector. So "empty revert data means out of gas" holds for a TOP-LEVEL
 * out-of-gas only; the trace is what distinguishes the nested case, which
 * would otherwise be reported as a custom error and mislead the reader.
 *
 * The devnet enables the `debug` namespace
 * (`testing/ethereum-testnet/config/docker-compose.yaml`), but a node without
 * it simply reports the tracer as unavailable.
 */
async function traceInnermostRevert(
  rpcUrl: string,
  txHash: string,
): Promise<{ line: string; outOfGasFrame?: string }> {
  try {
    const resp = await rpcCall<CallFrame>(rpcUrl, "debug_traceTransaction", [
      txHash,
      { tracer: "callTracer", tracerConfig: { onlyTopCall: false, withLog: false } },
    ]);
    if (resp.error || !resp.result) {
      return { line: `call trace: unavailable (${resp.error?.message ?? "no result"})` };
    }
    const failing: { depth: number; frame: CallFrame }[] = [];
    const walk = (frame: CallFrame, depth: number) => {
      if (frame.error) failing.push({ depth, frame });
      for (const child of frame.calls ?? []) walk(child, depth + 1);
    };
    walk(resp.result, 0);
    if (failing.length === 0) return { line: "call trace: no frame reported an error" };
    const deepest = failing.reduce((a, b) => (b.depth > a.depth ? b : a));
    const f = deepest.frame;
    const { summary } = classifyRevertData(f.output, f.revertReason ?? f.error);
    const gasFrame = failing.find((x) => /out of gas/i.test(x.frame.error ?? ""));
    return {
      line:
        `call trace: innermost failing frame at depth ${deepest.depth} — ` +
        `${f.type ?? "CALL"} ${f.from ?? "?"} -> ${f.to ?? "?"} (${f.error}); ${summary}`,
      outOfGasFrame: gasFrame
        ? `${gasFrame.frame.from ?? "?"} -> ${gasFrame.frame.to ?? "?"} at depth ${gasFrame.depth}`
        : undefined,
    };
  } catch (e) {
    return { line: `call trace: unavailable (${String(e)})` };
  }
}

/**
 * Dump the vault's routing state AS OF `blockNumber` — `totalAssets()` plus,
 * for every registered adapter, its address, `active` flag, `capBps` and
 * current assets.
 *
 * This is precisely the state gas accounting cannot show. In a passing `dev`
 * run and a failing one the five blocks before the deposit match to the
 * kilogas and only the deposit diverges, so the differing state is invisible
 * to gas; time-accrued adapter balances crossing a per-adapter `capBps`
 * threshold — which flips `_routeDeposit` between its pass-1 and pass-2 fill
 * branches — is the hypothesis this dump tests directly.
 *
 * Every read is pinned to `blockNumber`, never `latest` (issue #1367).
 * Returns lines; never throws.
 */
export async function dumpVaultRoutingState(
  rpcUrl: string,
  vault: string,
  blockNumber: bigint,
): Promise<string[]> {
  const blockTag = `0x${blockNumber.toString(16)}`;
  const read = async (to: string, data: string): Promise<`0x${string}`> => {
    const resp = await rpcCall<string>(rpcUrl, "eth_call", [{ to, data }, blockTag]);
    if (resp.error) throw new Error(resp.error.message ?? "eth_call failed");
    return (resp.result ?? "0x") as `0x${string}`;
  };
  const lines: string[] = [];
  try {
    const totalAssets = decodeFunctionResult({
      abi: robotMoneyVaultAbiGenerated,
      functionName: "totalAssets",
      data: await read(
        vault,
        encodeFunctionData({ abi: robotMoneyVaultAbiGenerated, functionName: "totalAssets" }),
      ),
    }) as bigint;
    const count = decodeFunctionResult({
      abi: robotMoneyVaultAbiGenerated,
      functionName: "adapterCount",
      data: await read(
        vault,
        encodeFunctionData({ abi: robotMoneyVaultAbiGenerated, functionName: "adapterCount" }),
      ),
    }) as bigint;
    lines.push(`vault.totalAssets() = ${totalAssets} (USDC units), adapterCount() = ${count}`);
    for (let i = 0n; i < count; i++) {
      try {
        const entry = decodeFunctionResult({
          abi: robotMoneyVaultAbiGenerated,
          functionName: "adapters",
          data: await read(
            vault,
            encodeFunctionData({
              abi: robotMoneyVaultAbiGenerated,
              functionName: "adapters",
              args: [i],
            }),
          ),
        }) as readonly [string, number, boolean];
        const [adapter, capBps, active] = entry;
        let assets = "unreadable";
        try {
          assets = String(
            decodeFunctionResult({
              abi: adapterViewAbi,
              functionName: "totalAssets",
              data: await read(
                adapter,
                encodeFunctionData({ abi: adapterViewAbi, functionName: "totalAssets" }),
              ),
            }),
          );
        } catch (e) {
          assets = `unreadable (${String(e)})`;
        }
        // capBps is a share of totalAssets: the pass-2 cap balance the router
        // compares each adapter against is `totalAssets * capBps / 10000`.
        const capBalance = (totalAssets * BigInt(capBps)) / 10_000n;
        lines.push(
          `adapters[${i}] ${adapter} active=${active} capBps=${capBps} ` +
            `assets=${assets} capBalance=${capBalance}`,
        );
      } catch (e) {
        lines.push(`adapters[${i}] unreadable (${String(e)})`);
      }
    }
  } catch (e) {
    lines.push(`routing state unavailable (${String(e)})`);
  }
  return lines;
}

/**
 * Full diagnostic report for a deposit whose receipt came back reverted.
 *
 * Call ONLY after observing `receipt.status !== "success"`. Never throws —
 * every section degrades to an "unavailable" line — so it cannot turn a
 * deposit failure into a different, more confusing failure, and it performs no
 * work at all on the happy path.
 */
export async function diagnoseRevertedDeposit(opts: {
  rpcUrl: string;
  vault: string;
  txHash: string;
  blockNumber: bigint;
  gasUsed: bigint;
}): Promise<string> {
  const { rpcUrl, vault, txHash, blockNumber, gasUsed } = opts;
  const lines: string[] = ["registry-receipt-rows: DEPOSIT REVERT DIAGNOSTICS (issue #1380)"];
  lines.push(`  tx ${txHash} mined in block ${blockNumber}`);

  let tx: RawTransaction | undefined;
  try {
    const resp = await rpcCall<RawTransaction>(rpcUrl, "eth_getTransactionByHash", [txHash]);
    tx = resp.result;
  } catch {
    tx = undefined;
  }

  // How much of the limit was burned is decidable from the receipt alone, and
  // it is corroborating evidence rather than a verdict: a TOP-LEVEL out-of-gas
  // consumes the limit exactly, while a sub-call out-of-gas leaves the caller
  // its EIP-150 1/64 reserve and so lands just under it.
  let gasNote = "";
  if (tx) {
    try {
      const limit = BigInt(tx.gas);
      const pct = limit > 0n ? Number((gasUsed * 10_000n) / limit) / 100 : 0;
      gasNote =
        gasUsed === limit
          ? " — the ENTIRE limit was consumed: a top-level out-of-gas"
          : pct >= 98
            ? " — effectively all of the limit was consumed, which a sub-call out-of-gas also " +
              "produces (EIP-150 leaves the caller 1/64); see the call trace below"
            : " — the limit was not exhausted, so gas starvation is not the cause";
      lines.push(`  gas ${gasUsed} used of ${limit} limit (${pct.toFixed(2)}%)${gasNote}`);
    } catch {
      lines.push("  gas comparison unavailable");
    }
  } else {
    lines.push("  gas comparison unavailable (eth_getTransactionByHash returned nothing)");
  }

  let topLevel: RevertClassification | undefined;
  if (tx) {
    const primary = await replayAt(
      rpcUrl,
      tx,
      blockNumber,
      "eth_call replay of the exact tx (from/value/calldata/gas-limit preserved)",
    );
    topLevel = primary.classification;
    lines.push(`  ${primary.line}`);
    if (blockNumber > 0n) {
      lines.push(`  ${(await replayAt(rpcUrl, tx, blockNumber - 1n, "cross-check replay")).line}`);
    }
  } else {
    lines.push("  eth_call replay unavailable (could not re-read the transaction)");
  }

  const trace = await traceInnermostRevert(rpcUrl, txHash);
  lines.push(`  ${trace.line}`);

  // One line that answers the question the issue was filed to answer. The
  // trace outranks the top-level payload deliberately: a nested out-of-gas
  // reaches the top as a well-formed `FailedInnerCall()` selector, so reading
  // the payload alone would call gas starvation a custom error.
  lines.push(
    `  VERDICT: ${
      trace.outOfGasFrame
        ? `OUT OF GAS — the frame ${trace.outOfGasFrame} exhausted its gas`
        : topLevel
          ? topLevel.kind === "empty"
            ? "OUT OF GAS (or a payload-less revert) — the top-level replay returned empty revert data"
            : `REVERT — ${topLevel.summary}`
          : "undetermined — no replay or trace evidence was obtainable"
    }`,
  );

  lines.push(`  routing state at block ${blockNumber} (never 'latest' — issue #1367):`);
  for (const line of await dumpVaultRoutingState(rpcUrl, vault, blockNumber)) {
    lines.push(`    ${line}`);
  }

  return lines.join("\n");
}
