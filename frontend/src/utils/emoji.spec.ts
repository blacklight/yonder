import { describe, expect, it } from "vitest";
import { emojifyHtml, emojiParts } from "@/utils/emoji";

const EMOJIS = [
  { shortcode: "blobcat", url: "https://remote.test/emoji/blobcat.png" },
];

describe("emojiParts", () => {
  it("splits text around known shortcodes", () => {
    expect(emojiParts("hi :blobcat: bye", EMOJIS)).toEqual([
      { text: "hi " },
      { emoji: EMOJIS[0] },
      { text: " bye" },
    ]);
  });

  it("leaves unknown shortcodes and non-candidates verbatim", () => {
    expect(emojiParts(":nope: x", EMOJIS)).toEqual([{ text: ":nope: x" }]);
    expect(emojiParts("a:b c:d:", EMOJIS)).toEqual([{ text: "a:b c:d:" }]);
    expect(emojiParts(":::", EMOJIS)).toEqual([{ text: ":::" }]);
    expect(emojiParts("http://x:y", EMOJIS)).toEqual([{ text: "http://x:y" }]);
  });

  it("matches shortcodes case-insensitively", () => {
    expect(emojiParts(":BlobCat:", EMOJIS)).toEqual([{ emoji: EMOJIS[0] }]);
  });

  it("returns the text untouched without emojis or colons", () => {
    expect(emojiParts(":blobcat:", undefined)).toEqual([{ text: ":blobcat:" }]);
    expect(emojiParts("plain", EMOJIS)).toEqual([{ text: "plain" }]);
  });
});

describe("emojifyHtml", () => {
  it("expands :code: in text runs to emoji imgs", () => {
    const out = emojifyHtml("<p>hey :blobcat:!</p>", EMOJIS);
    expect(out).toContain('<img class="emoji"');
    expect(out).toContain('src="https://remote.test/emoji/blobcat.png"');
    expect(out).toContain('alt=":blobcat:"');
    expect(out).toContain("!");
  });

  it("skips anchors and code/pre subtrees", () => {
    const html =
      '<a href="https://x.test">:blobcat:</a>' +
      "<code>:blobcat:</code><pre>:blobcat:</pre> :blobcat:";
    const out = emojifyHtml(html, EMOJIS);
    expect(out.match(/<img /g)).toHaveLength(1);
    expect(out).toContain("<code>:blobcat:</code>");
    expect(out).toContain("<pre>:blobcat:</pre>");
    expect(out).toContain(">:blobcat:</a>");
  });

  it("returns html unchanged without emojis or shortcode text", () => {
    expect(emojifyHtml("<p>:blobcat:</p>", undefined)).toBe("<p>:blobcat:</p>");
    expect(emojifyHtml("<p>plain</p>", EMOJIS)).toBe("<p>plain</p>");
  });
});
