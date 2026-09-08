import { defineConfig } from "vitest/config";
import { playwright } from "@vitest/browser-playwright";
import react from "@vitejs/plugin-react";

const testDefines = {
  __DAPP_VERSION__: JSON.stringify("0.1.0"),
  __GIT_COMMIT__: JSON.stringify("test-commit"),
};

export default defineConfig({
  plugins: [react()],
  define: testDefines,
  test: {
    projects: [
      {
        extends: true,
        test: {
          name: "browser",
          globals: true,
          fileParallelism: false,
          browser: {
            enabled: true,
            provider: playwright(),
            instances: [{ browser: "chromium", headless: true }],
          },
          setupFiles: ["./tests/unit/setup.ts"],
          include: ["tests/unit/**/*.{test,spec}.{ts,tsx}"],
          exclude: [
            "tests/e2e/**",
            "node_modules/**",
            "tests/unit/env-example-warning.test.ts",
            "tests/unit/faucet-shared-amount.test.ts",
            "tests/unit/csp.test.ts",
            "tests/unit/router-deposit-no-unguarded-vault.test.ts",
            "tests/unit/e2e-gas-buffer.test.ts",
          ],
        },
      },
      {
        extends: true,
        test: {
          name: "node",
          globals: true,
          environment: "node",
          setupFiles: ["./tests/unit/setup.node.ts"],
          include: [
            "tests/unit/env-example-warning.test.ts",
            "tests/unit/faucet-shared-amount.test.ts",
            "tests/unit/csp.test.ts",
            "tests/unit/router-deposit-no-unguarded-vault.test.ts",
            "tests/unit/e2e-gas-buffer.test.ts",
          ],
        },
      },
    ],
  },
});
