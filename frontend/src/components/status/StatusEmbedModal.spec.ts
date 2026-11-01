import { describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import StatusEmbedModal from "./StatusEmbedModal.vue";

const origin = window.location.origin;

function modal() {
  const { wrapper } = mountWith(StatusEmbedModal, {
    props: { statusId: "42", title: "Post by @alice" },
  });
  return wrapper;
}

describe("StatusEmbedModal", () => {
  it("offers script and iframe snippets for the status", () => {
    modal();
    const script = document.querySelector<HTMLTextAreaElement>(
      "[data-testid='embed-script']",
    );
    const iframe = document.querySelector<HTMLTextAreaElement>(
      "[data-testid='embed-iframe']",
    );
    expect(script?.value).toContain(
      '<div class="yonder-embed" data-status="42">',
    );
    expect(script?.value).toContain(
      `<a href="${origin}/statuses/42/embed">Post by @alice</a>`,
    );
    expect(script?.value).toContain(`${origin}/embed.js`);
    expect(iframe?.value).toContain(`src="${origin}/statuses/42/embed"`);
    expect(iframe?.value).toContain('title="Post by @alice"');
  });

  it("copies a snippet to the clipboard and confirms", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    modal();
    const button = document.querySelector<HTMLButtonElement>(
      "[data-testid='embed-copy-script']",
    )!;
    button.click();
    await flushPromises();
    expect(writeText).toHaveBeenCalledWith(
      expect.stringContaining('data-status="42"'),
    );
    expect(button.textContent).toContain("Copied");
  });
});
