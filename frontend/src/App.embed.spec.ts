import { afterEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import {
  createMemoryHistory,
  createRouter,
  type RouteRecordRaw,
} from "vue-router";
import App from "./App.vue";
import { i18n, loadInitialLocale } from "@/i18n";
import StatusEmbedPage from "@/pages/StatusEmbedPage.vue";
import type { Account, Status } from "@/api/entities";

vi.mock("@/api/statuses", async (orig) => {
  const m = await orig<typeof import("@/api/statuses")>();
  return { ...m, getStatus: vi.fn() };
});

import { getStatus } from "@/api/statuses";

// Firefox ETP / Tor Browser deny all Web Storage in third-party
// frames: every access throws a SecurityError DOMException. The app
// must still boot and render inside `/statuses/{id}/embed`.
const realStorage = localStorage;

function stubBlockedStorage() {
  const denied = () => {
    throw new DOMException("The operation is insecure.", "SecurityError");
  };
  vi.stubGlobal("localStorage", {
    getItem: denied,
    setItem: denied,
    removeItem: denied,
    clear: denied,
    key: denied,
    length: 0,
  });
}

afterEach(() => vi.stubGlobal("localStorage", realStorage));

const routes: RouteRecordRaw[] = [
  {
    path: "/statuses/:id/embed",
    name: "status-embed",
    component: StatusEmbedPage,
    props: true,
    meta: { embed: true },
  },
  {
    path: "/:pathMatch(.*)*",
    name: "not-found",
    component: { template: "<div />" },
  },
];

function status(): Status {
  return {
    id: "10",
    uri: "https://x.test/s/10",
    url: "https://x.test/@alice/10",
    account: {
      id: "7",
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
    } as Account,
    content: "<p>hello embed</p>",
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
  } as Status;
}

describe("app boot with third-party storage blocked", () => {
  it("mounts the embed route and renders the status", async () => {
    stubBlockedStorage();
    vi.mocked(getStatus).mockResolvedValue(status());

    // The calls that threw inside main.ts bootstrap().
    await expect(loadInitialLocale()).resolves.toBeUndefined();

    const router = createRouter({
      history: createMemoryHistory(),
      routes,
    });
    router.push("/statuses/10/embed");
    await router.isReady();

    const wrapper = mount(App, {
      global: { plugins: [router, i18n] },
    });
    await flushPromises();

    expect(wrapper.text()).toContain("Alice");
    expect(wrapper.text()).toContain("hello embed");
  });
});
