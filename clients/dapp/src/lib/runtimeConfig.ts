// Canonical: docs/architecture.md §5.3 — Human Dapp (runtime configuration)

/**
 * Runtime configuration fetch (issue #1356, mirrored from robotmoney/devops#5).
 *
 * ## Why this module exists
 *
 * Vite inlines every `import.meta.env.VITE_*` read into the bundle at build
 * time. Because the dapp image is commit-hash-addressed, per-environment
 * images are not viable — so a bundle built once could only ever point at
 * one set of contract addresses. This module moves the *deployment-shaped*
 * values (contract addresses, env class, RPC/explorer endpoints) into a
 * `/config.json` document fetched at startup, so one image serves every
 * environment.
 *
 * Authoring/serving `/config.json` is the stack tooling's job and is
 * deliberately out of scope here — this module only defines how the client
 * reads it.
 *
 * ## Layering
 *
 * The build-time env map remains the *base* layer; the fetched document is
 * an overlay on top of it. That keeps `bun run dev`, `vite preview`, and the
 * existing smoke-test images working unchanged when no `/config.json` is
 * served (HTTP 404 → fall back to the build-time values).
 *
 * ## Security — why the overlay is a strict allowlist
 *
 * `/config.json` is world-readable by anyone who can load the dapp, and it
 * is attacker-visible in a way a compiled bundle constant is not. Only the
 * keys in `RUNTIME_CONFIG_KEYS` are ever taken from it. Two exclusions are
 * load-bearing rather than incidental:
 *
 *   - `VITE_FAUCET_HARNESS_PRIVATE_KEY` stays build-time-only. Serving a
 *     private key from a fetched document would publish it to every
 *     requester of that URL; `buildEnvValidation.ts` already refuses to bake
 *     it into a mainnet-class bundle, and that guard would be meaningless if
 *     a runtime document could reintroduce the key.
 *   - `VITE_HISTORY_PANE` stays build+ADR-only. `featureFlags.ts` documents
 *     that flipping it must require a rebuild and an ADR, so no runtime
 *     toggle path is offered.
 *
 * Keys outside the allowlist are dropped with a warning rather than merged,
 * so a mistaken or hostile `/config.json` cannot reach either surface.
 */

/**
 * Deployment configuration as an env-shaped record. Env shape (rather than a
 * bespoke interface) is deliberate: `resolveFlags`, `resolveExplorerApiUrl`,
 * `readHarnessPrivateKey`, and `parseVaultAddressMap` already accept
 * `Record<string, string | undefined>`, so the merged config drops straight
 * into every existing consumer.
 */
export type RuntimeConfig = Readonly<Record<string, string | undefined>>;

/**
 * The only keys the fetched document may supply. Anything else — including
 * the two by-design build-time-only variables — is ignored. See the security
 * note in the module doc before adding to this list.
 */
export const RUNTIME_CONFIG_KEYS = [
  "VITE_GATEWAY_ADDRESS",
  "VITE_VAULT_ADDRESS",
  "VITE_REGISTRY_ADDRESS",
  "VITE_ROUTER_ADDRESS",
  "VITE_GOVERNANCE_ADDRESS",
  "VITE_TIMELOCK_ADDRESS",
  "VITE_RM_TOKEN_ADDRESS",
  "VITE_GATEWAY_EXPECTED_CODE_HASH",
  "VITE_ENV_CLASS",
  "VITE_VAULT_ADDRESSES",
  "VITE_DEVNET_RPC_URL",
  "VITE_EXPLORER_API_URL",
] as const;

/** URL the runtime config is served from, relative to the dapp origin. */
export const RUNTIME_CONFIG_URL = "/config.json";

/**
 * Raised when `/config.json` was served but could not be used. Distinct from
 * "absent", which is a supported deployment shape — see `loadRuntimeConfig`.
 */
export class RuntimeConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RuntimeConfigError";
  }
}

/** Where the effective config came from. Surfaced for the /debug view and tests. */
export type RuntimeConfigSource = "fetched" | "build-time";

export interface LoadedRuntimeConfig {
  readonly config: RuntimeConfig;
  readonly source: RuntimeConfigSource;
}

/**
 * Minimum `fetch` surface this module depends on — mirrors `explorerApi.ts`'s
 * `FetchLike` so unit tests inject a mock instead of patching global fetch.
 */
export type ConfigFetchLike = (
  input: string,
  init?: { cache?: RequestCache },
) => Promise<{ ok: boolean; status: number; json: () => Promise<unknown> }>;

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Validate a decoded `/config.json` payload and reduce it to the allowlisted
 * string entries.
 *
 * Throws `RuntimeConfigError` when the payload is not a JSON object or when
 * an allowlisted key carries a non-string value. Failing loudly is the point:
 * a malformed document means a broken deploy, and a half-configured dapp that
 * silently falls back to zero addresses is worse than a visible error.
 *
 * `null`/`undefined` values are treated as "not specified" and simply omitted,
 * so an operator can template a key without setting it. Empty strings *are*
 * kept — clearing `VITE_DEVNET_RPC_URL` is a meaningful instruction.
 */
export function parseRuntimeConfig(payload: unknown): RuntimeConfig {
  if (!isPlainObject(payload)) {
    throw new RuntimeConfigError(
      `${RUNTIME_CONFIG_URL} must contain a JSON object, got ${
        Array.isArray(payload) ? "an array" : typeof payload
      }.`,
    );
  }

  const allowed = new Set<string>(RUNTIME_CONFIG_KEYS);
  const parsed: Record<string, string> = {};
  const rejected: string[] = [];

  for (const [key, value] of Object.entries(payload)) {
    if (!allowed.has(key)) {
      rejected.push(key);
      continue;
    }
    if (value === null || value === undefined) continue;
    if (typeof value !== "string") {
      throw new RuntimeConfigError(
        `${RUNTIME_CONFIG_URL} key ${key} must be a string, got ${typeof value}.`,
      );
    }
    parsed[key] = value;
  }

  if (rejected.length > 0) {
    // console.warn, not console.error: an unknown key is a deployment smell
    // worth reporting but not a reason to refuse to start. The values are
    // already discarded by the time we get here.
    console.warn(
      `${RUNTIME_CONFIG_URL}: ignoring key(s) outside the runtime allowlist: ${rejected.join(", ")}. ` +
        "Build-time-only variables (the faucet harness key, the history-pane flag) " +
        "are never read from the runtime config by design.",
    );
  }

  return parsed;
}

/**
 * Fetch `/config.json` and overlay it on the build-time env map.
 *
 * Outcomes:
 *   - 2xx with a valid object → `source: "fetched"`, overlay applied.
 *   - 404 → `source: "build-time"`. No config document is deployed (local
 *     `bun run dev`, `vite preview`, existing single-environment images);
 *     the build-time values stand, exactly as they did before this module.
 *   - any other non-2xx, unreadable JSON, or a payload that fails
 *     `parseRuntimeConfig` → throws `RuntimeConfigError`, so the bootstrap
 *     can render a visible error instead of a blank or half-configured page.
 *   - a rejected fetch (network/CORS) → throws `RuntimeConfigError`. The
 *     document is same-origin, so a transport failure means the deployment
 *     is broken, not that the config is absent.
 */
export async function loadRuntimeConfig(args: {
  readonly fetchImpl: ConfigFetchLike;
  readonly buildEnv: RuntimeConfig;
  readonly url?: string;
}): Promise<LoadedRuntimeConfig> {
  const url = args.url ?? RUNTIME_CONFIG_URL;

  let response: Awaited<ReturnType<ConfigFetchLike>>;
  try {
    response = await args.fetchImpl(url, { cache: "no-store" });
  } catch (err) {
    throw new RuntimeConfigError(
      `Could not fetch ${url}: ${(err as { message?: string }).message ?? String(err)}`,
    );
  }

  if (response.status === 404) {
    console.warn(
      `${url} is not served by this deployment — falling back to the build-time environment.`,
    );
    return { config: args.buildEnv, source: "build-time" };
  }

  if (!response.ok) {
    throw new RuntimeConfigError(`Could not fetch ${url}: HTTP ${response.status}.`);
  }

  let payload: unknown;
  try {
    payload = await response.json();
  } catch (err) {
    throw new RuntimeConfigError(
      `${url} is not valid JSON: ${(err as { message?: string }).message ?? String(err)}`,
    );
  }

  return {
    config: { ...args.buildEnv, ...parseRuntimeConfig(payload) },
    source: "fetched",
  };
}
