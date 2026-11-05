import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import type { Account } from "@/api/entities";
import ModerationSettingsPage from "./ModerationSettingsPage.vue";

vi.mock("@/api/accounts", () => ({
  mutedAccounts: vi.fn(),
  blockedAccounts: vi.fn(),
  unmuteAccount: vi.fn(),
  unblockAccount: vi.fn(),
}));
vi.mock("@/api/domains", () => ({
  domainBlocksPage: vi.fn(),
  blockDomain: vi.fn(),
  unblockDomain: vi.fn(),
}));
import {
  blockedAccounts,
  mutedAccounts,
  unblockAccount,
  unmuteAccount,
} from "@/api/accounts";
import { blockDomain, domainBlocksPage, unblockDomain } from "@/api/domains";

const mockedMuted = vi.mocked(mutedAccounts);
const mockedBlocked = vi.mocked(blockedAccounts);
const mockedUnmute = vi.mocked(unmuteAccount);
const mockedUnblock = vi.mocked(unblockAccount);
const mockedDomains = vi.mocked(domainBlocksPage);
const mockedBlockDomain = vi.mocked(blockDomain);
const mockedUnblockDomain = vi.mocked(unblockDomain);

function account(id: string, acct: string, displayName = ""): Account {
  return {
    id,
    username: acct.split("@")[0],
    acct,
    display_name: displayName,
    note: "",
    url: `https://example.org/@${acct}`,
    avatar: "",
    header: "",
    locked: false,
    bot: false,
    created_at: "2026-01-01T00:00:00Z",
    followers_count: 0,
    following_count: 0,
    statuses_count: 0,
    fields: [],
    emojis: [],
    moved: null,
  };
}

const mutedRows = [
  account("1", "noisy@example.social", "Noisy"),
  account("2", "spammy", "Spammy"),
];
const blockedRows = [account("3", "troll@bad.example", "Troll")];
const domainRows = ["spam.example", "ads.example"];

const page = <T>(items: T[], next?: string) => ({ items, next });

async function mountPage(
  opts: {
    muted?: Account[];
    blocked?: Account[];
    domains?: string[];
  } = {},
) {
  mockedMuted.mockResolvedValue(
    page(opts.muted === undefined ? mutedRows : opts.muted),
  );
  mockedBlocked.mockResolvedValue(
    page(opts.blocked === undefined ? blockedRows : opts.blocked),
  );
  mockedDomains.mockResolvedValue(
    page(opts.domains === undefined ? domainRows : opts.domains),
  );
  const mounted = mountWith(ModerationSettingsPage);
  await flushPromises();
  return mounted;
}

describe("ModerationSettingsPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useAuthStore().setToken("tok");
  });

  it("lists muted and blocked users with their actions", async () => {
    const { wrapper } = await mountPage();
    const groups = wrapper.findAll(".mod__group");
    expect(groups).toHaveLength(3);

    const [mutedGroup, blockedGroup] = groups;
    expect(mutedGroup.findAll(".mod__row")).toHaveLength(2);
    expect(mutedGroup.text()).toContain("@noisy@example.social");
    expect(mutedGroup.text()).toContain("Unmute");

    expect(blockedGroup.findAll(".mod__row")).toHaveLength(1);
    expect(blockedGroup.text()).toContain("@troll@bad.example");
    expect(blockedGroup.text()).toContain("Unblock");
  });

  it("unmutes an account and drops it from the list", async () => {
    mockedUnmute.mockResolvedValue({} as never);
    const { wrapper } = await mountPage();
    const group = wrapper.findAll(".mod__group")[0];

    await group
      .findAll("button")
      .find((b) => b.text() === "Unmute")!
      .trigger("click");
    await flushPromises();

    expect(mockedUnmute).toHaveBeenCalledWith("1");
    expect(group.findAll(".mod__row")).toHaveLength(1);
    expect(group.text()).not.toContain("@noisy@example.social");
  });

  it("unblocks an account", async () => {
    mockedUnblock.mockResolvedValue({} as never);
    const { wrapper } = await mountPage();
    const group = wrapper.findAll(".mod__group")[1];

    await group
      .findAll("button")
      .find((b) => b.text() === "Unblock")!
      .trigger("click");
    await flushPromises();

    expect(mockedUnblock).toHaveBeenCalledWith("3");
    expect(group.text()).toContain("You haven't blocked anyone.");
  });

  it("lists blocked domains and unblocks one", async () => {
    mockedUnblockDomain.mockResolvedValue(undefined as never);
    const { wrapper } = await mountPage();
    const group = wrapper.findAll(".mod__group")[2];

    expect(group.findAll(".mod__domain")).toHaveLength(2);
    expect(group.text()).toContain("spam.example");

    await group
      .findAll("button")
      .find((b) => b.text() === "Unblock")!
      .trigger("click");
    await flushPromises();

    expect(mockedUnblockDomain).toHaveBeenCalledWith("spam.example");
    expect(group.findAll(".mod__domain")).toHaveLength(1);
  });

  it("blocks a domain, tolerating a pasted URL", async () => {
    mockedBlockDomain.mockResolvedValue(undefined as never);
    const { wrapper } = await mountPage();
    const group = wrapper.findAll(".mod__group")[2];

    await group.find("input").setValue("https://New.Spam.example/path?q=1");
    await group.find(".mod__form").trigger("submit");
    await flushPromises();

    expect(mockedBlockDomain).toHaveBeenCalledWith("new.spam.example");
    expect(group.findAll(".mod__domain")).toHaveLength(3);
    expect(group.findAll(".mod__domain")[0].text()).toBe("new.spam.example");
  });

  it("rejects a malformed domain without calling the API", async () => {
    const { wrapper } = await mountPage();
    const group = wrapper.findAll(".mod__group")[2];

    await group.find("input").setValue("not a domain");
    await group.find(".mod__form").trigger("submit");
    await flushPromises();

    expect(mockedBlockDomain).not.toHaveBeenCalled();
    expect(group.text()).toContain("Enter a domain like example.social.");
  });

  it("shows per-section empty states", async () => {
    const { wrapper } = await mountPage({
      muted: [],
      blocked: [],
      domains: [],
    });
    expect(wrapper.text()).toContain("You aren't muting anyone.");
    expect(wrapper.text()).toContain("You haven't blocked anyone.");
    expect(wrapper.text()).toContain("You haven't blocked any instances.");
  });

  it("pages through with the Link-header cursor", async () => {
    mockedMuted.mockResolvedValueOnce(
      page(mutedRows, "/api/v1/mutes?max_id=42"),
    );
    mockedMuted.mockResolvedValueOnce(page([account("9", "older")]));
    mockedBlocked.mockResolvedValue(page([]));
    mockedDomains.mockResolvedValue(page([]));

    const { wrapper } = await mountPage();
    const group = wrapper.findAll(".mod__group")[0];
    await group
      .findAll("button")
      .find((b) => b.text() === "Load more")!
      .trigger("click");
    await flushPromises();

    expect(mockedMuted).toHaveBeenLastCalledWith({ max_id: "42" });
    expect(group.findAll(".mod__row")).toHaveLength(3);
    expect(group.text()).toContain("@older");
  });
});
