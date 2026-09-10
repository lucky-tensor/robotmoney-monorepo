// Verifies the strict Content Security Policy (issue #665): no inline scripts,
// no eval, and a meta tag injected into the built index.html.
import { describe, it, expect } from "vitest";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { CSP_POLICY, CSP_META_POLICY, cspPlugin } from "../../src/lib/csp";

describe("CSP policy", () => {
  it("defines script-src 'self' without unsafe-inline or unsafe-eval", () => {
    expect(CSP_POLICY).toMatch(/script-src[^;]*'self'/);
    const scriptSrc = CSP_POLICY.split(";")
      .map((d) => d.trim())
      .find((d) => d.startsWith("script-src"));
    expect(scriptSrc).toBeDefined();
    expect(scriptSrc).not.toMatch(/unsafe-inline/);
    expect(scriptSrc).not.toMatch(/unsafe-eval/);
  });

  it("never allows 'unsafe-eval' in any directive", () => {
    expect(CSP_POLICY).not.toMatch(/unsafe-eval/);
  });

  it("locks down object-src, base-uri, and frame-ancestors", () => {
    expect(CSP_POLICY).toContain("object-src 'none'");
    expect(CSP_POLICY).toContain("base-uri 'self'");
    expect(CSP_POLICY).toContain("frame-ancestors 'none'");
  });

  it("allows the full-stack devnet receipt fixture origin", () => {
    expect(CSP_POLICY).toContain("http://receipt-fixtures:8097");
  });

  it("injects a CSP meta tag via transformIndexHtml", () => {
    const plugin = cspPlugin();
    const transform = plugin.transformIndexHtml;
    const hook = typeof transform === "function" ? transform : undefined;
    expect(hook).toBeDefined();
    const result = (
      hook as (
        html: string,
        ctx: unknown,
      ) => { tags?: Array<{ tag: string; attrs?: Record<string, string> }> }
    )("<html><head></head><body></body></html>", {
      path: "/",
      filename: "index.html",
    });
    const tags = result.tags ?? [];
    const meta = tags.find(
      (t) => t.tag === "meta" && t.attrs?.["http-equiv"] === "Content-Security-Policy",
    );
    expect(meta).toBeDefined();
    // The meta tag carries the meta-safe policy: browsers ignore (and log a
    // console error for) frame-ancestors in a <meta> element, so it is excluded
    // here and enforced via the HTTP header instead (issue #665 / dapp-e2e).
    expect(meta?.attrs?.content).toBe(CSP_META_POLICY);
    expect(meta?.attrs?.content).not.toContain("frame-ancestors");
    expect(meta?.attrs?.content).toContain("script-src 'self'");
  });

  it("keeps frame-ancestors in the header policy but not the meta policy", () => {
    expect(CSP_POLICY).toContain("frame-ancestors 'none'");
    expect(CSP_META_POLICY).not.toContain("frame-ancestors");
  });

  it("emits the CSP meta tag into the production build output", () => {
    const distIndex = fileURLToPath(new URL("../../dist/index.html", import.meta.url));
    // The production build is a REQUIRED resource for this assertion, not an
    // optional one: `.github/workflows/suite-09-dapp-quality.yml` runs its
    // `Production build` step immediately before `Vitest` so dist/index.html
    // exists here. Fail loudly rather than returning early — a silent no-op
    // reported this test green while executing zero assertions (issue #1383).
    if (!existsSync(distIndex)) {
      throw new Error(
        `dist/index.html is missing at ${distIndex}. Run \`bun run build\` before ` +
          `\`bun run test\` (CI does this in the Production build step).`,
      );
    }
    const html = readFileSync(distIndex, "utf8");
    expect(html).toMatch(/http-equiv="Content-Security-Policy"/);
    // Scope the unsafe-inline check to the script-src directive only: the policy
    // legitimately allows 'unsafe-inline' for style-src, so a directive-blind
    // match would false-positive across the ';' separator.
    expect(html).not.toMatch(/script-src[^";]*unsafe-inline/);
    expect(html).not.toMatch(/unsafe-eval/);
  });
});
