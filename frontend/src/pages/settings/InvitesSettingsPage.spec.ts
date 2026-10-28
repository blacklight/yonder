import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import InvitesSettingsPage from "./InvitesSettingsPage.vue";

vi.mock("@/api/settings", () => ({
  createInvite: vi.fn(),
  listInvites: vi.fn(),
  revokeInvite: vi.fn(),
}));
vi.mock("@/api/instance", () => ({
  getInstance: vi.fn(),
}));
import { createInvite, listInvites, revokeInvite } from "@/api/settings";
import { getInstance } from "@/api/instance";

const mockedList = vi.mocked(listInvites);
const mockedCreate = vi.mocked(createInvite);
const mockedRevoke = vi.mocked(revokeInvite);
const mockedInstance = vi.mocked(getInstance);

const inviteRows = [
  {
    id: "1",
    code: "abc123",
    url: "https://example.com/invite/abc123",
    uses: 2,
    max_uses: 10,
    created_at: "2026-10-01T09:00:00Z",
    expires_at: null,
  },
  {
    id: "2",
    code: "def456",
    url: "https://example.com/invite/def456",
    uses: 0,
    max_uses: null,
    created_at: "2026-10-02T09:00:00Z",
    expires_at: "2020-01-01T00:00:00Z",
  },
];

async function mountPage(
  rows: unknown[] = inviteRows,
  instance: unknown = { registrations: { invites_enabled: true } },
) {
  mockedList.mockResolvedValue(rows as never);
  mockedInstance.mockResolvedValue(instance as never);
  const mounted = mountWith(InvitesSettingsPage);
  await flushPromises();
  return mounted;
}

describe("InvitesSettingsPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useAuthStore().setToken("tok");
  });

  it("lists invites with uses and expiry metadata", async () => {
    const { wrapper } = await mountPage();
    const rows = wrapper.findAll(".invites__row");
    expect(rows).toHaveLength(2);
    expect(rows[0].text()).toContain("https://example.com/invite/abc123");
    expect(rows[0].text()).toContain("2 / 10");
    expect(rows[0].text()).toContain("never expires");
    // The second row is past its expires_at.
    expect(rows[1].find(".invites__badge").text()).toBe("Expired");
  });

  it("marks an exhausted invite", async () => {
    const { wrapper } = await mountPage([
      {
        id: "9",
        code: "full",
        url: "https://example.com/invite/full",
        uses: 5,
        max_uses: 5,
        created_at: "2026-10-01T09:00:00Z",
        expires_at: null,
      },
    ]);
    expect(wrapper.find(".invites__badge").text()).toBe("Exhausted");
  });

  it("shows the empty state", async () => {
    const { wrapper } = await mountPage([]);
    expect(wrapper.find(".invites__list").exists()).toBe(false);
    expect(wrapper.text()).toContain("No invite links.");
  });

  it("creates an invite and prepends it to the list", async () => {
    mockedCreate.mockResolvedValue({
      id: "3",
      code: "new99",
      url: "https://example.com/invite/new99",
      uses: 0,
      max_uses: null,
      expires_at: null,
    } as never);
    const { wrapper } = await mountPage();

    await wrapper.get(".invites__form").trigger("submit");
    await flushPromises();

    expect(mockedCreate).toHaveBeenCalledWith({});
    const rows = wrapper.findAll(".invites__row");
    expect(rows).toHaveLength(3);
    expect(rows[0].text()).toContain("https://example.com/invite/new99");
  });

  it("passes max_uses and expires_in when selected", async () => {
    mockedCreate.mockResolvedValue({
      id: "3",
      code: "x",
      url: "https://example.com/invite/x",
      uses: 0,
      max_uses: 5,
      expires_at: null,
    } as never);
    const { wrapper } = await mountPage();

    const selects = wrapper.findAll("select");
    await selects[0].setValue("5");
    await selects[1].setValue("604800");
    await wrapper.get(".invites__form").trigger("submit");
    await flushPromises();

    expect(mockedCreate).toHaveBeenCalledWith({
      max_uses: 5,
      expires_in: 604800,
    });
  });

  it("copies the invite url to the clipboard", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    const { wrapper } = await mountPage();

    const row = wrapper.findAll(".invites__row")[0];
    await row
      .findAll("button")
      .find((b) => b.text() === "Copy link")!
      .trigger("click");
    await flushPromises();

    expect(writeText).toHaveBeenCalledWith("https://example.com/invite/abc123");
  });

  it("revokes an invite after a two-click confirm", async () => {
    mockedRevoke.mockResolvedValue({} as never);
    const { wrapper } = await mountPage();
    const row = wrapper.findAll(".invites__row")[0];

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
    expect(wrapper.findAll(".invites__row")).toHaveLength(1);
  });

  it("hides the create form for non-staff when invites are disabled", async () => {
    const { wrapper } = await mountPage(inviteRows, {
      registrations: { invites_enabled: false },
    });
    await flushPromises();

    expect(wrapper.find(".invites__form").exists()).toBe(false);
    expect(wrapper.text()).toContain(
      "Invite links are disabled on this server.",
    );
    // Existing invites still list for copy/revoke.
    expect(wrapper.findAll(".invites__row")).toHaveLength(2);
  });

  it("shows the create form to staff even when invites are disabled", async () => {
    useAuthStore().role = "admin";
    const { wrapper } = await mountPage(inviteRows, {
      registrations: { invites_enabled: false },
    });
    await flushPromises();

    expect(wrapper.find(".invites__form").exists()).toBe(true);
  });
});
