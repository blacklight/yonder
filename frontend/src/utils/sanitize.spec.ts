import { describe, expect, it } from "vitest";
import { sanitizeHtml } from "./sanitize";

describe("sanitizeHtml", () => {
  it("strips scripts, styles and event handlers", () => {
    const out = sanitizeHtml(
      `<p>hi<script>alert(1)</script><style>p{}</style>` +
        `<a href="https://x.test" onclick="evil()">link</a>` +
        `<img src="https://x.test/a.png" onerror="evil()">`,
    );
    expect(out).not.toContain("<script");
    expect(out).not.toContain("<style");
    expect(out).not.toContain("onclick");
    expect(out).not.toContain("onerror");
    expect(out).toContain("hi");
    expect(out).toContain("<img");
  });

  it("strips javascript: and data: hrefs", () => {
    const out = sanitizeHtml(
      `<a href="javascript:alert(1)">x</a><a href="data:text/html,<b>y</b>">y</a>`,
    );
    expect(out).not.toContain("javascript:");
    expect(out).not.toContain("data:");
  });

  it("keeps backend-style mention/hashtag anchors with rel+target", () => {
    // The server emits plain <a href> — no classes.
    const out = sanitizeHtml(
      `<p>hey <a href="https://remote.test/users/alice">@alice</a> ` +
        `<a href="https://x.test/tags/rust">#rust</a></p>`,
    );
    expect(out).toContain('href="https://remote.test/users/alice"');
    expect(out).toContain('rel="nofollow noopener"');
    expect(out).toContain('target="_blank"');
    expect(out).not.toContain("class=");
  });

  it("drops non-href attributes on anchors", () => {
    const out = sanitizeHtml(
      `<a href="https://x.test" class="u-url" target="_self" title="t">x</a>`,
    );
    expect(out).not.toContain("class=");
    expect(out).not.toContain("title=");
    expect(out).not.toContain('target="_self"');
  });

  it("keeps blockquote, code, lists, img", () => {
    const html =
      `<blockquote>q</blockquote><pre><code>x=1</code></pre>` +
      `<ul><li>a</li></ul><img src="/media/b.png" alt="b">`;
    const out = sanitizeHtml(html);
    for (const tag of ["blockquote", "pre", "code", "ul", "li", "img"]) {
      expect(out).toContain(`<${tag}`);
    }
  });
});
