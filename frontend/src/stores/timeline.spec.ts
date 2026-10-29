import { describe, expect, it, vi, beforeEach } from "vitest";
import { useTimelineStore } from "./timeline";
import type { Status } from "@/api/entities";
import type { Page } from "@/api/pagination";

vi.mock("@/api/statuses", () => ({
  homeTimeline: vi.fn(),
  publicTimeline: vi.fn(),
  tagTimeline: vi.fn(),
  listTimeline: vi.fn(),
}));

import {
  homeTimeline,
  listTimeline,
  publicTimeline,
  tagTimeline,
} from "@/api/statuses";

const home = vi.mocked(homeTimeline);
const pub = vi.mocked(publicTimeline);
const tag = vi.mocked(tagTimeline);
const list = vi.mocked(listTimeline);

function status(id: string): Status {
  return { id } as Status;
}

function page(items: Status[], next?: string): Page<Status> {
  return { items, next };
}

describe("timeline store", () => {
  beforeEach(() => vi.clearAllMocks());

  it("loads the first page and exposes the cursor", async () => {
    home.mockResolvedValue(
      page(
        [status("1"), status("2")],
        "http://x/api/v1/timelines/home?max_id=2&limit=20",
      ),
    );
    const tl = useTimelineStore();
    await tl.load("home");
    expect(home).toHaveBeenCalledWith(undefined);
    expect(tl.get("home").items.map((s) => s.id)).toEqual(["1", "2"]);
    expect(tl.get("home").next).toEqual({ max_id: "2", limit: 20 });
    expect(tl.hasMore("home")).toBe(true);
    expect(tl.get("home").loaded).toBe(true);
  });

  it("loadMore appends and dedupes", async () => {
    home
      .mockResolvedValueOnce(
        page([status("1"), status("2")], "http://x/?max_id=2"),
      )
      .mockResolvedValueOnce(page([status("2"), status("3")]));
    const tl = useTimelineStore();
    await tl.load("home");
    await tl.loadMore("home");
    expect(home).toHaveBeenLastCalledWith({ max_id: "2" });
    expect(tl.get("home").items.map((s) => s.id)).toEqual(["1", "2", "3"]);
    expect(tl.hasMore("home")).toBe(false);
  });

  it("records the error and clears on retry", async () => {
    home.mockRejectedValueOnce(new Error("boom"));
    const tl = useTimelineStore();
    await tl.load("home");
    expect(tl.get("home").error).toBe("boom");
    home.mockResolvedValueOnce(page([status("1")]));
    await tl.load("home");
    expect(tl.get("home").error).toBeNull();
  });

  it("dispatches to the right fetcher per key", async () => {
    pub.mockResolvedValue(page([]));
    tag.mockResolvedValue(page([]));
    list.mockResolvedValue(page([]));
    const tl = useTimelineStore();
    await tl.load("public");
    await tl.load("public:local");
    await tl.load("hashtag:rust");
    await tl.load("list:7");
    expect(pub).toHaveBeenCalledWith(undefined);
    expect(pub).toHaveBeenCalledWith({ local: true });
    expect(tag).toHaveBeenCalledWith("rust", undefined);
    expect(list).toHaveBeenCalledWith("7", undefined);
  });

  it("buffers live updates until flushPending", async () => {
    pub.mockResolvedValue(page([status("1")]));
    const tl = useTimelineStore();
    await tl.load("public");
    tl.buffer("public", status("2"));
    tl.buffer("public", status("3"));
    expect(tl.get("public").items).toHaveLength(1);
    expect(tl.pendingCount("public")).toBe(2);
    tl.flushPending("public");
    expect(tl.get("public").items.map((s) => s.id)).toEqual(["3", "2", "1"]);
    expect(tl.pendingCount("public")).toBe(0);
  });

  it("buffer replaces a visible item on repeat events (live edits)", () => {
    const tl = useTimelineStore();
    tl.ensure("public").items = [status("1")];
    tl.buffer("public", { ...status("1"), content: "<p>edited</p>" });
    const got = tl.get("public");
    expect(got.items[0].content).toBe("<p>edited</p>");
    expect(got.pending).toHaveLength(0);
    // The same is true for a buffered item.
    tl.buffer("public", status("9"));
    tl.buffer("public", { ...status("9"), content: "<p>v2</p>" });
    expect(tl.get("public").pending[0].content).toBe("<p>v2</p>");
    expect(tl.get("public").pending).toHaveLength(1);
  });

  it("buffer dedupes against items and pending", async () => {
    pub.mockResolvedValue(page([status("1")]));
    const tl = useTimelineStore();
    await tl.load("public");
    tl.buffer("public", status("1"));
    tl.buffer("public", status("2"));
    tl.buffer("public", status("2"));
    expect(tl.pendingCount("public")).toBe(1);
  });

  it("remove drops ids from items and pending", async () => {
    pub.mockResolvedValue(page([status("1"), status("2")]));
    const tl = useTimelineStore();
    await tl.load("public");
    tl.buffer("public", status("3"));
    tl.remove("2");
    tl.remove("3");
    expect(tl.get("public").items.map((s) => s.id)).toEqual(["1"]);
    expect(tl.pendingCount("public")).toBe(0);
  });

  it("prepend dedupes at the top", async () => {
    pub.mockResolvedValue(page([status("1")]));
    const tl = useTimelineStore();
    await tl.load("public");
    tl.prepend("public", status("0"));
    tl.prepend("public", status("0"));
    expect(tl.get("public").items.map((s) => s.id)).toEqual(["0", "1"]);
  });
});

describe("timeline live patches", () => {
  function boost(id: string, original: Status): Status {
    return { id, reblog: original } as Status;
  }

  it("remove drops the status and its boost wrappers everywhere", async () => {
    home.mockResolvedValue(page([status("1"), boost("9", status("2"))]));
    pub.mockResolvedValue(page([status("2"), status("3")]));
    const tl = useTimelineStore();
    await tl.load("home");
    await tl.load("public");

    tl.remove("2");
    expect(tl.get("home").items.map((s) => s.id)).toEqual(["1"]);
    expect(tl.get("public").items.map((s) => s.id)).toEqual(["3"]);
  });

  it("replace patches the original inside a boost wrapper, keeping flags", async () => {
    const wrapped = {
      ...status("9"),
      reblog: { ...status("2"), favourited: true },
    } as Status;
    home.mockResolvedValue(page([wrapped]));
    const tl = useTimelineStore();
    await tl.load("home");

    tl.replace({ ...status("2"), content: "edited" } as Status);
    const item = tl.get("home").items[0];
    expect(item.id).toBe("9");
    expect(item.reblog?.content).toBe("edited");
    // Stream entities are unpersonalized — the viewer flag survives.
    expect(item.reblog?.favourited).toBe(true);
  });

  it("replace touches every loaded timeline, pending included", async () => {
    home.mockResolvedValue(page([status("1")]));
    pub.mockResolvedValue(page([status("1")]));
    const tl = useTimelineStore();
    await tl.load("home");
    await tl.load("public");
    tl.buffer("home", status("4"));
    tl.buffer("home", status("1"));

    tl.replace({ ...status("1"), content: "v2" } as Status);
    expect(tl.get("home").items[0].content).toBe("v2");
    expect(tl.get("public").items[0].content).toBe("v2");
    expect(tl.get("home").pending.map((s) => s.id)).toEqual(["4"]);
  });

  it("buffer patches a displayed wrapper's reblog instead of pending", async () => {
    const wrapped = boost("9", status("2"));
    home.mockResolvedValue(page([wrapped]));
    const tl = useTimelineStore();
    await tl.load("home");

    tl.buffer("home", { ...status("2"), content: "edited" } as Status);
    expect(tl.get("home").pending).toHaveLength(0);
    expect(tl.get("home").items[0].reblog?.content).toBe("edited");
  });

  it("buffer keeps unseen updates pending", async () => {
    home.mockResolvedValue(page([status("1")]));
    const tl = useTimelineStore();
    await tl.load("home");
    tl.buffer("home", status("5"));
    expect(tl.get("home").pending.map((s) => s.id)).toEqual(["5"]);
    expect(tl.get("home").items.map((s) => s.id)).toEqual(["1"]);
  });
});
