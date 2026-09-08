/*
 * Enforces rules from docs/development/react-guide.md.
 * Any rule below maps to a section there; do not loosen without updating both.
 */
module.exports = {
  root: true,
  env: { browser: true, es2022: true, node: true },
  parser: "@typescript-eslint/parser",
  parserOptions: { ecmaVersion: "latest", sourceType: "module" },
  plugins: ["@typescript-eslint", "react", "react-hooks", "react-refresh"],
  extends: [
    "eslint:recommended",
    "plugin:@typescript-eslint/recommended",
    "plugin:react/recommended",
    "plugin:react/jsx-runtime",
  ],
  settings: { react: { version: "detect" } },
  ignorePatterns: ["dist", "node_modules", ".eslintrc.cjs"],
  rules: {
    // Types (guide §Types)
    "@typescript-eslint/no-explicit-any": "error",
    "@typescript-eslint/no-non-null-assertion": "error",
    "@typescript-eslint/ban-ts-comment": [
      "error",
      {
        "ts-ignore": true,
        "ts-nocheck": true,
        "ts-expect-error": "allow-with-description",
      },
    ],
    "@typescript-eslint/consistent-type-assertions": [
      "error",
      {
        assertionStyle: "as",
        objectLiteralTypeAssertions: "never",
      },
    ],
    "@typescript-eslint/no-unused-vars": ["error", { argsIgnorePattern: "^_" }],

    // React (guide §Components, §Rendering)
    "react/no-array-index-key": "error",
    "react/no-danger": "error",
    "react/no-unstable-nested-components": ["error", { allowAsProps: false }],
    "react/jsx-no-target-blank": ["error", { allowReferrer: false, enforceDynamicLinks: "always" }],
    "react/button-has-type": "error",
    "react/jsx-no-useless-fragment": "error",
    "react/self-closing-comp": "error",
    "react/prop-types": "off",

    // Hooks (guide §State & data)
    "react-hooks/rules-of-hooks": "error",
    "react-hooks/exhaustive-deps": "error",

    // Components / banned patterns (guide §Components, §Dependencies)
    "no-restricted-syntax": [
      "error",
      {
        selector: "TSEnumDeclaration",
        message: "Use string-literal unions, not enums (react-guide §Types).",
      },
      {
        selector: "ExportDefaultDeclaration",
        message: "Named exports only (react-guide §Components).",
      },
      {
        selector: "TSTypeReference[typeName.name='FC']",
        message: "Do not use React.FC (react-guide §Components).",
      },
      {
        selector: "TSQualifiedName[left.name='React'][right.name='FC']",
        message: "Do not use React.FC (react-guide §Components).",
      },
      {
        selector: "JSXAttribute[name.name='dangerouslySetInnerHTML']",
        message: "dangerouslySetInnerHTML is banned (react-guide §Components).",
      },
      {
        selector: "CallExpression[callee.object.name='Math'][callee.property.name='random']",
        message: "No Math.random in render path; inject (react-guide §Prime directives).",
      },
      {
        selector: "CallExpression[callee.object.name='Date'][callee.property.name='now']",
        message: "No Date.now in render path; inject (react-guide §Prime directives).",
      },
    ],

    // Dependencies (guide §Dependencies — banned)
    "no-restricted-imports": [
      "error",
      {
        paths: [
          {
            name: "lodash",
            message: "Banned; write the one fn in lib/ (react-guide §Dependencies).",
          },
          {
            name: "date-fns",
            message: "Banned; write the one fn in lib/ (react-guide §Dependencies).",
          },
          {
            name: "redux",
            message: "Banned; use TanStack Query + useState (react-guide §Dependencies).",
          },
          { name: "@reduxjs/toolkit", message: "Banned (react-guide §Dependencies)." },
          { name: "zustand", message: "Banned (react-guide §Dependencies)." },
          { name: "jotai", message: "Banned (react-guide §Dependencies)." },
          { name: "mobx", message: "Banned (react-guide §Dependencies)." },
          {
            name: "react-hook-form",
            message: "Banned; use native <form> (react-guide §Dependencies).",
          },
          { name: "formik", message: "Banned; use native <form> (react-guide §Dependencies)." },
          { name: "react-router", message: "Banned without an issue (react-guide §Dependencies)." },
          {
            name: "react-router-dom",
            message: "Banned without an issue (react-guide §Dependencies).",
          },
          { name: "@mui/material", message: "No UI kits (react-guide §Dependencies)." },
          { name: "@chakra-ui/react", message: "No UI kits (react-guide §Dependencies)." },
          { name: "@radix-ui/react", message: "No UI kits (react-guide §Dependencies)." },
          { name: "framer-motion", message: "No animation libs (react-guide §Dependencies)." },
        ],
        patterns: [
          { group: ["lodash/*"], message: "Banned (react-guide §Dependencies)." },
          {
            group: ["@mui/*", "@chakra-ui/*", "@radix-ui/*", "@headlessui/*"],
            message: "No UI kits (react-guide §Dependencies).",
          },
        ],
      },
    ],
  },
  overrides: [
    {
      // Playwright requires a default export for globalSetup.
      // Vitest workspace/config files also use default exports (Vitest convention).
      files: [
        "tests/e2e/**/*.ts",
        "playwright.config.ts",
        "vite.config.ts",
        "vitest.config.ts",
        "vitest.workspace.ts",
      ],
      rules: { "no-restricted-syntax": "off" },
    },
    {
      // A spec that signs a devnet transaction directly bypasses both existing
      // 1.5x gas buffers (helpers/wallet.ts, Fixture::cast_send), and viem
      // forwards a bare `eth_estimateGas` result verbatim — the smallest limit
      // at which the OUTERMOST frame succeeds, hence zero usable margin for
      // anything nested under EIP-150's 63/64 rule. Route it through
      // helpers/gas.ts instead. Issue #1388; docs/testing/geth-gas-estimation.md.
      files: ["tests/e2e/**/*.spec.ts"],
      rules: {
        "no-restricted-syntax": [
          "error",
          {
            selector: "CallExpression[callee.property.name='sendTransaction']",
            message:
              "Do not call walletClient.sendTransaction() in an e2e spec — it forwards an " +
              "unbuffered eth_estimateGas result as the gas limit. Use " +
              "sendBufferedTransaction() from ./helpers/gas (issue #1388, " +
              "docs/testing/geth-gas-estimation.md).",
          },
        ],
      },
    },
    {
      files: ["tests/**/*.{ts,tsx}"],
      rules: {
        "@typescript-eslint/no-non-null-assertion": "off",
      },
    },
  ],
};
