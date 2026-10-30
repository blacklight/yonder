import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import EmojiText from "./EmojiText.vue";

const EMOJIS = [
  { shortcode: "blobcat", url: "https://remote.test/emoji/blobcat.png" },
];

// Fragment root — read the host's textContent (wrapper.text() trims
// text roots, hiding the spaces emoji substitution must preserve).
function mountText(text: string, emojis?: typeof EMOJIS) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  return {
    wrapper: mount(EmojiText, { props: { text, emojis }, attachTo: host }),
    host,
  };
}

describe("EmojiText", () => {
  it("renders plain text unchanged", () => {
    const { wrapper, host } = mountText("Alice");
    expect(host.textContent).toBe("Alice");
    expect(wrapper.find("img").exists()).toBe(false);
  });

  it("expands known shortcodes to emoji imgs, keeping spaces", () => {
    const { wrapper, host } = mountText("Alice :blobcat:!", EMOJIS);
    const img = wrapper.get("img.emoji");
    expect(img.attributes("src")).toBe(EMOJIS[0].url);
    expect(img.attributes("alt")).toBe(":blobcat:");
    expect(host.textContent).toBe("Alice !");
  });

  it("leaves unknown shortcodes verbatim", () => {
    const { wrapper, host } = mountText("hi :nope:", EMOJIS);
    expect(host.textContent).toBe("hi :nope:");
    expect(wrapper.find("img").exists()).toBe(false);
  });
});
