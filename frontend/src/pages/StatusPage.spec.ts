import { describe, expect, it, vi, beforeEach } from "vitest";
import { nextTick } from "vue";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import StatusPage from "./StatusPage.vue";
import type { Account, Status } from "@/api/entities";

vi.mock("@/api/client", async (orig) => {
  const m = await orig<typeof import("@/api/client")>();
  return { ...m, request: vi.fn() };
});
vi.mock("@/api/statuses", async (orig) => {
  const m = await orig<typeof import("@/api/statuses")>();
  return {
    ...m,
    getStatusContext: vi.fn(),
    fetchRemoteContext: vi.fn(),
  };
});
vi.mock("@/api/emojis", () => ({
  customEmojis: vi.fn().mockResolvedValue([]),
}));

import { request } from "@/api/client";
import { fetchRemoteContext, getStatusContext } from "@/api/statuses";

function account(over: Partial<Account> = {}): Account {
  return {
    id: "1",
    username: "alice",
    acct: "alice",
    display_name: "Alice",
    note: "",
    url: "https://x.test/@alice",
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
    ...over,
  } as Account;
}

function status(over: Partial<Status> = {}): Status {
  return {
    id: "10",
    uri: "https://remote.test/s/10",
    url: "https://remote.test/s/10",
    account: account(),
    content: "<p>opened post</p>",
    created_at: new Date().toISOString(),
    edited_at: null,
    visibility: "public",
    sensitive: false,
    spoiler_text: "",
    media_attachments: [],
    mentions: [],
    tags: [],
    emojis: [],
    card: null,
    poll: null,
    application: null,
    language: "en",
    in_reply_to_id: null,
    in_reply_to_account_id: null,
    reblog: null,
    replies_count: 0,
    reblogs_count: 0,
    favourites_count: 0,
    quotes_count: 0,
    quote_id: null,
    quote: null,
    reblogged: false,
    favourited: false,
    bookmarked: false,
    pinned: false,
    muted: false,
    pleroma: { emoji_reactions: [] },
    ...over,
  } as Status;
}

const remoteStatus = () =>
  status({ account: account({ acct: "alice@remote.test" }) });

async function open(id = "10") {
  const { wrapper, router } = mountWith(StatusPage, { props: { id } });
  await router.push(`/statuses/${id}`);
  await flushPromises();
  return wrapper;
}

describe("StatusPage remote context", () => {
  beforeEach(() => {
    vi.mocked(request).mockReset();
    vi.mocked(getStatusContext).mockReset();
    vi.mocked(fetchRemoteContext).mockReset();
    vi.mocked(fetchRemoteContext).mockResolvedValue({
      imported: 0,
      scanned: 0,
      pages: 0,
      complete: true,
    });
    const auth = useAuthStore();
    auth.setToken("tk");
  });

  it("fetches the remote thread for remote statuses and re-reads the context", async () => {
    const s = remoteStatus();
    vi.mocked(request).mockImplementation((opts) =>
      opts.path === `/statuses/${s.id}`
        ? (Promise.resolve(s) as never)
        : (Promise.resolve({}) as never),
    );
    const ancestor = status({ id: "8", content: "<p>root post</p>" });
    vi.mocked(getStatusContext)
      .mockResolvedValueOnce({ ancestors: [], descendants: [] })
      .mockResolvedValue({ ancestors: [ancestor], descendants: [] });

    const wrapper = await open();

    expect(fetchRemoteContext).toHaveBeenCalledWith(s.id);
    // The initial context + the post-fetch refresh.
    expect(getStatusContext).toHaveBeenCalledTimes(2);
    await flushPromises();
    expect(wrapper.text()).toContain("root post");
  });

  it("does not fetch remote context for local statuses", async () => {
    const s = status(); // acct "alice" — local
    vi.mocked(request).mockResolvedValue(s as never);
    vi.mocked(getStatusContext).mockResolvedValue({
      ancestors: [],
      descendants: [],
    });

    await open();

    expect(fetchRemoteContext).not.toHaveBeenCalled();
  });

  it("does not fetch remote context when logged out", async () => {
    useAuthStore().setToken(null);
    const s = remoteStatus();
    vi.mocked(request).mockResolvedValue(s as never);
    vi.mocked(getStatusContext).mockResolvedValue({
      ancestors: [],
      descendants: [],
    });

    await open();

    expect(fetchRemoteContext).not.toHaveBeenCalled();
  });

  it("shows the remote-loading indicator while the fetch runs", async () => {
    const s = remoteStatus();
    vi.mocked(request).mockResolvedValue(s as never);
    vi.mocked(getStatusContext).mockResolvedValue({
      ancestors: [],
      descendants: [],
    });
    let resolveFetch!: (v: {
      imported: number;
      scanned: number;
      pages: number;
      complete: boolean;
    }) => void;
    vi.mocked(fetchRemoteContext).mockReturnValue(
      new Promise((r) => {
        resolveFetch = r;
      }),
    );

    const wrapper = await open();
    await nextTick();

    const row = wrapper.find(".remote-loading");
    expect(row.exists()).toBe(true);
    expect(row.attributes("role")).toBe("status");
    expect(row.text()).toContain("Fetching replies from the remote instance");
    expect(row.find(".spinner__wheel").exists()).toBe(true);

    resolveFetch({ imported: 0, scanned: 0, pages: 0, complete: true });
    await flushPromises();
    expect(wrapper.find(".remote-loading").exists()).toBe(false);
  });

  it("keeps the local thread when the remote fetch fails", async () => {
    const s = remoteStatus();
    vi.mocked(request).mockResolvedValue(s as never);
    vi.mocked(getStatusContext).mockResolvedValue({
      ancestors: [],
      descendants: [],
    });
    vi.mocked(fetchRemoteContext).mockRejectedValue(new Error("boom"));

    const wrapper = await open();

    expect(fetchRemoteContext).toHaveBeenCalledWith(s.id);
    expect(wrapper.text()).toContain("opened post");
    expect(wrapper.find(".remote-loading").exists()).toBe(false);
  });
});
