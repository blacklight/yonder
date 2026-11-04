import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createMemoryHistory, createRouter } from "vue-router";
import { i18n } from "@/i18n";
import { stubRoutes } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import DataSettingsPage from "./DataSettingsPage.vue";

vi.mock("@/api/settings", () => ({
  beginImportConnection: vi.fn(),
  cancelImport: vi.fn(),
  createImport: vi.fn(),
  createImportSession: vi.fn(),
  downloadExport: vi.fn(),
  downloadInlineExport: vi.fn(),
  listExports: vi.fn(),
  listImportConnections: vi.fn(),
  listImports: vi.fn(),
  requestArchiveExport: vi.fn(),
  resumeImport: vi.fn(),
  revokeImportConnection: vi.fn(),
}));
import {
  beginImportConnection,
  createImport,
  createImportSession,
  listExports,
  listImportConnections,
  listImports,
} from "@/api/settings";

const conn = {
  id: "9",
  origin: "https://old.example",
  source_actor_uri: "https://old.example/users/me",
  source_acct: "me@old.example",
  scopes: "read",
  state: "active",
  created_at: "2026-01-01T00:00:00Z",
};

async function mountPage(
  route = "/",
  connections = [conn],
  imports: unknown[] = [],
) {
  vi.mocked(listExports).mockResolvedValue([] as never);
  vi.mocked(listImportConnections).mockResolvedValue(connections as never);
  vi.mocked(listImports).mockResolvedValue(imports as never);
  const router = createRouter({
    history: createMemoryHistory(),
    routes: stubRoutes,
  });
  await router.push(route);
  const wrapper = mount(
    DataSettingsPage as never,
    {
      global: { plugins: [i18n, router] },
    } as never,
  );
  await flushPromises();
  return { wrapper, router };
}

describe("DataSettingsPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useAuthStore().setToken("tok");
  });

  it("shows the connected notice after the OAuth callback", async () => {
    const { wrapper } = await mountPage("/settings/data?connected=1");
    expect(wrapper.text()).toContain("Remote account connected");
  });

  it("shows a fixed reason after a failed OAuth callback", async () => {
    const { wrapper } = await mountPage(
      "/settings/data?import_error=forbidden",
    );
    expect(wrapper.text()).toContain("refused the authorization");
  });

  it("queues an upload import with kind/mode/file", async () => {
    vi.mocked(createImport).mockResolvedValue({
      id: "1",
      kind: "archive",
      state: "pending",
      processed: 0,
      total: 1,
      failed: 0,
      file_size: null,
      error: null,
      created_at: "2026-01-01T00:00:00Z",
      completed_at: null,
      expires_at: null,
    } as never);
    const { wrapper } = await mountPage("/settings/data");
    const fileInput = wrapper.find('input[type="file"]');
    const file = new File(["PK"], "export.zip");
    Object.defineProperty(fileInput.element, "files", { value: [file] });
    await fileInput.trigger("change");
    const forms = wrapper.findAll("form");
    const uploadForm = forms.find((f) => f.text().includes("Start import"));
    expect(uploadForm).toBeTruthy();
    await uploadForm!.trigger("submit");
    await flushPromises();
    expect(createImport).toHaveBeenCalledWith("archive", "merge", file, false);
  });

  it("sends the selected categories when connecting", async () => {
    vi.mocked(beginImportConnection).mockResolvedValue({
      authorization_url: "https://old.example/oauth/authorize",
    } as never);
    const { wrapper } = await mountPage("/settings/data");
    // `following` is index 4 in the category list.
    const boxes = wrapper.findAll('.data__cats input[type="checkbox"]');
    expect(boxes.length).toBe(9);
    await boxes[4].setValue(true);
    const form = wrapper
      .findAll("form")
      .find((f) => f.text().includes("Connect"));
    expect(form).toBeTruthy();
    await form!.find("input").setValue("old.example");
    await form!.trigger("submit");
    await flushPromises();
    expect(beginImportConnection).toHaveBeenCalledWith("old.example", [
      "following",
    ]);
  });

  it("requires a category before connecting", async () => {
    const { wrapper } = await mountPage("/settings/data");
    expect(wrapper.text()).toContain("Select at least one category");
    const form = wrapper
      .findAll("form")
      .find((f) => f.text().includes("Connect"));
    expect(form!.find("button").attributes("disabled")).toBeDefined();
    await form!.find("input").setValue("old.example");
    await form!.trigger("submit");
    await flushPromises();
    expect(beginImportConnection).not.toHaveBeenCalled();
  });

  it("reauthorizes a narrow grant with the categories it already covers", async () => {
    vi.mocked(beginImportConnection).mockResolvedValue({
      authorization_url: "https://old.example/oauth/authorize",
    } as never);
    const { wrapper } = await mountPage("/settings/data", [
      { ...conn, scopes: "read:accounts read:follows" },
    ]);
    const btn = wrapper
      .findAll("button")
      .find((b) => b.text() === "Reauthorize");
    await btn!.trigger("click");
    await flushPromises();
    expect(beginImportConnection).toHaveBeenCalledWith("https://old.example", [
      "following",
    ]);
  });

  it("summarises per-category status, including failures", async () => {
    const { wrapper } = await mountPage(
      "/settings/data",
      [conn],
      [
        {
          id: "3",
          kind: "archive",
          state: "done",
          processed: 2,
          total: 3,
          failed: 1,
          file_size: null,
          error: null,
          source_origin: "https://old.example",
          created_at: "2026-01-01T00:00:00Z",
          completed_at: null,
          expires_at: null,
          category_state: {
            statuses: { done: true, processed: 120 },
            following: { done: true, error: "rate limited" },
            dms: {},
          },
        },
      ],
    );
    expect(wrapper.text()).toContain("Posts & replies: done, 120 processed");
    expect(wrapper.text()).toContain("Following: failed");
    expect(wrapper.text()).toContain("pending");
    expect(wrapper.text()).not.toContain("[object Object]");
  });

  it("reauthorizes with the union of granted and selected scopes", async () => {
    vi.mocked(beginImportConnection).mockResolvedValue({
      authorization_url: "https://old.example/oauth/authorize",
    } as never);
    const { wrapper } = await mountPage("/settings/data");
    // The fixture's grant is broad `read`, so reauthorization must
    // not narrow it even with no category selected.
    const btn = wrapper
      .findAll("button")
      .find((b) => b.text() === "Reauthorize");
    expect(btn).toBeTruthy();
    await btn!.trigger("click");
    await flushPromises();
    expect(beginImportConnection).toHaveBeenCalledWith(
      "https://old.example",
      expect.arrayContaining([
        "statuses",
        "dms",
        "bookmarks",
        "favourites",
        "following",
        "blocks",
        "mutes",
        "profile",
        "observations",
      ]),
    );
  });

  it("creates a remote session with the selected connection", async () => {
    vi.mocked(createImportSession).mockResolvedValue({
      id: "5",
      state: "pending",
    } as never);
    const { wrapper } = await mountPage("/settings/data");
    const select = wrapper.find("select[required]");
    expect(select.exists()).toBe(true);
    await select.setValue("9");
    const sessionForm = wrapper
      .findAll("form")
      .find((f) => f.text().includes("Start remote import"));
    expect(sessionForm).toBeTruthy();
    await sessionForm!.trigger("submit");
    await flushPromises();
    expect(createImportSession).toHaveBeenCalledWith(
      expect.objectContaining({ connection_id: "9", order: "newest" }),
    );
  });
});
