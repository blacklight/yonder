import { describe, expect, it, vi, beforeEach } from "vitest";
import { nextTick } from "vue";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import { useNotificationsStore } from "@/stores/notifications";
import type { Account, Notification } from "@/api/entities";
import NotificationsPage from "./NotificationsPage.vue";

const BOB: Account = {
  id: "9",
  acct: "bob@remote.example",
  username: "bob",
  display_name: "Bob Loblaw",
  note: "",
  url: "https://remote.example/@bob",
  avatar: "https://cdn.example/bob.png",
  header: "",
  locked: false,
  bot: false,
  created_at: "2026-01-01T00:00:00Z",
  followers_count: 0,
  following_count: 0,
  statuses_count: 0,
  fields: [],
  emojis: [],
};

class FakeObserver {
  static instances: FakeObserver[] = [];
  cb: IntersectionObserverCallback;
  observed = new Set<Element>();
  constructor(cb: IntersectionObserverCallback) {
    this.cb = cb;
    FakeObserver.instances.push(this);
  }
  observe(el: Element) {
    this.observed.add(el);
  }
  unobserve(el: Element) {
    this.observed.delete(el);
  }
  disconnect() {
    this.observed.clear();
  }
  intersect() {
    this.cb(
      [{ isIntersecting: true } as IntersectionObserverEntry],
      this as never,
    );
  }
}

function setup(items: unknown[]) {
  vi.spyOn(globalThis, "fetch").mockImplementation(() =>
    Promise.resolve(new Response(JSON.stringify(items), { status: 200 })),
  );
  useAuthStore().setToken("tk");
}

function notif(extra: Record<string, unknown> = {}): Notification {
  return {
    id: "1",
    type: "follow",
    created_at: "2026-10-26T00:00:00Z",
    account: BOB,
    status: null,
    ...extra,
  };
}

describe("NotificationsPage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    FakeObserver.instances = [];
  });

  it("renders the linked display name, avatar and acct line", async () => {
    setup([notif()]);
    const { wrapper } = mountWith(NotificationsPage);
    await flushPromises();
    const name = wrapper.get(".notif-name");
    expect(name.text()).toBe("Bob Loblaw");
    expect(name.attributes("href")).toBe("/@bob@remote.example");
    const avatar = wrapper.get(".notif-avatar");
    expect(avatar.attributes("href")).toBe("/@bob@remote.example");
    expect(avatar.get("img").attributes("src")).toBe(
      "https://cdn.example/bob.png",
    );
    expect(wrapper.get(".notif-label").text()).toContain("followed you");
    expect(wrapper.get(".notif-acct").text()).toBe("@bob@remote.example");
  });

  it("links the timestamp to the status", async () => {
    setup([
      notif({
        type: "mention",
        status: {
          id: "42",
          created_at: "2026-10-26T00:00:00Z",
          content: "<p>hello</p>",
          account: BOB,
          media_attachments: [],
          spoiler_text: "",
          visibility: "public",
        },
      }),
    ]);
    const { wrapper } = mountWith(NotificationsPage);
    await flushPromises();
    expect(wrapper.get(".notif-label").text()).toContain("mentioned you");
    expect(wrapper.get("a.notif-when").attributes("href")).toBe("/statuses/42");
  });

  it("shows a view-report link for admin.report rows", async () => {
    setup([notif({ type: "admin.report", report: { id: "7" } })]);
    const { wrapper } = mountWith(NotificationsPage);
    await flushPromises();
    expect(wrapper.get(".notif-report").attributes("href")).toBe(
      "/moderation/reports/7",
    );
    expect(wrapper.get(".notif-report").text()).toBe("View report");
  });

  it("falls back to the unknown-type label", async () => {
    setup([notif({ type: "severed_relationships" })]);
    const { wrapper } = mountWith(NotificationsPage);
    await flushPromises();
    expect(wrapper.get(".notif-label").text()).toContain("sent a notification");
  });

  it("marks rendered rows read automatically", async () => {
    setup([notif()]);
    mountWith(NotificationsPage);
    await flushPromises();
    const post = vi
      .mocked(globalThis.fetch)
      .mock.calls.find((c) => String(c[0]).includes("/notifications/read"));
    expect(post).toBeTruthy();
    // String ids — snowflake values corrupt as JS numbers.
    expect(JSON.parse(post![1]!.body as string)).toEqual({ ids: ["1"] });
  });

  it("fetches the next page when the bottom sentinel enters view", async () => {
    // Stubbed per-test: the mark-read path above relies on the
    // no-IntersectionObserver fallback.
    vi.stubGlobal("IntersectionObserver", FakeObserver);
    useAuthStore().setToken("tk");
    vi.spyOn(globalThis, "fetch")
      .mockImplementationOnce(() =>
        Promise.resolve(
          new Response(JSON.stringify([notif()]), {
            status: 200,
            headers: {
              Link: '<https://i.example/api/v1/notifications?max_id=99>; rel="next"',
            },
          }),
        ),
      )
      .mockImplementation(() =>
        Promise.resolve(
          new Response(JSON.stringify([notif({ id: "2" })]), {
            status: 200,
          }),
        ),
      );
    const { wrapper } = mountWith(NotificationsPage);
    await flushPromises();
    expect(wrapper.findAll(".notif")).toHaveLength(1);
    const sentinelObs = FakeObserver.instances.find((o) =>
      [...o.observed].some((el) =>
        (el as HTMLElement).classList?.contains("notif-sentinel"),
      ),
    );
    expect(sentinelObs).toBeTruthy();
    sentinelObs!.intersect();
    await flushPromises();
    expect(wrapper.findAll(".notif")).toHaveLength(2);
    const urls = vi
      .mocked(globalThis.fetch)
      .mock.calls.map((c) => String(c[0]));
    expect(urls.some((u) => u.includes("max_id=99"))).toBe(true);
  });

  it("mark-all button posts {all:true}, flips rows and hides itself", async () => {
    setup([notif()]);
    const { wrapper } = mountWith(NotificationsPage);
    const store = useNotificationsStore();
    await flushPromises();
    // No unread count yet — the bulk action stays hidden.
    expect(
      wrapper.findAll("button").some((b) => b.text().includes("Mark all")),
    ).toBe(false);
    store.unread = 3;
    await nextTick();
    const btn = wrapper
      .findAll("button")
      .find((b) => b.text() === "Mark all as read");
    expect(btn).toBeTruthy();
    // The viewport pass already posted `{ids:[…]}` — drop it so the
    // only /read call left is the button's.
    vi.mocked(globalThis.fetch).mockClear();
    await btn!.trigger("click");
    await flushPromises();
    const post = vi
      .mocked(globalThis.fetch)
      .mock.calls.find((c) => String(c[0]).includes("/notifications/read"));
    expect(JSON.parse(post![1]!.body as string)).toEqual({ all: true });
    expect(store.items.every((n) => n.read)).toBe(true);
    expect(store.unread).toBe(0);
    await nextTick();
    expect(
      wrapper.findAll("button").some((b) => b.text().includes("Mark all")),
    ).toBe(false);
  });

  it("live events prepend a row while the page is open", async () => {
    setup([]);
    const { wrapper } = mountWith(NotificationsPage);
    await flushPromises();
    useNotificationsStore().items.unshift(notif({ id: "live" }));
    await nextTick();
    expect(wrapper.findAll(".notif")).toHaveLength(1);
  });
});
