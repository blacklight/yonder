import { describe, expect, it } from "vitest";
import { highlightCodeBlocks } from "./highlight";

describe("highlightCodeBlocks", () => {
  it("highlights a fenced block by its language class", () => {
    const out = highlightCodeBlocks(
      `<p>hi</p><pre><code class="language-python">def f():\n    return 1</code></pre>`,
    );
    expect(out).toContain("hljs-keyword");
    expect(out).toContain("language-python");
    // Inline code outside <pre> is untouched.
    expect(out).toContain("<p>hi</p>");
  });

  it("auto-detects blocks without a language", () => {
    const out = highlightCodeBlocks(
      `<pre><code>SELECT * FROM users;</code></pre>`,
    );
    expect(out).toContain("hljs");
  });

  it("leaves html without pre blocks untouched", () => {
    const html = "<p>just <code>inline</code> text</p>";
    expect(highlightCodeBlocks(html)).toBe(html);
  });

  it("escapes code content instead of injecting markup", () => {
    const out = highlightCodeBlocks(
      `<pre><code class="language-html">&lt;script&gt;alert(1)&lt;/script&gt;</code></pre>`,
    );
    expect(out).not.toContain("<script>");
    expect(out).toContain("&lt;");
  });
});
