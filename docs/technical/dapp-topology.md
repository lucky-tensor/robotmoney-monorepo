# Dapp Hosting Topology — Paranoid Mode

Status: design spec, not yet implemented.

This document records the security posture target for hosting the
Robot Money admin dapp. The threat model assumes the hosting
infrastructure itself is hostile or compromisable, and asks: what is
the minimum trust the dapp must place in any party other than the
user's own wallet and the chain?

---

## Load-bearing insight

When the dapp calls `signer.sendTransaction(...)` via the injected
EIP-1193 provider (MetaMask, Rabby, Frame, hardware-wallet bridges),
**signing and broadcast happen through the wallet's own RPC**, not
through any URL the dapp was built with. The dapp's `VITE_FORK_RPC_URL`
only governs reads the dapp initiates itself (notably `eth_getCode`
for bytecode verification).

A compromised dapp-owned RPC therefore cannot tamper with a signed
transaction. The worst it can do is **lie about state** — balances,
allowances, `eth_call` simulation results — to trick the user into
signing the wrong intent. Display can be untrusted; **intent cannot**.

This is the lever the paranoid topology pulls: eliminate every byte
of state the dapp consumes from an attacker-controlled source before
the user signs.

---

## Topology

### 1. Static, content-addressed bundle

- Build deterministically (pinned toolchain versions, locked deps,
  reproducible Vite output).
- Pin the build to IPFS; publish via ENS `contenthash`. User-facing
  URL is e.g. `robotmoney.eth.limo` or `robotmoney.eth` via an
  ENS-aware resolver / wallet.
- No origin server, no SSR, no backend the host can mutate post-release.
- Pair every release with a SLSA / sigstore attestation so any user
  can rebuild from the tagged commit and verify the CID byte-for-byte.

### 2. No dapp-owned RPC

- Route **every** read through `window.ethereum.request(...)`, i.e.
  the wallet's configured RPC (the same node that will carry the
  user's write).
- Drop `VITE_FORK_RPC_URL` entirely. The dapp has zero network
  endpoints under hoster control.
- A user running a personal node, Alchemy with their own key, or a
  paid Base / Ethereum RPC sees end-to-end trust: every byte the
  dapp reads, and every byte it writes, traverses an endpoint the
  user chose.

### 3. Pinned on-chain invariants

- `VITE_GATEWAY_EXPECTED_CODE_HASH` is baked into the bundle at build
  time, and **only** there. It is excluded from `RUNTIME_CONFIG_KEYS`
  in `clients/dapp/src/lib/runtimeConfig.ts`, so a `/config.json` that
  carries the key has it dropped with a warning rather than merged.
- Gateway/vault addresses, `VITE_ENV_CLASS`, and the RPC/explorer
  endpoints are **not** build-time-only. Issue #1356 moved them into
  the `/config.json` document the dapp fetches at startup, so one
  commit-addressed image can serve several environments. The build-time
  env remains the base layer, so a deployment that serves no such
  document behaves exactly as it did before.
- The split is the point: an address the dapp merely talks to may be
  supplied at runtime, because the pin is what decides whether talking
  to it is safe. The pin itself may not, because a value fetched from a
  separately-mutable document sits outside whatever attests the bundle.
- The dapp refuses admin writes unless `keccak256(getBytecode(gateway))`
  matches the pinned hash. Even if the user's RPC lies about
  `eth_getCode`, the mismatch fails closed — and so does a missing pin,
  so an image built without one enables no admin writes at all.
- The dapp trusts the **bundle**, not the **node**, for the pin. A
  gateway address supplied at runtime still has to present bytecode
  hashing to the pinned value before any admin write is enabled.

#### Decision — the code-hash pin is build-time-only (issue #1375)

Recorded here because #1356 briefly made the pin runtime-configurable
along with the deployment-shaped keys, which made the first bullet above
false for a while.

- **Decision.** `VITE_GATEWAY_EXPECTED_CODE_HASH` is removed from
  `RUNTIME_CONFIG_KEYS` and joins `VITE_FAUCET_HARNESS_PRIVATE_KEY` and
  `VITE_HISTORY_PANE` as a documented load-bearing exclusion.
- **Why.** `docs/architecture.md` §10 makes release provenance a
  prerequisite for public mainnet use. Once the bundle is attested, a
  pin fetched at runtime would be the one value the attestation did not
  cover — a verified artifact taking its trust anchor from an unverified
  document served by the same origin. Nothing is weakened *today* (the
  dapp image carries no attestation yet, and `/config.json` is served by
  the dapp's own origin rather than by the node the threat model
  distrusts), so this is pre-emptive rather than a fix for a live hole.
- **Cost, stated plainly.** The gateway's `usdcToken`, `vaultContract`
  and `routerContract` are `immutable`, so they live in its runtime
  bytecode and the hash is deployment-specific. There is therefore no
  correct hash for an environment-agnostic image to pin: a bundle that
  is to enable admin writes must be built for its deployment. The
  generic image published by `.github/workflows/release-dapp.yml` builds
  with an empty pin and stays admin-read-only — which is the fail-closed
  outcome, not a regression to fix by re-opening the runtime path.

### 4. Indexer / explorer is untrusted UI

- The explorer-api / indexer is for display only: history panes,
  status timelines, paginated logs.
- Nothing read from the indexer may gate a signing decision. Any
  state that influences a transaction (balances, role membership,
  pending withdrawals) is re-fetched on-chain through the wallet RPC
  immediately before the sign prompt.
- The wallet's sign prompt is the canonical authorization surface,
  showing calldata derived from on-chain state, not indexer output.

### 5. Strict CSP, zero third-party runtime

- `Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self' <wallet only>;`
- No analytics, no telemetry, no remote fonts, no CDN-hosted libs.
- A bundle that loads zero external resources at runtime cannot be
  poisoned post-release.

### 6. Hardware-wallet-first UX for admin roles

- Admin-pauser-revoker flows surface guidance to use a hardware
  signer (Ledger / Trezor) and warn on hot-key signers.
- Combined with on-chain RBAC, a hot-key compromise on a developer
  machine cannot reach the privileged functions.

### 7. Reproducible build pipeline

- CI publishes the IPFS CID alongside the git tag.
- Anyone (auditor, user, ops) can clone the tag, run the build, and
  confirm the CID matches. Mismatch = compromise.

---

## Threats neutralised

| Threat | Mitigation |
|---|---|
| Hoster swaps the bundle | CID pinned in ENS; users can verify against tagged commit |
| Hoster swaps `VITE_FORK_RPC_URL` to a hostile RPC | No dapp-owned RPC exists; reads go through wallet RPC |
| Hostile RPC lies about state to mislead signer | Pre-sign state re-read through user RPC; sign prompt shows calldata; bytecode hash pinned |
| Hostile RPC swaps the gateway implementation | `VITE_GATEWAY_EXPECTED_CODE_HASH` mismatch fails closed |
| Indexer DB compromise | Indexer output is display-only; signing decisions ignore it |
| Runtime script injection (XSS / CDN compromise) | Strict CSP; no third-party scripts; no remote eval |
| Stolen developer hot key | Hardware-wallet UX + on-chain RBAC for admin functions |
| Phishing clone of the dapp | ENS contenthash + reproducible builds give users a verifiable canonical CID |

---

## Threats out of scope

- **Wallet compromise.** If the user's wallet or RPC is owned, this
  topology offers no protection — the wallet is the trust root.
- **Chain-level reorgs / consensus failure.** Application-layer
  concern only; not addressed here.
- **Social engineering of the signer.** The wallet prompt shows
  calldata; user-side discipline is required.

---

## Trade-offs

- **Latency / capability.** Routing all reads through the wallet
  RPC is slower and feature-limited (no archive queries, no
  high-volume log scans). The indexer covers the rich-display gap,
  but cannot influence intent.
- **No hosted backend means no server-side rate limiting / auth.**
  All access control collapses onto the chain. This is by design.
- **Operator UX.** Operators must understand that the dapp is
  intentionally thin and that *they* are responsible for their RPC
  choice and signer hygiene.

---

## Current gap vs. target

Status of the migration toward this topology:

1. ~~Migrate the dapp's read calls onto the injected EIP-1193 provider.~~
   **Done.** `clients/dapp/src/lib/wagmi.ts` uses
   `unstable_connector(injected)` as the foundry-chain transport; all
   reads on the devnet/operator chain route through the wallet RPC.
2. ~~Remove `VITE_FORK_RPC_URL` from the build matrix.~~ **Done.**
   Stripped from `clients/dapp/Dockerfile`, the dapp's `docker-compose.yml`,
   `testing/ethereum-testnet/config/docker-compose.dapp.yaml`, the
   smoke-test harness, and `.github/workflows/release-dapp.yml`. The
   bundle no longer has a hoster-controlled RPC endpoint.
3. Add CI publication of the IPFS CID + sigstore attestation.
4. Configure ENS contenthash for the canonical name.
5. Tighten CSP to forbid `connect-src` outside the wallet bridge.

Open items (3–5) are independent of each other.

The `rmpc` config export panel still surfaces an `rpcUrl` field, but
that URL is for the off-chain `rmpc` client the operator runs
themselves (it lives in the downloaded TOML, not in any browser fetch
the dapp performs). The field is now operator-editable in the UI with
a `http://127.0.0.1:8545` default, instead of being baked at build
time — see `clients/dapp/src/components/ConfigExportPanel.tsx`.

---

## Out-of-scope: `smoke-test --tunnel`

The `smoke-test --tunnel` flag is **explicitly not** part of this
topology. It bakes hoster-controlled `https://*.trycloudflare.com`
URLs into `VITE_FORK_RPC_URL` and `VITE_EXPLORER_API_URL` at build
time so a hosted devnet is reachable from a remote browser. Each
invocation produces a different bundle (the tunnel URLs are random
per session), so it cannot be reproducible, content-addressed, or
free of hoster-controlled network endpoints — i.e. it violates §1,
§2, §4, and §7 by design.

It exists as a developer affordance for demos, recorded walkthroughs,
and reviewer access. It is not a production hosting pattern and the
topology above remains the canonical target. The migration of reads
onto the wallet provider (§2) is what eventually retires the need
for any hoster-owned RPC URL, including the tunnel one.
