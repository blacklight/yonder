import { describe, expect, it } from "vitest";
import {
  EMBED_IFRAME_HEIGHT,
  embedPageUrl,
  embedScriptUrl,
  iframeEmbed,
  scriptEmbed,
} from "./embed";

const origin = window.location.origin;

describe("embed snippets", () => {
  it("embedPageUrl targets the standalone embed route", () => {
    expect(embedPageUrl("42")).toBe(`${origin}/statuses/42/embed`);
    expect(embedPageUrl("a/b")).toBe(`${origin}/statuses/a%2Fb/embed`);
  });

  it("embedScriptUrl points at the public embed.js", () => {
    expect(embedScriptUrl()).toBe(`${origin}/embed.js`);
  });

  it("iframeEmbed renders a lazy iframe with a title", () => {
    const html = iframeEmbed("42", 'Post by @alice "quoted"');
    expect(html).toContain(`src="${origin}/statuses/42/embed"`);
    expect(html).toContain('width="100%"');
    expect(html).toContain(`height="${EMBED_IFRAME_HEIGHT}"`);
    expect(html).toContain('loading="lazy"');
    expect(html).toContain('title="Post by @alice &quot;quoted&quot;"');
  });

  it("scriptEmbed renders a placeholder div with a fallback link", () => {
    const html = scriptEmbed("42", "Post by @alice");
    expect(html).toContain('<div class="yonder-embed" data-status="42">');
    // The inner anchor is the no-JS fallback and the backlink
    // Webmention verification looks for.
    expect(html).toContain(
      `<a href="${origin}/statuses/42/embed">Post by @alice</a>`,
    );
    expect(html).toContain(`<script async src="${origin}/embed.js"`);
  });
});
