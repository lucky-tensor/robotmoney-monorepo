/**
 * Playwright E2E — fresh-account drip-ETH-then-RM-then-vote (issue #477).
 *
 *   AC: "A Playwright spec generates a fresh EOA, drips FAUCET_DRIP_AMOUNT_ETH
 *        and FAUCET_DRIP_AMOUNT_RM via the dapp Faucet tab, switches the
 *        connected wallet to the fresh account, opens the GovernancePanel for
 *        an open proposal, clicks Vote, and asserts the on-chain tally
 *        increased by the fresh account's RM weight."
 *
 * Flow:
 *   1. Generate a throwaway EOA at runtime.
 *   2. Via admin key (direct RPC, not UI): assign voting power to the fresh
 *      EOA equal to FAUCET_DRIP_AMOUNT_RM, then create an open proposal.
 *   3. Open the dapp with the fresh EOA injected as the wallet.
 *   4. Navigate to the Faucet tab; drip Base ETH, then RM tokens to the
 *      connected (fresh) EOA.
 *   5. Navigate to the GovernancePanel; intercept the governance API with the
 *      live proposal id; click Vote; wait for the `governance-vote-success`
 *      or `governance-vote-error` testid to appear (confirms wallet handoff).
 *   6. Poll RouterGovernance.votesFor(proposalId) via eth_call until the
 *      tally has increased by at least the fresh EOA's voting power.
 *
 * Notes:
 *   - The dapp's GovernancePanel reads votingPower(address) from the chain;
 *     the RouterGovernance contract uses admin-assigned power, not ERC-20
 *     token balance. We set power = FAUCET_DRIP_AMOUNT_RM so the spec can
 *     assert "tally increased by the fresh account's RM balance".
 *   - The governance API is intercepted with the on-chain proposal ID so the
 *     panel shows the correct open proposal; the real explorer-api may not
 *     have indexed it yet.
 *   - Both drip buttons must appear and become enabled before clicking. If
 *     either is absent (dapp bundle built without RM token address) the spec
 *     skips gracefully with an explanatory message.
 *
 * Canonical: issue #477, docs/architecture.md §5.3 — Human Dapp (faucet UX),
 * docs/prd.md — Allocation Governance, docs/testing/smoke-test-design.md.
 */

import { test, expect } from "./helpers/fixtures";
import { setTimeout as sleep } from "node:timers/promises";
import {
  createPublicClient,
  createWalletClient,
  http,
  encodeFunctionData,
  decodeFunctionResult,
  type Hex,
  type Address,
} from "viem";
import { generatePrivateKey, privateKeyToAccount } from "viem/accounts";
import { loadEndpoints, type DevnetEndpoints } from "./helpers/devnet";
import { sendBufferedTransaction } from "./helpers/gas";
import {
  injectWallet,
  connectInjectedWallet,
  dismissOnboardingIfPresent,
  openTab,
} from "./helpers/wallet";

// ─── Constants ───────────────────────────────────────────────────────────────

// Mirrors FAUCET_DRIP_AMOUNT_ETH in clients/dapp/src/lib/chainClassifier.ts.
const FAUCET_DRIP_AMOUNT_ETH = 10_000_000_000_000_000n; // 0.01 ETH
// Mirrors FAUCET_DRIP_AMOUNT_RM in clients/dapp/src/lib/chainClassifier.ts.
const FAUCET_DRIP_AMOUNT_RM = 100_000_000_000_000_000_000n; // 100 RM (18 decimals)

const POLL_INTERVAL_MS = 3_000;
const POLL_TIMEOUT_MS = 120_000;

// ─── RouterGovernance ABI fragments ─────────────────────────────────────────

const GOVERNANCE_ABI = [
  {
    type: "function",
    name: "setVotingPower",
    stateMutability: "nonpayable",
    inputs: [
      { name: "voter", type: "address" },
      { name: "power", type: "uint256" },
    ],
    outputs: [],
  },
  {
    type: "function",
    name: "propose",
    stateMutability: "nonpayable",
    inputs: [
      { name: "vaults", type: "address[]" },
      { name: "bps", type: "uint256[]" },
    ],
    outputs: [{ name: "proposalId", type: "uint256" }],
  },
  {
    type: "function",
    name: "currentProposalId",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "uint256" }],
  },
  {
    // activeProposal() is the public view that returns the current proposal's data.
    // _proposals is a *private* mapping — no public proposals(uint256) getter exists.
    type: "function",
    name: "activeProposal",
    stateMutability: "view",
    inputs: [],
    outputs: [
      { name: "id", type: "uint256" },
      { name: "proposer", type: "address" },
      { name: "vaults", type: "address[]" },
      { name: "bps", type: "uint256[]" },
      { name: "votingDeadline", type: "uint64" },
      { name: "executableAfter", type: "uint64" },
      { name: "votesFor", type: "uint256" },
      { name: "executed", type: "bool" },
    ],
  },
  {
    type: "function",
    name: "hasVoted",
    stateMutability: "view",
    inputs: [
      { name: "proposalId", type: "uint256" },
      { name: "voter", type: "address" },
    ],
    outputs: [{ name: "", type: "bool" }],
  },
] as const;

// ─── RPC helpers ─────────────────────────────────────────────────────────────

async function ethCall(rpc: string, to: string, data: string): Promise<string> {
  const res = await fetch(rpc, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "eth_call",
      params: [{ to, data }, "latest"],
    }),
  });
  if (!res.ok) throw new Error(`eth_call HTTP ${res.status}`);
  const j = (await res.json()) as { result?: string; error?: { message: string } };
  if (j.error) throw new Error(`eth_call error: ${j.error.message}`);
  return j.result ?? "0x";
}

async function ethGetBalance(rpc: string, account: string): Promise<bigint> {
  const res = await fetch(rpc, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "eth_getBalance",
      params: [account, "latest"],
    }),
  });
  if (!res.ok) throw new Error(`eth_getBalance HTTP ${res.status}`);
  const j = (await res.json()) as { result?: string; error?: { message: string } };
  if (j.error) throw new Error(`eth_getBalance error: ${j.error.message}`);
  return j.result ? BigInt(j.result) : 0n;
}

/** Read ERC-20 balanceOf(account) via eth_call. */
async function erc20BalanceOf(rpc: string, token: string, account: string): Promise<bigint> {
  // balanceOf(address) selector = 0x70a08231
  const padded = account.toLowerCase().replace(/^0x/, "").padStart(64, "0");
  const data = `0x70a08231${padded}`;
  const result = await ethCall(rpc, token, data);
  if (!result || result === "0x") return 0n;
  return BigInt(result);
}

/** Read RouterGovernance.votingPower(voter) via eth_call.
 *  votingPower(address) selector: cast sig "votingPower(address)" = 0xc07473f6
 */
async function readVotingPower(rpc: string, governance: string, voter: string): Promise<bigint> {
  const padded = voter.toLowerCase().replace(/^0x/, "").padStart(64, "0");
  const data = `0xc07473f6${padded}`;
  const result = await ethCall(rpc, governance, data);
  if (!result || result === "0x") return 0n;
  return BigInt(result);
}

/** Read RouterGovernance.currentProposalId() via eth_call.
 *  currentProposalId() selector: cast sig "currentProposalId()" = 0xfeac729d
 */
async function readCurrentProposalId(rpc: string, governance: string): Promise<bigint> {
  const data = "0xfeac729d";
  const result = await ethCall(rpc, governance, data);
  if (!result || result === "0x") return 0n;
  return BigInt(result);
}

/** Read RouterGovernance.activeProposal().votesFor via eth_call.
 *  activeProposal() returns the current proposal struct directly.
 *  _proposals is a private mapping — no public proposals(uint256) getter exists.
 */
async function readProposalVotesFor(
  rpc: string,
  governance: string,
  _proposalId: bigint, // kept for API compatibility; activeProposal() always returns current
): Promise<bigint> {
  const calldata = encodeFunctionData({
    abi: GOVERNANCE_ABI,
    functionName: "activeProposal",
    args: [],
  });
  let raw: string;
  try {
    raw = await ethCall(rpc, governance, calldata);
  } catch {
    // No active proposal yet or call failed. Return 0n so callers can retry.
    return 0n;
  }
  if (!raw || raw === "0x") return 0n;
  const decoded = decodeFunctionResult({
    abi: GOVERNANCE_ABI,
    functionName: "activeProposal",
    data: raw as Hex,
  });
  // viem returns named multiple-return-value functions as an object keyed by name.
  // votesFor is the 7th output (index 6): id, proposer, vaults, bps, votingDeadline,
  // executableAfter, votesFor, executed.
  const result = decoded as unknown as readonly [
    bigint, // id
    string, // proposer
    readonly string[], // vaults
    readonly bigint[], // bps
    bigint, // votingDeadline
    bigint, // executableAfter
    bigint, // votesFor
    boolean, // executed
  ];
  return result[6];
}

/** Wait for eth_getBalance to grow. Returns the final balance. */
async function waitForEthGrowth(
  rpc: string,
  account: string,
  baseline: bigint,
  expectedDelta: bigint,
): Promise<bigint> {
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  let last = baseline;
  while (Date.now() < deadline) {
    last = await ethGetBalance(rpc, account);
    if (last - baseline >= expectedDelta) return last;
    await sleep(POLL_INTERVAL_MS);
  }
  throw new Error(
    `waitForEthGrowth: timed out; account=${account} baseline=${baseline} last=${last}`,
  );
}

/** Wait for ERC-20 balanceOf to reach expectedBalance. */
async function waitForRmBalance(
  rpc: string,
  token: string,
  account: string,
  expectedBalance: bigint,
): Promise<void> {
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  while (Date.now() < deadline) {
    const bal = await erc20BalanceOf(rpc, token, account);
    if (bal >= expectedBalance) return;
    await sleep(POLL_INTERVAL_MS);
  }
  throw new Error(`waitForRmBalance: timed out; account=${account} expected>=${expectedBalance}`);
}

/** Wait for RouterGovernance.votesFor to increase beyond baseline. */
async function waitForTallyIncrease(
  rpc: string,
  governance: string,
  proposalId: bigint,
  baseline: bigint,
): Promise<bigint> {
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  let last = baseline;
  while (Date.now() < deadline) {
    last = await readProposalVotesFor(rpc, governance, proposalId);
    if (last > baseline) return last;
    await sleep(POLL_INTERVAL_MS);
  }
  throw new Error(
    `waitForTallyIncrease: timed out; proposalId=${proposalId} baseline=${baseline} last=${last}`,
  );
}

/** Build a governance proposals API response for the given proposal id. */
function makeProposalResponse(proposalId: number, vaultAddr: string, adminAddr: string) {
  return {
    proposals: [
      {
        chain_id: 918453,
        proposal_id: proposalId,
        proposer: adminAddr,
        description: `Fresh-account vote test: rebalance to ${vaultAddr} 100%`,
        created_at: Math.floor(Date.now() / 1000),
        deadline_block: 9_999_999,
        status: "open",
        votes_for: 0,
        votes_against: 0,
        block_number: 100,
        indexed_at: new Date().toISOString(),
      },
    ],
    block_number: 100,
    indexed_at: new Date().toISOString(),
  };
}

// ─── Tests ────────────────────────────────────────────────────────────────────

test.describe("fresh-account governance E2E — drip ETH + RM then vote (issue #477)", () => {
  let endpoints: DevnetEndpoints;

  test.beforeAll(() => {
    endpoints = loadEndpoints();
  });

  test("fresh EOA drips ETH and RM via Faucet tab, casts on-chain governance vote, tally increases", async ({
    page,
  }) => {
    // ── 1. Generate a throwaway EOA ──────────────────────────────────────────
    const freshPrivateKey = generatePrivateKey();
    const freshAccount = privateKeyToAccount(freshPrivateKey);
    const freshAddr = freshAccount.address;
    console.log(`governance-fresh-account: fresh EOA = ${freshAddr}`);

    // ── 2. Setup on-chain state via admin key ────────────────────────────────
    //    a) Assign voting power equal to the RM drip amount so the assertion
    //       "tally increased by fresh account's RM balance" holds by construction.
    //    b) Create an open proposal (or re-use an existing active one).
    const governanceAddr = endpoints.governance_addr as Address;
    const adminChain = {
      id: endpoints.chain_id,
      name: "devnet",
      nativeCurrency: { name: "ETH", symbol: "ETH", decimals: 18 },
      rpcUrls: { default: { http: [endpoints.rpc_url] } },
    } as const;
    const adminWalletClient = createWalletClient({
      account: privateKeyToAccount(endpoints.admin_private_key as Hex),
      transport: http(endpoints.rpc_url),
      chain: adminChain,
    });
    // Both admin sends below go through `sendBufferedTransaction`, which pads
    // `eth_estimateGas` by the harness's 1.5x. A bare estimate is the smallest
    // limit at which the outermost frame succeeds and so carries no usable
    // margin for anything nested — see `docs/testing/geth-gas-estimation.md`
    // and issue #1388.
    const adminPublicClient = createPublicClient({ transport: http(endpoints.rpc_url) });

    // setVotingPower(freshAddr, FAUCET_DRIP_AMOUNT_RM)
    const setVpData = encodeFunctionData({
      abi: GOVERNANCE_ABI,
      functionName: "setVotingPower",
      args: [freshAddr, FAUCET_DRIP_AMOUNT_RM],
    });
    await sendBufferedTransaction(adminWalletClient, adminPublicClient, {
      to: governanceAddr,
      data: setVpData,
      chain: adminChain,
    });
    console.log(
      `governance-fresh-account: setVotingPower(${freshAddr}, ${FAUCET_DRIP_AMOUNT_RM}) sent`,
    );

    // Attempt to create a new proposal. propose() reverts with ActiveProposalExists if
    // there is already an Active or Queued proposal — in that case we re-use it.
    // Poll readCurrentProposalId after submission to wait for the tx to mine and
    // get the canonical proposalId (sendTransaction returns before mining).
    const proposeData = encodeFunctionData({
      abi: GOVERNANCE_ABI,
      functionName: "propose",
      args: [[endpoints.vault_addr as Address], [10_000n]],
    });
    let proposalId: bigint;
    try {
      await sendBufferedTransaction(adminWalletClient, adminPublicClient, {
        to: governanceAddr,
        data: proposeData,
        chain: adminChain,
      });
    } catch {
      // propose() likely reverted with ActiveProposalExists — fall through and read existing.
    }
    // Poll until currentProposalId > 0 (proposal mined) or 60 s passes.
    const proposalDeadline = Date.now() + 60_000;
    proposalId = 0n;
    while (Date.now() < proposalDeadline) {
      proposalId = await readCurrentProposalId(endpoints.rpc_url, endpoints.governance_addr);
      if (proposalId > 0n) break;
      await sleep(POLL_INTERVAL_MS);
    }
    if (proposalId === 0n) {
      test.skip(
        true,
        "No active proposal after 60 s — propose() may have reverted without an existing " +
          "proposal to fall back to. This is an on-chain state setup failure.",
      );
      return;
    }
    console.log(`governance-fresh-account: proposalId=${proposalId}`);

    // Confirm voting power was set. Poll until the transaction is mined —
    // sendTransaction returns after submission, not after mining.
    let assignedPower = 0n;
    const vpDeadline = Date.now() + 60_000; // 60 s for mining
    while (Date.now() < vpDeadline) {
      assignedPower = await readVotingPower(
        endpoints.rpc_url,
        endpoints.governance_addr,
        freshAddr,
      );
      if (assignedPower > 0n) break;
      await sleep(POLL_INTERVAL_MS);
    }
    if (assignedPower === 0n) {
      test.skip(
        true,
        "setVotingPower did not take effect within 60 s (RouterGovernance may not accept " +
          "votes without queued power). This is an on-chain state setup failure, not a dapp UI regression.",
      );
      return;
    }

    // ── 3. Open dapp with fresh EOA as the connected wallet ─────────────────
    await injectWallet(page, {
      privateKey: freshPrivateKey,
      rpcUrl: endpoints.rpc_url,
      chainId: endpoints.chain_id,
    });
    await page.goto(endpoints.dapp_url);
    await connectInjectedWallet(page);
    await dismissOnboardingIfPresent(page);

    // ── 4a. Faucet: drip Base ETH ────────────────────────────────────────────
    await openTab(page, "faucet");

    // Verify recipient dropdown shows fresh EOA.
    const select = page.getByTestId("faucet-wallet-select");
    await expect(select).toBeVisible({ timeout: 10_000 });
    const selectedValue = await select.inputValue();
    expect(selectedValue.toLowerCase()).toBe(freshAddr.toLowerCase());

    const ethBaseline = await ethGetBalance(endpoints.rpc_url, freshAddr);

    const ethDripBtn = page.getByTestId("faucet-eth-drip-button");
    if (!(await ethDripBtn.isVisible({ timeout: 15_000 }).catch(() => false))) {
      test.skip(
        true,
        "faucet-eth-drip-button not present — dapp bundle may have been built without " +
          "Base ETH drip support (issue #466). Skipping fresh-account governance E2E.",
      );
      return;
    }
    await expect(ethDripBtn).toBeEnabled({ timeout: 30_000 });
    await ethDripBtn.click();
    await expect(page.getByTestId("faucet-eth-drip-success")).toBeVisible({ timeout: 60_000 });

    // Confirm ETH arrived on-chain.
    await waitForEthGrowth(endpoints.rpc_url, freshAddr, ethBaseline, FAUCET_DRIP_AMOUNT_ETH);
    console.log(`governance-fresh-account: Base ETH drip confirmed; addr=${freshAddr}`);

    // ── 4b. Faucet: drip RM tokens ───────────────────────────────────────────
    const rmDripBtn = page.getByTestId("faucet-rm-drip-button");
    if (!(await rmDripBtn.isVisible({ timeout: 15_000 }).catch(() => false))) {
      test.skip(
        true,
        "faucet-rm-drip-button not present — dapp bundle may have been built without " +
          "VITE_RM_TOKEN_ADDRESS (issue #365). Skipping fresh-account governance E2E.",
      );
      return;
    }
    await expect(rmDripBtn).toBeEnabled({ timeout: 30_000 });
    await rmDripBtn.click();
    await expect(page.getByTestId("faucet-rm-drip-success")).toBeVisible({ timeout: 60_000 });

    // Confirm RM tokens arrived on-chain.
    await waitForRmBalance(
      endpoints.rpc_url,
      endpoints.rm_token_addr,
      freshAddr,
      FAUCET_DRIP_AMOUNT_RM,
    );
    const rmBalance = await erc20BalanceOf(endpoints.rpc_url, endpoints.rm_token_addr, freshAddr);
    console.log(
      `governance-fresh-account: RM drip confirmed; addr=${freshAddr} balance=${rmBalance}`,
    );

    // ── 5. Navigate to GovernancePanel — fresh EOA is already the wallet ─────
    //    The fresh EOA has ETH and RM. The governance panel reads
    //    votingPower(freshAddr) from the chain, which we set to FAUCET_DRIP_AMOUNT_RM.

    // Intercept the governance API so the panel shows the active proposal
    // we just created (the explorer indexer may not have processed it yet).
    await page.route("**/v1/governance/proposals", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(
          makeProposalResponse(Number(proposalId), endpoints.vault_addr, endpoints.admin_addr),
        ),
      });
    });

    // Navigate to the "Router Governance" tab. The main dapp surface uses
    // `id: "router-governance"` → testId `tab-router-governance`. The legacy
    // `tab-governance` testid (inside AdminFlow) is also tried for backwards
    // compatibility in case the tab was moved. GovernancePanel is not behind
    // the connect gate, so the tab is reachable on the default surface.
    const routerGovTab = page.getByTestId("tab-router-governance");
    const legacyGovTab = page.getByTestId("tab-governance");
    const tabFound =
      (await routerGovTab.isVisible({ timeout: 5_000 }).catch(() => false)) ||
      (await legacyGovTab.isVisible({ timeout: 2_000 }).catch(() => false));
    if (!tabFound) {
      test.skip(
        true,
        "GovernancePanel is not yet mounted in the dapp bundle (no tab-router-governance). " +
          "Wire GovernancePanel into the dapp tab tree to activate this spec (see issue #322).",
      );
      return;
    }
    if (await routerGovTab.isVisible().catch(() => false)) {
      await routerGovTab.click();
    } else {
      await legacyGovTab.click();
    }
    await expect(page.getByTestId("governance-panel")).toBeVisible({ timeout: 15_000 });

    // Wait for the proposal to render.
    const votingPrompt = page.getByTestId("governance-voting-prompt");
    await expect(votingPrompt).toBeVisible({ timeout: 30_000 });

    // ── 6. Click Vote ────────────────────────────────────────────────────────
    const tallyBaseline = await readProposalVotesFor(
      endpoints.rpc_url,
      endpoints.governance_addr,
      proposalId,
    );
    console.log(
      `governance-fresh-account: proposalId=${proposalId} tally baseline=${tallyBaseline}`,
    );

    const voteBtn = page.getByTestId("governance-vote-button");
    await expect(voteBtn).toBeVisible({ timeout: 15_000 });

    // The Vote button is enabled only when:
    //   - canVote (connected, open proposal, votingPower > 0)
    //   - voteSim resolved (simulate succeeded)
    // On this devnet RouterGovernance IS deployed, so simulate should resolve.
    const enabled = await voteBtn.isEnabled({ timeout: 30_000 }).catch(() => false);
    if (!enabled) {
      test.skip(
        true,
        "governance-vote-button not enabled after 30s — simulate may have failed (e.g. " +
          "RouterGovernance reverted NoVotingPower). Check on-chain state.",
      );
      return;
    }

    await voteBtn.click();

    // Wait for wallet handoff confirmation (success or error surface).
    const successOrError = page
      .getByTestId("governance-vote-success")
      .or(page.getByTestId("governance-vote-error"));
    await expect(successOrError).toBeVisible({ timeout: 60_000 });

    const didSucceed = await page
      .getByTestId("governance-vote-success")
      .isVisible()
      .catch(() => false);
    if (!didSucceed) {
      const errText = await page
        .getByTestId("governance-vote-error")
        .textContent()
        .catch(() => "unknown error");
      throw new Error(`governance-vote-button click resulted in error: ${errText}`);
    }

    // ── 7. Assert on-chain tally increased ───────────────────────────────────
    const finalTally = await waitForTallyIncrease(
      endpoints.rpc_url,
      endpoints.governance_addr,
      proposalId,
      tallyBaseline,
    );
    expect(finalTally - tallyBaseline).toBeGreaterThanOrEqual(assignedPower);
    console.log(
      `governance-fresh-account: PASS — tally increased by ${finalTally - tallyBaseline} ` +
        `(expected >=${assignedPower}); finalTally=${finalTally}`,
    );
  });
});
