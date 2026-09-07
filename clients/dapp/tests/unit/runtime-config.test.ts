/**
 * Runtime config fetch — issue #1356.
 *
 * Covers the three outcomes the dapp must distinguish when it asks a
 * deployment what contracts to talk to:
 *   - the document is served and valid → its values overlay the build-time env,
 *   - the document is absent (404) → the build-time env stands (dev server,
 *     `vite preview`, existing single-environment images),
 *   - the document is served but unusable → a loud failure, never a silent
 *     half-configured app.
 *
 * Also pins the security property that makes the runtime config safe to
 * serve publicly: the faucet harness private key and the history-pane flag
 * can never be introduced through it.
 */
import { describe, expect, it, vi } from "vitest";
import {
  loadRuntimeConfig,
  parseRuntimeConfig,
  RuntimeConfigError,
  RUNTIME_CONFIG_KEYS,
  RUNTIME_CONFIG_URL,
  type ConfigFetchLike,
} from "../../src/lib/runtimeConfig";

const BUILD_ENV = {
  VITE_GATEWAY_ADDRESS: "0x1111111111111111111111111111111111111111",
  VITE_ENV_CLASS: "fork",
  VITE_HISTORY_PANE: "false",
  VITE_FAUCET_HARNESS_PRIVATE_KEY: `0x${"a".repeat(64)}`,
} as const;

/** A fetch that returns one canned response, recording what it was asked for. */
function stubFetch(response: { ok?: boolean; status?: number; json?: () => Promise<unknown> }): {
  fetchImpl: ConfigFetchLike;
  calls: string[];
} {
  const calls: string[] = [];
  const fetchImpl: ConfigFetchLike = (input) => {
    calls.push(input);
    return Promise.resolve({
      ok: response.ok ?? true,
      status: response.status ?? 200,
      json: response.json ?? (() => Promise.resolve({})),
    });
  };
  return { fetchImpl, calls };
}

describe("loadRuntimeConfig — served and valid", () => {
  it("overlays the fetched values on the build-time env", async () => {
    const { fetchImpl, calls } = stubFetch({
      json: () =>
        Promise.resolve({
          VITE_GATEWAY_ADDRESS: "0x2222222222222222222222222222222222222222",
          VITE_VAULT_ADDRESS: "0x3333333333333333333333333333333333333333",
          VITE_ENV_CLASS: "devnet",
          VITE_DEVNET_RPC_URL: "https://devnet.example/rpc",
          VITE_EXPLORER_API_URL: "https://explorer.example",
        }),
    });

    const { config, source } = await loadRuntimeConfig({ fetchImpl, buildEnv: BUILD_ENV });

    expect(calls).toEqual([RUNTIME_CONFIG_URL]);
    expect(source).toBe("fetched");
    // Fetched values win over the build-time ones …
    expect(config.VITE_GATEWAY_ADDRESS).toBe("0x2222222222222222222222222222222222222222");
    expect(config.VITE_ENV_CLASS).toBe("devnet");
    expect(config.VITE_VAULT_ADDRESS).toBe("0x3333333333333333333333333333333333333333");
    expect(config.VITE_DEVNET_RPC_URL).toBe("https://devnet.example/rpc");
    // … and build-time-only values are still there underneath.
    expect(config.VITE_FAUCET_HARNESS_PRIVATE_KEY).toBe(BUILD_ENV.VITE_FAUCET_HARNESS_PRIVATE_KEY);
  });

  it("accepts an empty string as an explicit instruction to clear a value", async () => {
    const { fetchImpl } = stubFetch({
      json: () => Promise.resolve({ VITE_DEVNET_RPC_URL: "" }),
    });

    const { config } = await loadRuntimeConfig({
      fetchImpl,
      buildEnv: { ...BUILD_ENV, VITE_DEVNET_RPC_URL: "https://stale.example" },
    });

    expect(config.VITE_DEVNET_RPC_URL).toBe("");
  });

  it("treats a null value as unspecified and keeps the build-time value", async () => {
    const { fetchImpl } = stubFetch({
      json: () => Promise.resolve({ VITE_GATEWAY_ADDRESS: null }),
    });

    const { config } = await loadRuntimeConfig({ fetchImpl, buildEnv: BUILD_ENV });

    expect(config.VITE_GATEWAY_ADDRESS).toBe(BUILD_ENV.VITE_GATEWAY_ADDRESS);
  });

  it("requests the config with cache disabled so a redeploy is picked up", async () => {
    const seen: (RequestCache | undefined)[] = [];
    const fetchImpl: ConfigFetchLike = (_input, init) => {
      seen.push(init?.cache);
      return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({}) });
    };

    await loadRuntimeConfig({ fetchImpl, buildEnv: BUILD_ENV });

    expect(seen).toEqual(["no-store"]);
  });
});

describe("loadRuntimeConfig — document absent", () => {
  it("falls back to the build-time env on 404", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { fetchImpl } = stubFetch({ ok: false, status: 404 });

    const { config, source } = await loadRuntimeConfig({ fetchImpl, buildEnv: BUILD_ENV });

    expect(source).toBe("build-time");
    expect(config).toEqual(BUILD_ENV);
    expect(warn).toHaveBeenCalled();
  });
});

describe("loadRuntimeConfig — served but unusable", () => {
  it("throws on a non-404 error status rather than falling back", async () => {
    const { fetchImpl } = stubFetch({ ok: false, status: 500 });

    await expect(loadRuntimeConfig({ fetchImpl, buildEnv: BUILD_ENV })).rejects.toBeInstanceOf(
      RuntimeConfigError,
    );
  });

  it("throws when the body is not valid JSON", async () => {
    const { fetchImpl } = stubFetch({
      json: () => Promise.reject(new Error("Unexpected token < in JSON at position 0")),
    });

    await expect(loadRuntimeConfig({ fetchImpl, buildEnv: BUILD_ENV })).rejects.toThrowError(
      /not valid JSON/,
    );
  });

  it("throws when a transport failure prevents the fetch", async () => {
    const fetchImpl: ConfigFetchLike = () => Promise.reject(new Error("NetworkError"));

    await expect(loadRuntimeConfig({ fetchImpl, buildEnv: BUILD_ENV })).rejects.toThrowError(
      /Could not fetch/,
    );
  });

  it("throws when the payload is not a JSON object", async () => {
    const { fetchImpl } = stubFetch({ json: () => Promise.resolve(["not", "an", "object"]) });

    await expect(loadRuntimeConfig({ fetchImpl, buildEnv: BUILD_ENV })).rejects.toThrowError(
      /must contain a JSON object/,
    );
  });
});

describe("parseRuntimeConfig", () => {
  it("rejects a non-string value for an allowlisted key", () => {
    expect(() => parseRuntimeConfig({ VITE_ENV_CLASS: 3 })).toThrowError(/must be a string/);
  });

  it("rejects null and array payloads", () => {
    expect(() => parseRuntimeConfig(null)).toThrowError(RuntimeConfigError);
    expect(() => parseRuntimeConfig([])).toThrowError(/an array/);
  });

  it("accepts every documented runtime key", () => {
    const payload = Object.fromEntries(RUNTIME_CONFIG_KEYS.map((k) => [k, `value-for-${k}`]));

    expect(parseRuntimeConfig(payload)).toEqual(payload);
  });

  // ── security ─────────────────────────────────────────────────────────────
  // /config.json is readable by anyone who can load the dapp. The allowlist is
  // what stops a mistaken or hostile document from publishing a private key or
  // from bypassing the build+ADR gate on the history pane.
  it("never admits the faucet harness private key from the runtime document", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);

    const parsed = parseRuntimeConfig({
      VITE_FAUCET_HARNESS_PRIVATE_KEY: `0x${"b".repeat(64)}`,
      VITE_ENV_CLASS: "devnet",
    });

    expect(parsed).toEqual({ VITE_ENV_CLASS: "devnet" });
    expect(parsed.VITE_FAUCET_HARNESS_PRIVATE_KEY).toBeUndefined();
    expect(RUNTIME_CONFIG_KEYS).not.toContain("VITE_FAUCET_HARNESS_PRIVATE_KEY");
    expect(warn).toHaveBeenCalled();
  });

  it("never admits the history-pane flag from the runtime document", () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);

    const parsed = parseRuntimeConfig({ VITE_HISTORY_PANE: "true" });

    expect(parsed).toEqual({});
    expect(RUNTIME_CONFIG_KEYS).not.toContain("VITE_HISTORY_PANE");
  });

  it("cannot override a build-time-only value through loadRuntimeConfig either", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { fetchImpl } = stubFetch({
      json: () =>
        Promise.resolve({
          VITE_HISTORY_PANE: "true",
          VITE_FAUCET_HARNESS_PRIVATE_KEY: `0x${"c".repeat(64)}`,
        }),
    });

    const { config } = await loadRuntimeConfig({ fetchImpl, buildEnv: BUILD_ENV });

    expect(config.VITE_HISTORY_PANE).toBe("false");
    expect(config.VITE_FAUCET_HARNESS_PRIVATE_KEY).toBe(BUILD_ENV.VITE_FAUCET_HARNESS_PRIVATE_KEY);
  });
});
