import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import TokensSettingsPage from "./TokensSettingsPage.vue";

vi.mock("@/api/settings", () => ({
  createToken: vi.fn(),
  listTokens: vi.fn(),
  revokeToken: vi.fn(),
}));
import { createToken, listTokens, revokeToken } from "@/api/settings";

const mockedList = vi.mocked(listTokens);
const mockedCreate = vi.mocked(createToken);
const mockedRevoke = vi.mocked(revokeToken);

const tokenRows = [
  {
    id: "1",
    name: "backup script",
    scopes: ["read"],
    created_at: "2026-10-01T09:00:00Z",
    expires_at: null,
  },
  {
    id: "2",
    name: "posting bot",
    scopes: ["read", "write"],
    created_at: "2026-10-02T09:00:00Z",
    expires_at: "2020-01-01T00:00:00Z",
  },
];

async function mountPage(rows: unknown[] = tokenRows) {
  mockedList.mockResolvedValue(rows as never);
  const mounted = mountWith(TokensSettingsPage);
  await flushPromises();
  return mounted;
}

describe("TokensSettingsPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useAuthStore().setToken("tok");
  });

  it("lists the active tokens with scope and expiry metadata", async () => {
    const { wrapper } = await mountPage();
    const rows = wrapper.findAll(".tokens__row");
    expect(rows).toHaveLength(2);
    expect(rows[0].text()).toContain("backup script");
    expect(rows[0].text()).toContain("never expires");
    expect(rows[1].text()).toContain("read write");
    // The second row is past its expires_at.
    expect(rows[1].find(".tokens__badge").text()).toBe("Expired");
  });

  it("shows the empty state", async () => {
    const { wrapper } = await mountPage([]);
    expect(wrapper.find(".tokens__list").exists()).toBe(false);
    expect(wrapper.text()).toContain("No API tokens.");
  });

  it("creates a token and reveals the plaintext once", async () => {
    mockedCreate.mockResolvedValue({
      id: "3",
      name: "ci",
      token: "plaintext-secret",
    } as never);
    const { wrapper } = await mountPage();

    await wrapper.get(".tokens__form input").setValue("ci");
    await wrapper
      .findAll(".tokens__check input")
      .find((c) => (c.element as HTMLInputElement).value === "write")!
      .setValue(true);
    await wrapper.get(".tokens__form").trigger("submit");
    await flushPromises();

    expect(mockedCreate).toHaveBeenCalledWith({
      name: "ci",
      scopes: "read write",
    });
    expect(wrapper.get(".tokens__secret").text()).toBe("plaintext-secret");
  });

  it("passes expires_in for a selected TTL", async () => {
    mockedCreate.mockResolvedValue({
      id: "3",
      name: "ci",
      token: "x",
    } as never);
    const { wrapper } = await mountPage();

    await wrapper.get(".tokens__form input").setValue("ci");
    await wrapper.get("select").setValue("604800");
    await wrapper.get(".tokens__form").trigger("submit");
    await flushPromises();

    expect(mockedCreate).toHaveBeenCalledWith({
      name: "ci",
      scopes: "read",
      expires_in: 604800,
    });
  });

  it("requires a scope without calling the API", async () => {
    const { wrapper } = await mountPage();
    await wrapper.get(".tokens__form input").setValue("ci");
    await wrapper.get(".tokens__check input").setValue(false);
    await wrapper.get(".tokens__form").trigger("submit");
    await flushPromises();

    expect(mockedCreate).not.toHaveBeenCalled();
    expect(wrapper.find(".tokens__error").exists()).toBe(true);
  });

  it("copies the revealed token to the clipboard", async () => {
    mockedCreate.mockResolvedValue({
      id: "3",
      name: "ci",
      token: "plaintext-secret",
    } as never);
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    const { wrapper } = await mountPage();

    await wrapper.get(".tokens__form input").setValue("ci");
    await wrapper.get(".tokens__form").trigger("submit");
    await flushPromises();
    await wrapper
      .findAll(".tokens__reveal button")
      .find((b) => b.text() === "Copy token")!
      .trigger("click");
    await flushPromises();

    expect(writeText).toHaveBeenCalledWith("plaintext-secret");
  });

  it("revokes a token after a two-click confirm", async () => {
    mockedRevoke.mockResolvedValue({} as never);
    const { wrapper } = await mountPage();
    const row = wrapper.findAll(".tokens__row")[0];

    await row
      .findAll("button")
      .find((b) => b.text() === "Revoke")!
      .trigger("click");
    expect(mockedRevoke).not.toHaveBeenCalled();

    await row
      .findAll("button")
      .find((b) => b.text() === "Revoke?")!
      .trigger("click");
    await flushPromises();

    expect(mockedRevoke).toHaveBeenCalledWith("1");
    expect(wrapper.findAll(".tokens__row")).toHaveLength(1);
  });

  it("only offers the admin scope to staff", async () => {
    const { wrapper } = await mountPage();
    expect(wrapper.text()).not.toContain("administrative access");

    const auth = useAuthStore();
    auth.role = "admin";
    const second = mountWith(TokensSettingsPage);
    await flushPromises();
    expect(second.wrapper.text()).toContain("administrative access");
  });
});
