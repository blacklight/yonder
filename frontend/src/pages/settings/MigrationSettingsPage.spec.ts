import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createMemoryHistory, createRouter } from "vue-router";
import { i18n } from "@/i18n";
import { mountWith, stubRoutes } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import MigrationSettingsPage from "./MigrationSettingsPage.vue";

vi.mock("@/api/settings", () => ({
  addAlias: vi.fn(),
  beginImportConnection: vi.fn(),
  cancelMove: vi.fn(),
  listAliases: vi.fn(),
  listImports: vi.fn(),
  moveAccount: vi.fn(),
  moveState: vi.fn(),
  removeAlias: vi.fn(),
}));
import {
  addAlias,
  beginImportConnection,
  listAliases,
  listImports,
  moveAccount,
  moveState,
  removeAlias,
} from "@/api/settings";

const idle = {
  acct: "me@old.example",
  moved: false,
  target: null,
  moved_at: null,
  cooldown_until: null,
  event: null,
  followers: null,
};

async function mountAt(route: string) {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: stubRoutes,
  });
  await router.push(route);
  const wrapper = mount(
    MigrationSettingsPage as never,
    {
      global: { plugins: [i18n, router] },
    } as never,
  );
  await flushPromises();
  return { wrapper, router };
}

describe("MigrationSettingsPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useAuthStore().setToken("tok");
    vi.mocked(listAliases).mockResolvedValue(["me@old.example"] as never);
    vi.mocked(moveState).mockResolvedValue(idle as never);
    vi.mocked(listImports).mockResolvedValue([] as never);
  });

  it("connects with move scopes and returns the callback to the wizard", async () => {
    vi.mocked(beginImportConnection).mockResolvedValue({
      authorization_url: "https://old.example/oauth/authorize",
    } as never);
    const { wrapper } = mountWith(MigrationSettingsPage);
    await flushPromises();
    const form = wrapper
      .findAll("form")
      .find((f) => f.text().includes("Authorize on old server"));
    expect(form).toBeTruthy();
    await form!.find("input").setValue("old.example");
    await form!.trigger("submit");
    await flushPromises();
    expect(beginImportConnection).toHaveBeenCalledWith(
      "old.example",
      ["following"],
      "/settings/migration",
    );
  });

  it("resumes the wizard on the import step after the OAuth callback", async () => {
    const { wrapper } = await mountAt("/settings/migration?connected=1");
    expect(wrapper.find(".mig__step--active h4").text()).toBe(
      "2. Import your data",
    );
  });

  it("shows a fixed reason for an unknown OAuth error code", async () => {
    const { wrapper } = await mountAt("/settings/migration?import_error=bogus");
    expect(wrapper.text()).toContain(
      "The remote authorization failed. Try again.",
    );
  });

  it("lists aliases and adds a new one", async () => {
    vi.mocked(addAlias).mockResolvedValue({} as never);
    const { wrapper } = mountWith(MigrationSettingsPage);
    await flushPromises();
    expect(wrapper.text()).toContain("me@old.example");
    const form = wrapper
      .findAll("form")
      .find((f) => f.text().includes("Add alias"));
    expect(form).toBeTruthy();
    await form!.find("input").setValue("me@new.example");
    await form!.trigger("submit");
    await flushPromises();
    expect(addAlias).toHaveBeenCalledWith("me@new.example");
  });

  it("removes an alias", async () => {
    vi.mocked(removeAlias).mockResolvedValue({} as never);
    const { wrapper } = mountWith(MigrationSettingsPage);
    await flushPromises();
    const remove = wrapper.findAll("button").find((b) => b.text() === "Remove");
    expect(remove).toBeTruthy();
    await remove!.trigger("click");
    await flushPromises();
    expect(removeAlias).toHaveBeenCalledWith("me@old.example");
  });

  it("submits a password-confirmed move", async () => {
    vi.mocked(moveAccount).mockResolvedValue({} as never);
    const { wrapper } = mountWith(MigrationSettingsPage);
    await flushPromises();
    const form = wrapper
      .findAll("form")
      .find((f) => f.text().includes("Move account"));
    expect(form).toBeTruthy();
    const target = form!
      .findAll("input")
      .find((i) =>
        (i.element as HTMLInputElement).placeholder.includes("new.example"),
      );
    const password = form!.find('input[type="password"]');
    expect(target && password.exists()).toBeTruthy();
    await target!.setValue("me@new.example");
    await password.setValue("correct-horse");
    await form!.trigger("submit");
    await flushPromises();
    expect(moveAccount).toHaveBeenCalledWith("me@new.example", "correct-horse");
  });

  it("shows cooldown + follower progress from move state", async () => {
    vi.mocked(moveState).mockResolvedValue({
      acct: "me@old.example",
      moved: true,
      target: "me@new.example",
      moved_at: "2026-01-01T00:00:00Z",
      cooldown_until: "2026-02-01T00:00:00Z",
      event: { id: "7", state: "migrating", error: null },
      followers: { migrated: 3, skipped: 0, failed: 0, pending: 7 },
    } as never);
    const { wrapper } = mountWith(MigrationSettingsPage);
    await flushPromises();
    expect(wrapper.text()).toContain("Migrating followers → me@new.example");
    expect(wrapper.text()).toContain("3/10");
    expect(wrapper.text()).not.toContain("undefined");
    // A live redirect offers cancel instead of a new move.
    expect(wrapper.text()).toContain("Cancel redirect");
  });

  it("shows idle without a raw state key", async () => {
    const { wrapper } = mountWith(MigrationSettingsPage);
    await flushPromises();
    expect(wrapper.text()).toContain("This account is not being moved.");
    expect(wrapper.text()).not.toContain("undefined");
    expect(wrapper.text()).toContain("Move account");
  });

  it("reports followers that failed to migrate", async () => {
    vi.mocked(moveState).mockResolvedValue({
      acct: "me@old.example",
      moved: true,
      target: "me@new.example",
      moved_at: null,
      cooldown_until: null,
      event: { id: "8", state: "migrating", error: null },
      followers: { migrated: 2, skipped: 0, failed: 1, pending: 0 },
    } as never);
    const { wrapper } = mountWith(MigrationSettingsPage);
    await flushPromises();
    expect(wrapper.text()).toContain("1 followers failed to migrate");
  });
});
