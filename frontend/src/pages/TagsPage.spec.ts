import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import TagsPage from "./TagsPage.vue";
import type { Tag } from "@/api/entities";

vi.mock("@/api/tags", async (orig) => {
  const m = await orig<typeof import("@/api/tags")>();
  return {
    ...m,
    trendingTags: vi.fn(),
    followedTags: vi.fn(),
    unfollowTag: vi.fn(),
  };
});
import { followedTags, trendingTags, unfollowTag } from "@/api/tags";

function tag(id: string, over: Partial<Tag> = {}): Tag {
  return {
    id,
    name: `tag${id}`,
    url: `https://x.test/tags/tag${id}`,
    history: [],
    following: false,
    featured: false,
    ...over,
  };
}

function lastCall() {
  const calls = vi.mocked(trendingTags).mock.calls;
  return calls[calls.length - 1]?.[0];
}

describe("TagsPage", () => {
  beforeEach(() => {
    vi.mocked(trendingTags).mockReset().mockResolvedValue([]);
    vi.mocked(followedTags).mockReset().mockResolvedValue({ items: [] });
    useAuthStore().setToken(null);
  });

  it("defaults to the last-24h-latest window, global scope, 20 per page", async () => {
    mountWith(TagsPage);
    await flushPromises();
    expect(trendingTags).toHaveBeenCalledWith({
      period: "day",
      anchor: "latest",
      local: false,
      limit: 21,
      offset: 0,
    });
  });

  it("switches scope and range, resetting the page", async () => {
    const { wrapper } = mountWith(TagsPage);
    await flushPromises();
    await wrapper.get("[data-testid='scope']").setValue("local");
    await flushPromises();
    expect(lastCall()).toMatchObject({ local: true, offset: 0 });

    await wrapper.get("[data-testid='range']").setValue("7d");
    await flushPromises();
    expect(lastCall()).toMatchObject({ period: "week", anchor: "latest" });
  });

  it("sends explicit from/to in custom mode after Apply", async () => {
    const { wrapper } = mountWith(TagsPage);
    await flushPromises();
    await wrapper.get("[data-testid='range']").setValue("custom");
    await flushPromises();
    // Entering custom must not fire a query yet.
    expect(vi.mocked(trendingTags).mock.calls).toHaveLength(1);
    await wrapper.get("[data-testid='from']").setValue("2026-01-01");
    await wrapper.get("[data-testid='to']").setValue("2026-01-31");
    await wrapper.get("[data-testid='apply']").trigger("click");
    await flushPromises();
    const q = lastCall();
    expect(q).toMatchObject({ from: "2026-01-01", to: "2026-01-31" });
    expect(q?.period).toBeUndefined();
    expect(q?.anchor).toBeUndefined();
  });

  it("pages with offset and shows one page-size worth of cards", async () => {
    const items = Array.from({ length: 21 }, (_, i) => tag(String(i)));
    vi.mocked(trendingTags).mockResolvedValue(items);
    const { wrapper } = mountWith(TagsPage);
    await flushPromises();
    // Lookahead row hidden — 20 cards rendered, next enabled.
    expect(wrapper.findAll("[data-testid='tag-card']")).toHaveLength(20);
    const next = wrapper.get("[data-testid='next-page']");
    expect(next.attributes("disabled")).toBeUndefined();
    await next.trigger("click");
    await flushPromises();
    expect(lastCall()).toMatchObject({ offset: 20 });
    await wrapper.get("[data-testid='prev-page']").trigger("click");
    await flushPromises();
    expect(lastCall()).toMatchObject({ offset: 0 });
  });

  it("disables next on a short page and changes page size", async () => {
    vi.mocked(trendingTags).mockResolvedValue([tag("1"), tag("2")]);
    const { wrapper } = mountWith(TagsPage);
    await flushPromises();
    // 2 < 21 → no lookahead → next disabled; pager shows only when
    // page > 0 or hasMore, so it stays hidden here.
    expect(wrapper.find("[data-testid='next-page']").exists()).toBe(false);

    await wrapper.get("[data-testid='per-page']").setValue("5");
    await flushPromises();
    expect(lastCall()).toMatchObject({ limit: 6, offset: 0 });
  });

  it("hides the followed section when logged out", async () => {
    const { wrapper } = mountWith(TagsPage);
    await flushPromises();
    expect(wrapper.find("[data-testid='followed']").exists()).toBe(false);
    expect(followedTags).not.toHaveBeenCalled();
  });

  it("lists followed tags and drops a card on unfollow", async () => {
    useAuthStore().setToken("tk");
    vi.mocked(followedTags).mockResolvedValue({
      items: [tag("f1", { following: true }), tag("f2", { following: true })],
    });
    vi.mocked(unfollowTag).mockResolvedValue(tag("f1", { following: false }));
    const { wrapper } = mountWith(TagsPage);
    await flushPromises();
    const section = wrapper.get("[data-testid='followed']");
    expect(section.findAll("[data-testid='tag-card']")).toHaveLength(2);
    await section.findAll("[data-testid='tag-follow']")[0]!.trigger("click");
    await flushPromises();
    expect(unfollowTag).toHaveBeenCalledWith("tagf1");
    expect(section.findAll("[data-testid='tag-card']")).toHaveLength(1);
  });

  it("shows the empty state for an empty window", async () => {
    const { wrapper } = mountWith(TagsPage);
    await flushPromises();
    expect(wrapper.get("[data-testid='trending']").text()).toContain(
      "No hashtags used in this window.",
    );
  });
});
