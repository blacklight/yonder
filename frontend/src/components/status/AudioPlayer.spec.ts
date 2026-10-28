import { describe, expect, it } from "vitest";
import { mountWith } from "@/test/render";
import AudioPlayer from "./AudioPlayer.vue";
import type { MediaAttachment } from "@/api/entities";

function attachment(over: Partial<MediaAttachment> = {}): MediaAttachment {
  return {
    id: "1",
    type: "audio",
    url: "https://x.test/files/track.mp3",
    preview_url: null,
    remote_url: "https://x.test/files/track.mp3",
    description: "eve - aurora",
    blurhash: null,
    ...over,
  } as MediaAttachment;
}

describe("AudioPlayer", () => {
  it("shows cover art when preview_url differs from the media url", () => {
    const { wrapper } = mountWith(AudioPlayer, {
      props: {
        media: attachment({ preview_url: "https://x.test/art/cover.png" }),
      },
    });
    const img = wrapper.get<HTMLImageElement>(".audio-player__cover");
    expect(img.attributes("src")).toBe("https://x.test/art/cover.png");
    expect(wrapper.find(".audio-player__placeholder").exists()).toBe(false);
  });

  it("falls back to the music placeholder when there is no poster art", () => {
    // The server emits preview_url = url when no art was extracted.
    const { wrapper } = mountWith(AudioPlayer, {
      props: {
        media: attachment({ preview_url: "https://x.test/files/track.mp3" }),
      },
    });
    expect(wrapper.find(".audio-player__cover").exists()).toBe(false);
    expect(wrapper.find(".audio-player__placeholder").exists()).toBe(true);
  });

  it("shows the description as the title and falls back to the filename", () => {
    const named = mountWith(AudioPlayer, {
      props: { media: attachment() },
    });
    expect(named.wrapper.get(".audio-player__title").text()).toBe(
      "eve - aurora",
    );
    const unnamed = mountWith(AudioPlayer, {
      props: { media: attachment({ description: null }) },
    });
    expect(unnamed.wrapper.get(".audio-player__title").text()).toBe(
      "track.mp3",
    );
  });

  it("toggles the play/pause affordance with playback state", async () => {
    const { wrapper } = mountWith(AudioPlayer, {
      props: { media: attachment() },
    });
    const play = wrapper.get(".audio-player__play");
    expect(play.attributes("aria-label")).toBe("Play");
    // jsdom cannot actually play media — drive the player's state
    // through the element's playback events.
    await wrapper.get("audio").trigger("play");
    expect(wrapper.get(".audio-player__play").attributes("aria-label")).toBe(
      "Pause",
    );
    await wrapper.get("audio").trigger("pause");
    expect(wrapper.get(".audio-player__play").attributes("aria-label")).toBe(
      "Play",
    );
  });

  it("wires seek, volume and download controls", () => {
    const { wrapper } = mountWith(AudioPlayer, {
      props: { media: attachment() },
    });
    const seek = wrapper.get<HTMLInputElement>(".audio-player__seek");
    expect(seek.attributes("type")).toBe("range");
    expect(seek.attributes("aria-label")).toBe("Seek");
    const vol = wrapper.get<HTMLInputElement>(".audio-player__volume-slider");
    expect(vol.attributes("type")).toBe("range");
    const download = wrapper.get<HTMLAnchorElement>(".audio-player__download");
    expect(download.attributes("href")).toBe("https://x.test/files/track.mp3");
    expect(download.attributes("download")).toBeDefined();
  });
});
