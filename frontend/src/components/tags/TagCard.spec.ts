import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import TagCard from "./TagCard.vue";
import type { Tag } from "@/api/entities";

vi.mock("@/api/tags", async (orig) => {
  const m = await orig<typeof import("@/api/tags")>();
  return { ...m, followTag: vi.fn(), unfollowTag: vi.fn() };
});
import { followTag, unfollowTag } from "@/api/tags";

function tag(over: Partial<Tag> = {}): Tag {
  return {
    id: "1",
    name: "rust",
    url: "https://x.test/tags/rust",
    history: [],
    following: false,
    featured: false,
    ...over,
  };
}

describe("TagCard", () => {
  beforeEach(() => {
    vi.mocked(followTag).mockReset();
    vi.mocked(unfollowTag).mockReset();
    useAuthStore().setToken(null);
  });

  it("links the name to the tag timeline and shows window counters", () => {
    const { wrapper } = mountWith(TagCard, {
      props: { tag: tag({ uses: "42", accounts: "7" }) },
    });
    const link = wrapper.get(".tag-card__name");
    expect(link.text()).toBe("#rust");
    expect(link.attributes("href")).toBe("/tags/rust");
    expect(wrapper.text()).toContain("42");
    expect(wrapper.text()).toContain("7");
  });

  it("encodes the tag name in the link", () => {
    const { wrapper } = mountWith(TagCard, {
      props: { tag: tag({ name: "épî çâ" }) },
    });
    expect(wrapper.get(".tag-card__name").attributes("href")).toBe(
      `/tags/${encodeURIComponent("épî çâ")}`,
    );
  });

  it("sums history when window counters are absent", () => {
    const { wrapper } = mountWith(TagCard, {
      props: {
        tag: tag({
          history: [
            { day: "1", uses: "3", accounts: "2" },
            { day: "2", uses: "4", accounts: "1" },
          ],
        }),
      },
    });
    expect(wrapper.text()).toContain("7");
    expect(wrapper.text()).toContain("3");
  });

  it("hides the follow button when logged out", () => {
    const { wrapper } = mountWith(TagCard, { props: { tag: tag() } });
    expect(wrapper.find("[data-testid='tag-follow']").exists()).toBe(false);
  });

  it("follows and emits change", async () => {
    useAuthStore().setToken("tk");
    vi.mocked(followTag).mockResolvedValue(tag({ following: true }));
    const { wrapper } = mountWith(TagCard, { props: { tag: tag() } });
    await wrapper.get("[data-testid='tag-follow']").trigger("click");
    await flushPromises();
    expect(followTag).toHaveBeenCalledWith("rust");
    expect(wrapper.emitted("change")?.[0]?.[0]).toMatchObject({
      name: "rust",
      following: true,
    });
    expect(wrapper.get("[data-testid='tag-follow']").text()).toContain(
      "Unfollow",
    );
  });

  it("unfollows and emits change", async () => {
    useAuthStore().setToken("tk");
    vi.mocked(unfollowTag).mockResolvedValue(tag({ following: false }));
    const { wrapper } = mountWith(TagCard, {
      props: { tag: tag({ following: true }) },
    });
    await wrapper.get("[data-testid='tag-follow']").trigger("click");
    await flushPromises();
    expect(unfollowTag).toHaveBeenCalledWith("rust");
    expect(wrapper.emitted("change")?.[0]?.[0]).toMatchObject({
      following: false,
    });
  });
});
