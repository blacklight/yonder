import { describe, expect, it } from "vitest";
import { mountWith } from "@/test/render";
import VideoAttachment from "./VideoAttachment.vue";
import type { MediaAttachment } from "@/api/entities";

function attachment(over: Partial<MediaAttachment> = {}): MediaAttachment {
  return {
    id: "1",
    type: "video",
    url: "https://x.test/files/clip.mp4",
    preview_url: "https://x.test/files/clip.mp4",
    remote_url: "https://x.test/files/clip.mp4",
    description: "a clip",
    blurhash: null,
    ...over,
  } as MediaAttachment;
}

describe("VideoAttachment", () => {
  it("renders a playable video element with poster art", () => {
    const { wrapper } = mountWith(VideoAttachment, {
      props: {
        media: attachment({ preview_url: "https://x.test/art/poster.jpg" }),
      },
    });
    const video = wrapper.get<HTMLVideoElement>("video");
    expect(video.attributes("src")).toBe("https://x.test/files/clip.mp4");
    expect(video.attributes("poster")).toBe("https://x.test/art/poster.jpg");
    expect(video.attributes("controls")).toBeDefined();
    expect(video.attributes("playsinline")).toBeDefined();
  });

  it("omits the poster attribute when no art exists", () => {
    // Server-side preview_url falls back to `url` when no poster was
    // extracted — an mp4 is not poster art.
    const { wrapper } = mountWith(VideoAttachment, {
      props: { media: attachment() },
    });
    expect(wrapper.get("video").attributes("poster")).toBeUndefined();
  });
});
