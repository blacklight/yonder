import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import { useComposeStore } from "@/stores/compose";
import ComposeBox from "./ComposeBox.vue";

vi.mock("@/api/emojis", () => ({
  customEmojis: vi.fn().mockResolvedValue([]),
}));
vi.mock("@/api/instance", () => ({
  getInstance: vi.fn().mockResolvedValue({
    configuration: {
      statuses: { max_characters: 500, max_media_attachments: 4 },
      polls: { max_options: 4, min_expiration: 300, max_expiration: 2629746 },
    },
  }),
  composeLimits: vi.fn().mockReturnValue({
    maxCharacters: 500,
    maxMediaAttachments: 4,
    poll: {
      maxOptions: 4,
      maxCharactersPerOption: 80,
      minExpiration: 300,
      maxExpiration: 2629746,
    },
  }),
}));

function mockPost(body: unknown = { id: "1" }) {
  return vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(() =>
      Promise.resolve(new Response(JSON.stringify(body), { status: 200 })),
    );
}

describe("ComposeBox", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    localStorage.clear();
    useAuthStore().setToken("t");
    useComposeStore().reset();
  });

  it("posts with Idempotency-Key and resets", async () => {
    const spy = mockPost();
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    await wrapper.get("[data-testid='compose-text']").setValue("hello world");
    await wrapper.get("[data-testid='post']").trigger("click");
    await flushPromises();
    const [, init] = spy.mock.calls[0];
    expect((init!.headers as Headers).get("Idempotency-Key")).toBeTruthy();
    expect(JSON.parse(init!.body as string).status).toBe("hello world");
    expect(useComposeStore().draft.text).toBe("");
  });

  it("Ctrl+Enter submits from the textarea, plain Enter does not", async () => {
    const spy = mockPost();
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    const text = wrapper.get("[data-testid='compose-text']");
    await text.setValue("hello world");
    await text.trigger("keydown", { key: "Enter" });
    await flushPromises();
    expect(spy).not.toHaveBeenCalled();
    await text.trigger("keydown", { key: "Enter", ctrlKey: true });
    await flushPromises();
    const body = JSON.parse(spy.mock.calls[0][1]!.body as string);
    expect(body.status).toBe("hello world");
  });

  it("Cmd+Enter (macOS) also submits", async () => {
    const spy = mockPost();
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    const text = wrapper.get("[data-testid='compose-text']");
    await text.setValue("meta submit");
    await text.trigger("keydown", { key: "Enter", metaKey: true });
    await flushPromises();
    expect(spy).toHaveBeenCalled();
  });

  it("Ctrl+Enter does not bypass posting limits", async () => {
    const spy = mockPost();
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    const text = wrapper.get("[data-testid='compose-text']");
    await text.setValue("x".repeat(501));
    await text.trigger("keydown", { key: "Enter", ctrlKey: true });
    await flushPromises();
    expect(spy).not.toHaveBeenCalled();
  });

  it("disables post over the character limit", async () => {
    mockPost();
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    await wrapper.get("[data-testid='compose-text']").setValue("x".repeat(501));
    expect(wrapper.get("[data-testid='counter']").text()).toContain(
      "501 / 500",
    );
    expect(
      wrapper.get("[data-testid='post']").attributes("disabled"),
    ).toBeDefined();
  });

  it("reply context sets in_reply_to_id without manual mentions", async () => {
    const spy = mockPost();
    const compose = useComposeStore();
    compose.openReply({
      id: "42",
      account: { acct: "bob" },
      reblog: null,
    } as never);
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    await wrapper.get("[data-testid='post']").trigger("click");
    await flushPromises();
    const body = JSON.parse(spy.mock.calls[0][1]!.body as string);
    expect(body.in_reply_to_id).toBe("42");
    expect(body.status).toBe("@bob ");
  });

  it("scheduled posts carry scheduled_at", async () => {
    const spy = mockPost();
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    await wrapper.get("[data-testid='compose-text']").setValue("later");
    useComposeStore().update({ scheduledAt: "2030-01-01T00:00:00.000Z" });
    await wrapper.get("[data-testid='post']").trigger("click");
    await flushPromises();
    const body = JSON.parse(spy.mock.calls[0][1]!.body as string);
    expect(body.scheduled_at).toBe("2030-01-01T00:00:00.000Z");
  });

  it("reuses the idempotency key for an unchanged retry, mints a new one on edit", async () => {
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(
        new Response(JSON.stringify({ error: "boom" }), { status: 500 }),
      ),
    );
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    await wrapper.get("[data-testid='compose-text']").setValue("first");
    const post = () => wrapper.get("[data-testid='post']").trigger("click");

    await post();
    await flushPromises();
    const key1 = (vi.mocked(fetch).mock.calls[0][1]!.headers as Headers).get(
      "Idempotency-Key",
    );
    // Same body → same key (replay is safe server-side).
    await post();
    await flushPromises();
    const key2 = (vi.mocked(fetch).mock.calls[1][1]!.headers as Headers).get(
      "Idempotency-Key",
    );
    expect(key2).toBe(key1);
    // Edited body → a new key so a committed-but-timed-out first post
    // can't silently swallow the change.
    await wrapper.get("[data-testid='compose-text']").setValue("changed");
    await post();
    await flushPromises();
    const key3 = (vi.mocked(fetch).mock.calls[2][1]!.headers as Headers).get(
      "Idempotency-Key",
    );
    expect(key3).not.toBe(key1);
  });

  it("a poll with fewer than two options cannot be posted", async () => {
    mockPost();
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    await wrapper.get("[data-testid='compose-text']").setValue("poll time");
    await wrapper.get("[data-testid='poll-toggle']").trigger("click");
    // Two blank options → post stays disabled.
    expect(
      wrapper.get("[data-testid='post']").attributes("disabled"),
    ).toBeDefined();
    const inputs = wrapper.findAll(".poll__option");
    await inputs[0].setValue("yes");
    await inputs[1].setValue("no");
    expect(
      wrapper.get("[data-testid='post']").attributes("disabled"),
    ).toBeUndefined();
  });

  it("media and poll are mutually exclusive", async () => {
    mockPost({ id: "m1", url: "/m/1", preview_url: "/m/1", type: "image" });
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    // Open the poll builder.
    await wrapper.get("[data-testid='poll-toggle']").trigger("click");
    expect(useComposeStore().draft.poll).toBeTruthy();
    // Selecting media clears the poll (and poll button disables with media).
    const input = wrapper.get("[data-testid='media-input']");
    Object.defineProperty(input.element, "files", {
      value: [new File(["x"], "a.png", { type: "image/png" })],
    });
    await input.trigger("change");
    await flushPromises();
    expect(useComposeStore().draft.poll).toBeNull();
    expect(useComposeStore().draft.mediaIds).toContain("m1");
  });

  it("blocks posting a schedule time in the past", async () => {
    mockPost();
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    await wrapper.get("[data-testid='compose-text']").setValue("later");
    useComposeStore().update({ scheduledAt: "2000-01-01T00:00:00.000Z" });
    await flushPromises();
    expect(
      wrapper.get("[data-testid='post']").attributes("disabled"),
    ).toBeDefined();
    expect(wrapper.text()).toContain("future");
  });

  it("rehydrates persisted draft media ids on reload", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockImplementation((input) => {
      const url = String(input);
      const ok = url.includes("/api/v2/media/m9");
      return Promise.resolve(
        new Response(
          JSON.stringify(
            ok
              ? { id: "m9", url: "/m/9", preview_url: "/m/9", type: "image" }
              : { error: "not found" },
          ),
          { status: ok ? 200 : 404 },
        ),
      );
    });
    useComposeStore().update({ mediaIds: ["m9", "gone"] });
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    // The missing id drops out of the draft; the live one previews.
    const called = spy.mock.calls.map(([u]) => String(u));
    expect(called.some((u) => u.includes("/api/v2/media/m9"))).toBe(true);
    expect(called.some((u) => u.includes("/api/v2/media/gone"))).toBe(true);
    expect(wrapper.findAll(".compose__media-item")).toHaveLength(1);
    expect(useComposeStore().draft.mediaIds).toEqual(["m9"]);
  });

  it("keeps draft media ids when rehydration fails transiently", async () => {
    vi.spyOn(globalThis, "fetch").mockImplementation((input) => {
      const url = String(input);
      const ok = url.includes("/api/v2/media/m9");
      return Promise.resolve(
        new Response(
          JSON.stringify(
            ok
              ? { id: "m9", url: "/m/9", preview_url: "/m/9", type: "image" }
              : { error: "upstream" },
          ),
          { status: ok ? 200 : 500 },
        ),
      );
    });
    useComposeStore().update({ mediaIds: ["m9", "flaky"] });
    const { wrapper } = mountWith(ComposeBox);
    await flushPromises();
    // A 500 is transient: the id stays as a placeholder tile instead
    // of being dropped from the persisted draft.
    expect(wrapper.findAll(".compose__media-item")).toHaveLength(2);
    expect(useComposeStore().draft.mediaIds).toEqual(["m9", "flaky"]);
  });
});
