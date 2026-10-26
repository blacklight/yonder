import { afterEach, describe, expect, it } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import StatusMedia from "./StatusMedia.vue";
import type { MediaAttachment } from "@/api/entities";

function attachment(over: Partial<MediaAttachment> = {}): MediaAttachment {
  return {
    id: "1",
    type: "image",
    url: "https://x.test/full.png",
    preview_url: "https://x.test/preview.png",
    remote_url: null,
    description: null,
    blurhash: null,
    ...over,
  } as MediaAttachment;
}

describe("StatusMedia", () => {
  afterEach(() => {
    document.body.innerHTML = "";
    document.body.style.overflow = "";
  });

  it("renders thumbnails as buttons that open the media modal", async () => {
    const { wrapper } = mountWith(StatusMedia, {
      props: { media: [attachment()], sensitive: false },
    });
    const btn = wrapper.get(".status-media__img-btn");
    expect(btn.get("img").attributes("src")).toBe("https://x.test/preview.png");
    await btn.trigger("click");
    const modal = document.querySelector("[data-testid='media-modal']");
    expect(modal).toBeTruthy();
    const img = modal!.querySelector<HTMLImageElement>(".media-modal__img")!;
    expect(img.src).toBe("https://x.test/full.png");
  });

  it("closes the modal on Escape and restores body scroll", async () => {
    const { wrapper } = mountWith(StatusMedia, {
      props: { media: [attachment()], sensitive: false },
    });
    await wrapper.get(".status-media__img-btn").trigger("click");
    expect(document.body.style.overflow).toBe("hidden");
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await flushPromises();
    expect(document.querySelector("[data-testid='media-modal']")).toBeNull();
    expect(document.body.style.overflow).toBe("");
  });

  it("navigates between attachments with arrows and keyboard", async () => {
    const media = [
      attachment({ id: "1", url: "https://x.test/1.png" }),
      attachment({ id: "2", url: "https://x.test/2.png" }),
    ];
    const { wrapper } = mountWith(StatusMedia, {
      props: { media, sensitive: false },
    });
    await wrapper.get(".status-media__img-btn").trigger("click");
    document.querySelector<HTMLElement>("[data-testid='media-next']")!.click();
    await flushPromises();
    expect(
      document
        .querySelector<HTMLImageElement>(".media-modal__img")!
        .getAttribute("src"),
    ).toBe("https://x.test/2.png");
    // Wraps around backwards to the first attachment.
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft" }));
    await flushPromises();
    expect(
      document
        .querySelector<HTMLImageElement>(".media-modal__img")!
        .getAttribute("src"),
    ).toBe("https://x.test/1.png");
  });

  it("shows the alt text caption in the modal", async () => {
    const { wrapper } = mountWith(StatusMedia, {
      props: {
        media: [attachment({ description: "a red fox" })],
        sensitive: false,
      },
    });
    await wrapper.get(".status-media__img-btn").trigger("click");
    expect(document.querySelector(".media-modal__caption")?.textContent).toBe(
      "a red fox",
    );
  });

  it("keeps media hidden behind the sensitive cover", () => {
    const { wrapper } = mountWith(StatusMedia, {
      props: { media: [attachment()], sensitive: true },
    });
    expect(wrapper.find(".status-media__img-btn").exists()).toBe(false);
    expect(wrapper.find("[data-testid='sensitive-cover']").exists()).toBe(true);
  });
});
