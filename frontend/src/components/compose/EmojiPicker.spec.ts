import { describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { customEmojis } from "@/api/emojis";
import { UNICODE_EMOJIS } from "@/utils/unicodeEmoji";
import EmojiPicker from "./EmojiPicker.vue";

vi.mock("@/api/emojis", () => ({
  customEmojis: vi.fn().mockResolvedValue([]),
}));

const BLOB = {
  shortcode: "blob",
  url: "https://example.com/blob.png",
  visible_in_picker: true,
  category: "Blob",
};

describe("EmojiPicker", () => {
  it("renders the unicode catalogue even with no custom emoji", async () => {
    const { wrapper } = mountWith(EmojiPicker);
    await flushPromises();
    const cats = wrapper.findAll(".emoji-picker__cat");
    expect(cats.map((c) => c.text())).toContain("Smileys & emotion");
    expect(cats.map((c) => c.text())).toContain("Flags");
    // Every catalogue entry is a square tile with a text glyph.
    const glyphs = wrapper.findAll(".emoji-picker__glyph");
    expect(glyphs).toHaveLength(
      UNICODE_EMOJIS.reduce((n, c) => n + c.emojis.length, 0),
    );
  });

  it("a unicode pick emits the literal character", async () => {
    const { wrapper } = mountWith(EmojiPicker);
    await flushPromises();
    const smile = wrapper
      .findAll(".emoji-picker__glyph")
      .find((g) => g.text() === "😀");
    expect(smile).toBeTruthy();
    await smile!.trigger("click");
    expect(wrapper.emitted("select")).toEqual([
      [{ kind: "unicode", value: "😀" }],
    ]);
  });

  it("custom emoji are listed by category and emit the shortcode", async () => {
    vi.mocked(customEmojis).mockResolvedValueOnce([
      BLOB,
      { ...BLOB, shortcode: "hidden", visible_in_picker: false },
    ]);
    const { wrapper } = mountWith(EmojiPicker);
    await flushPromises();
    const cats = wrapper.findAll(".emoji-picker__cat");
    expect(cats[0].text()).toBe("Blob");
    const img = wrapper.get("img[alt='blob']");
    await img.trigger("click");
    expect(wrapper.emitted("select")).toEqual([
      [{ kind: "custom", value: "blob" }],
    ]);
    // Picker-invisible emoji are filtered out.
    expect(wrapper.find("img[alt='hidden']").exists()).toBe(false);
  });
});
