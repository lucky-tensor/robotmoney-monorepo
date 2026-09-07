/**
 * Async startup gate — issue #1356.
 *
 * The dapp used to render synchronously from build-time-inlined constants.
 * It now renders a loading state, awaits `/config.json`, and only then
 * renders the app with the fetched values. These tests pin that sequence and
 * the failure branch, because the failure branch is what stands between a
 * broken deploy and a blank page.
 */
import { act } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import ReactDOM from "react-dom/client";
import {
  bootstrapDapp,
  deriveDappConfig,
  type BootstrapDeps,
  type DappConfig,
} from "../../src/bootstrap";
import type { ConfigFetchLike, RuntimeConfig } from "../../src/lib/runtimeConfig";

const BUILD_ENV: RuntimeConfig = {
  VITE_GATEWAY_ADDRESS: "0x1111111111111111111111111111111111111111",
  VITE_ENV_CLASS: "fork",
  VITE_HISTORY_PANE: "false",
  VITE_FAUCET_HARNESS_PRIVATE_KEY: `0x${"a".repeat(64)}`,
};

const FETCHED = {
  VITE_GATEWAY_ADDRESS: "0x2222222222222222222222222222222222222222",
  VITE_VAULT_ADDRESS: "0x3333333333333333333333333333333333333333",
  VITE_REGISTRY_ADDRESS: "0x4444444444444444444444444444444444444444",
  VITE_ENV_CLASS: "devnet",
  VITE_EXPLORER_API_URL: "https://explorer.example",
  VITE_DEVNET_RPC_URL: "https://devnet.example/rpc",
} as const;

/**
 * Stand-in for `DappRoot`. `bootstrapDapp` takes `renderApp` as a dependency
 * precisely so the config handed downstream can be inspected as rendered
 * props without mounting the chain-connected tree.
 */
let lastProps: DappConfig | undefined;

function ConfigProbe({ config }: { readonly config: RuntimeConfig }) {
  const cfg = deriveDappConfig(config);
  lastProps = cfg;
  return (
    <div
      data-testid="config-probe"
      data-gateway={cfg.gateway}
      data-vault={cfg.vault}
      data-env-class={cfg.envClass}
      data-explorer={cfg.explorerApiUrl}
    />
  );
}

interface Harness {
  readonly container: HTMLElement;
  readonly root: ReactDOM.Root;
}

const mounted: Harness[] = [];

function mount(): Harness {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const harness = { container, root: ReactDOM.createRoot(container) };
  mounted.push(harness);
  return harness;
}

function deps(fetchImpl: ConfigFetchLike): BootstrapDeps {
  return {
    buildEnv: BUILD_ENV,
    fetchImpl,
    renderApp: (config) => <ConfigProbe config={config} />,
  };
}

afterEach(() => {
  for (const { container, root } of mounted.splice(0)) {
    act(() => root.unmount());
    container.remove();
  }
  lastProps = undefined;
});

describe("bootstrapDapp — loading then render", () => {
  it("shows the loading state while the config fetch is in flight, then the app", async () => {
    let release: (payload: unknown) => void = () => undefined;
    const fetchImpl: ConfigFetchLike = () =>
      new Promise((resolve) => {
        release = (payload) =>
          resolve({ ok: true, status: 200, json: () => Promise.resolve(payload) });
      });

    const { container, root } = mount();
    let finished!: Promise<unknown>;
    await act(async () => {
      finished = bootstrapDapp(root, deps(fetchImpl));
    });

    // In flight: the loading state is on screen and the app is not.
    expect(container.querySelector('[data-testid="runtime-config-loading"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="config-probe"]')).toBeNull();

    await act(async () => {
      release(FETCHED);
      await finished;
    });

    // Resolved: the app replaced the loading state.
    expect(container.querySelector('[data-testid="runtime-config-loading"]')).toBeNull();
    const probe = container.querySelector('[data-testid="config-probe"]');
    expect(probe).not.toBeNull();
    expect(probe?.getAttribute("data-gateway")).toBe(FETCHED.VITE_GATEWAY_ADDRESS);
    expect(probe?.getAttribute("data-vault")).toBe(FETCHED.VITE_VAULT_ADDRESS);
    expect(probe?.getAttribute("data-env-class")).toBe("devnet");
    expect(probe?.getAttribute("data-explorer")).toBe(FETCHED.VITE_EXPLORER_API_URL);
  });

  it("hands the app the fetched addresses, not the build-time ones", async () => {
    const fetchImpl: ConfigFetchLike = () =>
      Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(FETCHED) });

    const { root } = mount();
    await act(async () => {
      await bootstrapDapp(root, deps(fetchImpl));
    });

    expect(lastProps?.gateway).toBe(FETCHED.VITE_GATEWAY_ADDRESS);
    expect(lastProps?.gateway).not.toBe(BUILD_ENV.VITE_GATEWAY_ADDRESS);
    expect(lastProps?.registry).toBe(FETCHED.VITE_REGISTRY_ADDRESS);
    expect(lastProps?.envClass).toBe("devnet");
    // Build-time-only values still reach the tree through the base layer.
    expect(lastProps?.env.VITE_FAUCET_HARNESS_PRIVATE_KEY).toBe(
      BUILD_ENV.VITE_FAUCET_HARNESS_PRIVATE_KEY,
    );
    expect(lastProps?.env.VITE_HISTORY_PANE).toBe("false");
  });

  it("renders the app from the build-time env when no config document is deployed", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const fetchImpl: ConfigFetchLike = () =>
      Promise.resolve({ ok: false, status: 404, json: () => Promise.resolve({}) });

    const { container, root } = mount();
    let source: unknown;
    await act(async () => {
      source = await bootstrapDapp(root, deps(fetchImpl));
    });

    expect(source).toBe("build-time");
    expect(container.querySelector('[data-testid="config-probe"]')).not.toBeNull();
    expect(lastProps?.gateway).toBe(BUILD_ENV.VITE_GATEWAY_ADDRESS);
  });
});

describe("bootstrapDapp — visible failure", () => {
  it("renders an error panel instead of the app when the config fetch fails", async () => {
    const fetchImpl: ConfigFetchLike = () =>
      Promise.resolve({ ok: false, status: 500, json: () => Promise.resolve({}) });

    const { container, root } = mount();
    let source: unknown = "unset";
    await act(async () => {
      source = await bootstrapDapp(root, deps(fetchImpl));
    });

    expect(source).toBeUndefined();
    expect(container.querySelector('[data-testid="runtime-config-loading"]')).toBeNull();
    expect(container.querySelector('[data-testid="config-probe"]')).toBeNull();
    const error = container.querySelector('[data-testid="runtime-config-error"]');
    expect(error).not.toBeNull();
    expect(error?.textContent).toContain("Configuration unavailable");
    expect(
      container.querySelector('[data-testid="runtime-config-error-detail"]')?.textContent,
    ).toContain("HTTP 500");
  });

  it("renders an error panel when the config document is malformed", async () => {
    const fetchImpl: ConfigFetchLike = () =>
      Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve("not-an-object") });

    const { container, root } = mount();
    await act(async () => {
      await bootstrapDapp(root, deps(fetchImpl));
    });

    expect(container.querySelector('[data-testid="config-probe"]')).toBeNull();
    expect(
      container.querySelector('[data-testid="runtime-config-error-detail"]')?.textContent,
    ).toContain("must contain a JSON object");
  });

  it("renders an error panel when the config document is unreachable", async () => {
    const fetchImpl: ConfigFetchLike = () => Promise.reject(new Error("NetworkError"));

    const { container, root } = mount();
    await act(async () => {
      await bootstrapDapp(root, deps(fetchImpl));
    });

    expect(container.querySelector('[data-testid="config-probe"]')).toBeNull();
    expect(
      container.querySelector('[data-testid="runtime-config-error-detail"]')?.textContent,
    ).toContain("NetworkError");
  });
});
