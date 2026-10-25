import { describe, expect, it, vi, beforeEach } from "vitest";
import {
  bookmark,
  createStatus,
  deleteScheduledStatus,
  deleteStatus,
  favourite,
  homeTimeline,
  listTimeline,
  mediaProcessing,
  pin,
  publicTimeline,
  react,
  reactionsList,
  reblog,
  scheduledStatuses,
  tagTimeline,
  unbookmark,
  unfavourite,
  unpin,
  unreblog,
  unreact,
  updateMedia,
  uploadMedia,
  votePoll,
} from "./statuses";

function mockFetch(
  status: number,
  body: unknown,
  headers: Record<string, string> = {},
) {
  return vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(() =>
      Promise.resolve(new Response(JSON.stringify(body), { status, headers })),
    );
}

describe("statuses api", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("homeTimeline GETs /timelines/home with pagination", async () => {
    const spy = mockFetch(200, [{ id: "1" }], {
      link: '</next-url>; rel="next"',
    });
    const page = await homeTimeline({ limit: 20 });
    expect(spy.mock.calls[0][0]).toBe("/api/v1/timelines/home?limit=20");
    expect(page.items).toHaveLength(1);
    expect(page.next).toBe("/next-url");
  });

  it("publicTimeline passes local/remote/only_media", async () => {
    const spy = mockFetch(200, []);
    await publicTimeline({ local: true, only_media: true });
    expect(spy.mock.calls[0][0]).toBe(
      "/api/v1/timelines/public?local=true&only_media=true",
    );
  });

  it("tagTimeline and listTimeline encode the path segment", async () => {
    const spy = mockFetch(200, []);
    await tagTimeline("rust lang", { max_id: "9" });
    expect(spy.mock.calls[0][0]).toBe(
      "/api/v1/timelines/tag/rust%20lang?max_id=9",
    );
    await listTimeline("7");
    expect(spy.mock.calls[1][0]).toBe("/api/v1/timelines/list/7");
  });

  it("createStatus posts the body plus the Idempotency-Key", async () => {
    const spy = mockFetch(200, { id: "9" });
    await createStatus({ status: "hi", visibility: "public" }, "key-1");
    const [url, init] = spy.mock.calls[0];
    expect(url).toBe("/api/v1/statuses");
    expect(init!.method).toBe("POST");
    expect(JSON.parse(init!.body as string)).toEqual({
      status: "hi",
      visibility: "public",
    });
    expect((init!.headers as Headers).get("Idempotency-Key")).toBe("key-1");
  });

  it("omits Idempotency-Key when not given", async () => {
    const spy = mockFetch(200, { id: "9" });
    await createStatus({ status: "hi" });
    const [, init] = spy.mock.calls[0];
    expect((init!.headers as Headers).get("Idempotency-Key")).toBeNull();
  });

  it("deleteStatus DELETEs the row", async () => {
    const spy = mockFetch(200, { id: "3" });
    await deleteStatus("3");
    expect(spy.mock.calls[0][0]).toBe("/api/v1/statuses/3");
    expect(spy.mock.calls[0][1]!.method).toBe("DELETE");
  });

  it.each([
    ["favourite", favourite, "POST", "/api/v1/statuses/5/favourite"],
    ["unfavourite", unfavourite, "POST", "/api/v1/statuses/5/unfavourite"],
    ["unreblog", unreblog, "POST", "/api/v1/statuses/5/unreblog"],
    ["bookmark", bookmark, "POST", "/api/v1/statuses/5/bookmark"],
    ["unbookmark", unbookmark, "POST", "/api/v1/statuses/5/unbookmark"],
    ["pin", pin, "POST", "/api/v1/statuses/5/pin"],
    ["unpin", unpin, "POST", "/api/v1/statuses/5/unpin"],
  ] as const)("%s issues %s %s", async (_name, fn, method, url) => {
    const spy = mockFetch(200, {});
    await (fn as (id: string) => Promise<unknown>)("5");
    expect(spy.mock.calls[0][0]).toBe(url);
    expect(spy.mock.calls[0][1]!.method).toBe(method);
  });

  it("reblog sends an optional visibility body", async () => {
    const spy = mockFetch(200, {});
    await reblog("5", "unlisted");
    expect(spy.mock.calls[0][0]).toBe("/api/v1/statuses/5/reblog");
    expect(JSON.parse(spy.mock.calls[0][1]!.body as string)).toEqual({
      visibility: "unlisted",
    });
  });

  it("react PUTs and unreact DELETEs the encoded emoji segment", async () => {
    const spy = mockFetch(200, {});
    await react("5", "blob_cat");
    expect(spy.mock.calls[0][0]).toBe(
      "/api/v1/pleroma/statuses/5/reactions/blob_cat",
    );
    expect(spy.mock.calls[0][1]!.method).toBe("PUT");
    await unreact("5", "🎉");
    expect(spy.mock.calls[1][0]).toBe(
      "/api/v1/pleroma/statuses/5/reactions/%F0%9F%8E%89",
    );
    expect(spy.mock.calls[1][1]!.method).toBe("DELETE");
  });

  it("reactionsList takes only the status id", async () => {
    const spy = mockFetch(200, []);
    await reactionsList("5");
    expect(spy.mock.calls[0][0]).toBe("/api/v1/pleroma/statuses/5/reactions");
    expect(spy.mock.calls[0][1]!.method).toBe("GET");
  });

  it("votePoll posts {choices}", async () => {
    const spy = mockFetch(200, {});
    await votePoll("8", [0, 2]);
    expect(spy.mock.calls[0][0]).toBe("/api/v1/polls/8/votes");
    expect(JSON.parse(spy.mock.calls[0][1]!.body as string)).toEqual({
      choices: [0, 2],
    });
  });

  it("uploadMedia POSTs multipart to /api/v2/media", async () => {
    const spy = mockFetch(200, { id: "m1" });
    const file = new File(["x"], "a.png", { type: "image/png" });
    await uploadMedia(file, { description: "alt" });
    const [url, init] = spy.mock.calls[0];
    expect(url).toBe("/api/v2/media");
    expect(init!.method).toBe("POST");
    const form = init!.body as FormData;
    expect(form.get("file")).toBeTruthy();
    expect(form.get("description")).toBe("alt");
  });

  it("mediaProcessing GETs /api/v2/media/{id}", async () => {
    const spy = mockFetch(200, { id: "m1" });
    await mediaProcessing("m1");
    expect(spy.mock.calls[0][0]).toBe("/api/v2/media/m1");
  });

  it("updateMedia PUTs to /api/v1/media/{id}", async () => {
    const spy = mockFetch(200, { id: "m1" });
    await updateMedia("m1", { description: "new alt" });
    expect(spy.mock.calls[0][0]).toBe("/api/v1/media/m1");
    expect(spy.mock.calls[0][1]!.method).toBe("PUT");
    expect(JSON.parse(spy.mock.calls[0][1]!.body as string)).toEqual({
      description: "new alt",
    });
  });

  it("scheduled statuses list + delete", async () => {
    const spy = mockFetch(200, []);
    await scheduledStatuses();
    expect(spy.mock.calls[0][0]).toBe("/api/v1/scheduled_statuses");
    await deleteScheduledStatus("4");
    expect(spy.mock.calls[1][0]).toBe("/api/v1/scheduled_statuses/4");
    expect(spy.mock.calls[1][1]!.method).toBe("DELETE");
  });
});
