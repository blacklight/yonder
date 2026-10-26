import { describe, expect, it, vi, beforeEach } from "vitest";
import { fileReport } from "./reports";
import { blockDomain, domainBlocks, unblockDomain } from "./domains";
import { customEmojis } from "./emojis";
import {
  clearNotifications,
  dismissNotification,
  notifications,
  unreadCount,
} from "./notifications";
import { adminAccountAction, adminStatusAction } from "./admin";
import { composeLimits, getInstance } from "./instance";

function mockFetch(status: number, body: unknown) {
  return vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(() =>
      Promise.resolve(new Response(JSON.stringify(body), { status })),
    );
}

describe("reports api", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("fileReport POSTs the report body", async () => {
    const spy = mockFetch(200, { id: "1" });
    await fileReport({
      account_id: 7,
      status_ids: [11],
      comment: "spam",
      category: "spam",
      forward: true,
    });
    const [url, init] = spy.mock.calls[0];
    expect(url).toBe("/api/v1/reports");
    expect(init!.method).toBe("POST");
    expect(JSON.parse(init!.body as string)).toEqual({
      account_id: 7,
      status_ids: [11],
      comment: "spam",
      category: "spam",
      forward: true,
    });
  });
});

describe("domains api", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("lists, blocks and unblocks domains", async () => {
    const spy = mockFetch(200, []);
    await domainBlocks();
    expect(spy.mock.calls[0][0]).toBe("/api/v1/domain_blocks");
    await blockDomain("spam.example");
    expect(spy.mock.calls[1][0]).toBe("/api/v1/domain_blocks");
    expect(JSON.parse(spy.mock.calls[1][1]!.body as string)).toEqual({
      domain: "spam.example",
    });
    await unblockDomain("spam.example");
    expect(spy.mock.calls[2][0]).toBe("/api/v1/domain_blocks");
    expect(spy.mock.calls[2][1]!.method).toBe("DELETE");
    expect(JSON.parse(spy.mock.calls[2][1]!.body as string)).toEqual({
      domain: "spam.example",
    });
  });
});

describe("emojis api", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("customEmojis GETs anonymously", async () => {
    const spy = mockFetch(200, [{ shortcode: "blob", url: "/e/blob.png" }]);
    const emojis = await customEmojis();
    expect(spy.mock.calls[0][0]).toBe("/api/v1/custom_emojis");
    expect(emojis[0].shortcode).toBe("blob");
  });
});

describe("notifications api", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("notifications sends repeated types[]/exclude_types[] params", async () => {
    const spy = mockFetch(200, []);
    await notifications({
      types: ["mention"],
      exclude_types: ["poll", "update"],
      limit: 30,
      max_id: "50",
    });
    expect(spy.mock.calls[0][0]).toBe(
      "/api/v1/notifications?max_id=50&limit=30&types%5B%5D=mention&exclude_types%5B%5D=poll&exclude_types%5B%5D=update",
    );
  });

  it("unreadCount unwraps {count}", async () => {
    mockFetch(200, { count: 7 });
    expect(await unreadCount()).toBe(7);
  });

  it("clear + dismiss issue POSTs", async () => {
    const spy = mockFetch(200, null);
    await clearNotifications();
    expect(spy.mock.calls[0][0]).toBe("/api/v1/notifications/clear");
    expect(spy.mock.calls[0][1]!.method).toBe("POST");
    await dismissNotification("12");
    expect(spy.mock.calls[1][0]).toBe("/api/v1/notifications/12/dismiss");
    expect(spy.mock.calls[1][1]!.method).toBe("POST");
  });
});

describe("admin api", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("adminStatusAction POSTs {type}", async () => {
    const spy = mockFetch(200, { applied: true });
    await adminStatusAction("33", "sensitive");
    expect(spy.mock.calls[0][0]).toBe("/api/v1/admin/statuses/33/action");
    expect(JSON.parse(spy.mock.calls[0][1]!.body as string)).toEqual({
      type: "sensitive",
    });
  });

  it("adminAccountAction POSTs the action body", async () => {
    const spy = mockFetch(200, {});
    await adminAccountAction("9", { type: "suspend", text: "spam" });
    expect(spy.mock.calls[0][0]).toBe("/api/v1/admin/accounts/9/action");
    expect(JSON.parse(spy.mock.calls[0][1]!.body as string)).toEqual({
      type: "suspend",
      text: "spam",
    });
  });
});

describe("instance api", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("getInstance GETs /instance", async () => {
    const spy = mockFetch(200, { title: "Yonder" });
    await getInstance();
    expect(spy.mock.calls[0][0]).toBe("/api/v1/instance");
  });

  it("composeLimits reads configuration with fallbacks", () => {
    const limits = composeLimits({
      configuration: {
        statuses: { max_characters: 500, max_media_attachments: 8 },
        polls: { max_options: 5 },
      },
    });
    expect(limits.maxCharacters).toBe(500);
    expect(limits.maxMediaAttachments).toBe(8);
    expect(limits.poll.maxOptions).toBe(5);
    expect(limits.poll.maxCharactersPerOption).toBe(80);
    expect(composeLimits(null).maxCharacters).toBe(5000);
  });
});
