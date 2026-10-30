import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import ListsPage from "./ListsPage.vue";
import type { Account, ListEntity } from "@/api/entities";

vi.mock("@/api/lists", async (orig) => {
  const m = await orig<typeof import("@/api/lists")>();
  return {
    ...m,
    listLists: vi.fn(),
    createList: vi.fn(),
    deleteList: vi.fn(),
    listMembers: vi.fn(),
    addListMembers: vi.fn(),
    removeListMembers: vi.fn(),
  };
});
vi.mock("@/api/search", async (orig) => {
  const m = await orig<typeof import("@/api/search")>();
  return { ...m, search: vi.fn() };
});
import {
  listLists,
  createList,
  deleteList,
  listMembers,
  removeListMembers,
} from "@/api/lists";

function list(over: Partial<ListEntity> = {}): ListEntity {
  return {
    id: "1",
    title: "Friends",
    replies_policy: "followed",
    ...over,
  };
}

function account(partial: Partial<Account>): Account {
  return {
    id: "1",
    username: "alice",
    acct: "alice",
    display_name: "Alice",
    note: "",
    url: "https://example.org/@alice",
    avatar: "",
    header: "",
    locked: false,
    bot: false,
    created_at: new Date().toISOString(),
    followers_count: 0,
    following_count: 0,
    statuses_count: 0,
    fields: [],
    emojis: [],
    moved: null,
    ...partial,
  };
}

describe("ListsPage", () => {
  beforeEach(() => {
    vi.mocked(listLists).mockReset().mockResolvedValue([]);
    vi.mocked(createList).mockReset();
    vi.mocked(deleteList).mockReset();
    vi.mocked(listMembers).mockReset().mockResolvedValue({ items: [] });
    vi.mocked(removeListMembers).mockReset();
  });

  it("shows the empty state when there are no lists", async () => {
    const { wrapper } = mountWith(ListsPage);
    await flushPromises();
    expect(wrapper.text()).toContain("Nothing here yet.");
    expect(wrapper.find(".lists__list").exists()).toBe(false);
  });

  it("renders each list linking to its timeline", async () => {
    vi.mocked(listLists).mockResolvedValue([
      list({ id: "1", title: "Friends" }),
      list({ id: "2", title: "Work" }),
    ]);
    const { wrapper } = mountWith(ListsPage);
    await flushPromises();
    const links = wrapper.findAll(".lists__title");
    expect(links).toHaveLength(2);
    expect(links[0].text()).toBe("Friends");
    expect(links[0].attributes("href")).toBe("/lists/1");
    expect(links[1].text()).toBe("Work");
  });

  it("creates a list from the form", async () => {
    vi.mocked(createList).mockResolvedValue(list({ id: "9", title: "New" }));
    const { wrapper } = mountWith(ListsPage);
    await flushPromises();
    const input = wrapper.find(".field__input");
    await input.setValue("New");
    await wrapper.find("form").trigger("submit.prevent");
    await flushPromises();
    expect(vi.mocked(createList)).toHaveBeenCalledWith({
      title: "New",
      replies_policy: "followed",
      exclusive: false,
    });
    expect(wrapper.find(".lists__title").text()).toBe("New");
  });

  it("confirms before deleting a list", async () => {
    vi.mocked(listLists).mockResolvedValue([list()]);
    vi.mocked(deleteList).mockResolvedValue({});
    const { wrapper } = mountWith(ListsPage);
    await flushPromises();
    await wrapper
      .findAll("button")
      .find((b) => b.text() === "Delete")!
      .trigger("click");
    await flushPromises();
    expect(vi.mocked(deleteList)).not.toHaveBeenCalled();
    const modal = document.querySelector(".modal__panel");
    expect(modal?.textContent).toContain("Delete this list?");
    const confirm = [...document.querySelectorAll("button")].find(
      (b) => b.textContent === "Confirm",
    )!;
    confirm.click();
    await flushPromises();
    expect(vi.mocked(deleteList)).toHaveBeenCalledWith("1");
    expect(wrapper.find(".lists__list").exists()).toBe(false);
  });

  it("expands members and removes one", async () => {
    vi.mocked(listLists).mockResolvedValue([list()]);
    vi.mocked(listMembers).mockResolvedValue({
      items: [account({ id: "2", acct: "bob" })],
    });
    vi.mocked(removeListMembers).mockResolvedValue({});
    const { wrapper } = mountWith(ListsPage);
    await flushPromises();
    await wrapper
      .findAll("button")
      .find((b) => b.text() === "Members")!
      .trigger("click");
    await flushPromises();
    expect(vi.mocked(listMembers)).toHaveBeenCalledWith("1");
    expect(wrapper.text()).toContain("@bob");
    await wrapper
      .findAll(".lists__person button")
      .find((b) => b.text() === "Remove")!
      .trigger("click");
    await flushPromises();
    expect(vi.mocked(removeListMembers)).toHaveBeenCalledWith("1", ["2"]);
    expect(wrapper.text()).not.toContain("@bob");
  });
});
