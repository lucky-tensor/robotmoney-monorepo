// Canonical: docs/architecture.md §5.3 — Human Dapp (Vite entry point)

/**
 * Vite entry point. Installs global error capture, then hands off to the
 * async startup gate in `bootstrap.tsx`, which fetches `/config.json` and
 * renders a loading state while it is in flight (issue #1356).
 *
 * This file deliberately contains no logic beyond wiring: it executes on
 * import and so cannot be unit-tested. `import.meta.env` is read here only
 * to supply the build-time base layer the fetched config overlays — the
 * contract addresses, env class, and endpoints themselves are no longer
 * resolved at module scope. See `bootstrap.tsx` and `lib/runtimeConfig.ts`.
 */
import "./styles.css";
import ReactDOM from "react-dom/client";
import { DappRoot, bootstrapDapp } from "./bootstrap";

// Install global error capture before React renders so startup errors are
// included in the /debug feed.
import { initErrorCapture } from "./lib/error-capture";

initErrorCapture();

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("#root element missing from index.html");

void bootstrapDapp(ReactDOM.createRoot(rootEl), {
  buildEnv: import.meta.env as Record<string, string | undefined>,
  fetchImpl: (input, init) => fetch(input, init),
  renderApp: (config) => <DappRoot config={config} />,
});
