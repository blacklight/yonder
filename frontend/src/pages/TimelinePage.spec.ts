import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useTimelineStore } from "@/stores/timeline";
import TimelinePage from "./TimelinePage.vue";
import type { Status } from "@/api/entities";

vi.mock("@/api/statuses", async (orig) => {
  const m = await orig<typeof import("@/api/statuses")>();
  return { ...m, publicTimeline: vi.fn(), tagTimeline: vi.fn() };
});
import { publicTimeline, tagTimeline } from "@/api/statuses";

function status(id: string): Status {
  return {
    id,
    account: { acct: "a", username: "a" },
    content: "<p>x</p>",
    created_at: new Date().toISOString(),
    media_attachments: [],
    reblog: null,
    pleroma: {},
  } as unknown as Status;
}

class FakeObserver {
  static last: FakeObserver;
  cb: IntersectionObserverCallback;
  constructor(cb: IntersectionObserverCallback) {
    this.cb = cb;
    FakeObserver.last = this;
  }
  observe() {}
  unobserve() {}
  disconnect() {}
  intersect() {
    this.cb(
      [{ isIntersecting: true } as IntersectionObserverEntry],
      this as never,
    );
  }
}
vi.stubGlobal("IntersectionObserver", FakeObserver);

describe("TimelinePage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.mocked(publicTimeline).mockReset();
    vi.mocked(tagTimeline).mockReset();
  });

  it("loads and renders cards", async () => {
    vi.mocked(publicTimeline).mockResolvedValue({ items: [status("1")] });
    const { wrapper } = mountWith(TimelinePage, {
      props: { timeline: "public", stream: "public" },
    });
    await flushPromises();
    expect(wrapper.findAll(".status-card")).toHaveLength(1);
  });

  it("shows error state with retry", async () => {
    vi.mocked(publicTimeline).mockRejectedValue(new Error("down"));
    const { wrapper } = mountWith(TimelinePage, {
      props: { timeline: "public", stream: "public" },
    });
    await flushPromises();
    expect(wrapper.text()).toContain("down");
    vi.mocked(publicTimeline).mockResolvedValue({ items: [status("2")] });
    await wrapper.find("button").trigger("click");
    await flushPromises();
    expect(wrapper.findAll(".status-card")).toHaveLength(1);
  });

  it("empty state renders", async () => {
    vi.mocked(publicTimeline).mockResolvedValue({ items: [] });
    const { wrapper } = mountWith(TimelinePage, {
      props: { timeline: "public", stream: "public", emptyKey: "explore" },
    });
    await flushPromises();
    expect(wrapper.text()).toContain("explore");
  });

  it("pending buffer shows and flushes new posts", async () => {
    vi.mocked(publicTimeline).mockResolvedValue({ items: [status("1")] });
    const { wrapper } = mountWith(TimelinePage, {
      props: { timeline: "public", stream: "public" },
    });
    await flushPromises();
    useTimelineStore().buffer("public", status("9"));
    await flushPromises();
    const btn = wrapper.get("[data-testid='pending']");
    await btn.trigger("click");
    expect(wrapper.findAll(".status-card")).toHaveLength(2);
  });

  it("retry after a failed loadMore resumes from the cursor", async () => {
    vi.mocked(publicTimeline)
      .mockResolvedValueOnce({
        items: [status("1")],
        next: "http://x/?max_id=1",
      })
      .mockRejectedValueOnce(new Error("flaky"))
      .mockResolvedValueOnce({ items: [status("2")] });
    const { wrapper } = mountWith(TimelinePage, {
      props: { timeline: "public", stream: "public" },
    });
    await flushPromises();
    FakeObserver.last.intersect();
    await flushPromises();
    // The failure is shown, the first page is still there.
    expect(wrapper.text()).toContain("flaky");
    expect(wrapper.findAll(".status-card")).toHaveLength(1);
    await wrapper.find(".timeline__error button").trigger("click");
    await flushPromises();
    // Retry was a loadMore — page one was kept, not reset.
    expect(wrapper.findAll(".status-card")).toHaveLength(2);
    expect(vi.mocked(publicTimeline).mock.calls[2][0]).toEqual(
      expect.objectContaining({ max_id: "1" }),
    );
  });

  it("hashtag timelines show the tag header and feed link", async () => {
    vi.mocked(tagTimeline).mockResolvedValue({ items: [] });
    const { wrapper } = mountWith(TimelinePage, {
      props: { timeline: "hashtag:rust", stream: "hashtag:rust" },
    });
    await flushPromises();
    expect(wrapper.get(".timeline__title").text()).toBe("#rust");
    expect(wrapper.get(".timeline__feed").attributes("href")).toBe(
      "/tags/rust.rss",
    );
  });

  it("percent-encodes unicode tags in the feed link", async () => {
    vi.mocked(tagTimeline).mockResolvedValue({ items: [] });
    const { wrapper } = mountWith(TimelinePage, {
      props: { timeline: "hashtag:café", stream: "hashtag:café" },
    });
    await flushPromises();
    expect(wrapper.get(".timeline__feed").attributes("href")).toBe(
      "/tags/caf%C3%A9.rss",
    );
  });

  it("non-hashtag timelines have no feed header", async () => {
    vi.mocked(publicTimeline).mockResolvedValue({ items: [] });
    const { wrapper } = mountWith(TimelinePage, {
      props: { timeline: "public", stream: "public" },
    });
    await flushPromises();
    expect(wrapper.find(".timeline__header").exists()).toBe(false);
  });

  it("sentinel triggers loadMore", async () => {
    vi.mocked(publicTimeline)
      .mockResolvedValueOnce({
        items: [status("1")],
        next: "http://x/?max_id=1",
      })
      .mockResolvedValueOnce({ items: [status("2")] });
    mountWith(TimelinePage, {
      props: { timeline: "public", stream: "public" },
    });
    await flushPromises();
    FakeObserver.last.intersect();
    await flushPromises();
    expect(useTimelineStore().get("public").items).toHaveLength(2);
  });
});
