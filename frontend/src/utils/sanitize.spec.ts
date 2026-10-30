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

  it("localizes markerless mention/hashtag links via their text", () => {
    // The backend strips every attribute but href from remote HTML —
    // an `@name`/`#tag` link label is what identifies the reference.
    const out = sanitizeHtml(
      `<p>hey <a href="https://remote.test/users/alice">@alice</a> ` +
        `<a href="https://x.test/tags/rust">#rust</a></p>`,
    );
    expect(out).toContain('href="/@alice@remote.test"');
    expect(out).toContain('href="/tags/rust"');
    expect(out).not.toContain("target=");
    // Ordinary links stay external.
    const ext = sanitizeHtml(`<a href="https://x.test/post">a post</a>`);
    expect(ext).toContain('href="https://x.test/post"');
    expect(ext).toContain('rel="nofollow noopener"');
    expect(ext).toContain('target="_blank"');
    expect(ext).not.toContain("class=");
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

  it("rewrites mention-class anchors to local profile routes", () => {
    const out = sanitizeHtml(
      `<p><a href="https://remote.test/@alice" class="u-url mention">@alice</a> ` +
        `<a href="https://fedi.example/users/bob" class="mention">@bob</a></p>`,
    );
    expect(out).toContain('href="/@alice@remote.test"');
    expect(out).toContain('href="/@bob@fedi.example"');
    // Internal links keep same-tab navigation — the SPA router takes
    // over at click time.
    expect(out).not.toContain("target=");
  });

  it("rewrites hashtag anchors to local tag routes", () => {
    // Mastodon's shape: class="mention hashtag" + rel="tag".
    const out = sanitizeHtml(
      `<a href="https://remote.test/tags/rust" class="mention hashtag" rel="tag">` +
        `#<span>rust</span></a>` +
        `<a href="https://other.example/tags/%C3%A9t%C3%A9" rel="tag">#été</a>`,
    );
    expect(out).toContain('href="/tags/rust"');
    expect(out).toContain('href="/tags/%C3%A9t%C3%A9"');
  });

  it("maps mention/tag anchors through the status entities", () => {
    // No marker classes — the entity url match is authoritative.
    const out = sanitizeHtml(
      `<a href="https://remote.test/actors/abc123">@bob</a>` +
        `<a href="https://remote.test/topics/rust">#rust</a>`,
      {
        mentions: [
          {
            acct: "bob@remote.test",
            url: "https://remote.test/actors/abc123",
          },
        ],
        tags: [{ name: "rust", url: "https://remote.test/topics/rust/" }],
      },
    );
    expect(out).toContain('href="/@bob@remote.test"');
    expect(out).toContain('href="/tags/rust"');
  });

  it("matches entities by text/tag-path when URLs don't align", () => {
    // Real shape of our API: `tags[].url` is THIS instance's tag URL,
    // `mentions[].url` the actor URL — neither equals the remote href.
    const out = sanitizeHtml(
      `<a href="https://remote.test/@alice">@alice</a>` +
        `<a href="https://remote.test/tags/rust">#rust</a>`,
      {
        mentions: [
          {
            acct: "alice@remote.test",
            url: "https://remote.test/users/alice",
          },
        ],
        tags: [{ name: "rust", url: `${location.origin}/tags/rust` }],
      },
    );
    expect(out).toContain('href="/@alice@remote.test"');
    expect(out).toContain('href="/tags/rust"');
  });

  it("collapses same-origin links to local paths", () => {
    const out = sanitizeHtml(
      `<a href="${location.origin}/@alice">@alice</a>` +
        `<a href="${location.origin}/@alice/123">post</a>` +
        `<a href="${location.origin}/tags/rust">#rust</a>`,
    );
    expect(out).toContain('href="/@alice"');
    expect(out).toContain('href="/@alice/123"');
    expect(out).toContain('href="/tags/rust"');
    expect(out).not.toContain("target=");
  });

  it("leaves unmarked remote links external", () => {
    // A non-fedi site's /tags/ path is not a hashtag.
    const out = sanitizeHtml(
      `<a href="https://blog.example/tags/post">x</a>` +
        `<a href="https://video.example/@chan">y</a>`,
    );
    expect(out).toContain('href="https://blog.example/tags/post"');
    expect(out).toContain('href="https://video.example/@chan"');
    expect(out).toContain('target="_blank"');
  });

  it("keeps emoji imgs styled — class restored after FORBID_ATTR", () => {
    // The backend emits `<img class="emoji" alt=":code:" …>` for
    // substituted shortcodes; `class`/`title` are forbidden attrs, so
    // the hook re-adds the fixed `emoji` class.
    const out = sanitizeHtml(
      `<p>x <img class="emoji" alt=":blobcat:" title=":blobcat:" ` +
        `src="https://remote.test/emoji/blobcat.png" width="20" height="20"></p>`,
    );
    expect(out).toContain('class="emoji"');
    expect(out).toContain('alt=":blobcat:"');
    expect(out).toContain('draggable="false"');
  });

  it("expands :shortcode: in content through links.emojis", () => {
    // Translated content arrives with literal shortcodes — the status
    // `emojis` array expands them, skipping link text.
    const out = sanitizeHtml(
      `<p>wow :blobcat: <a href="https://x.test">:blobcat:</a></p>`,
      {
        emojis: [
          {
            shortcode: "blobcat",
            url: "https://remote.test/emoji/blobcat.png",
          },
        ],
      },
    );
    expect(out.match(/<img /g)).toHaveLength(1);
    expect(out).toContain('src="https://remote.test/emoji/blobcat.png"');
    expect(out).toContain(">:blobcat:</a>");
  });
});
