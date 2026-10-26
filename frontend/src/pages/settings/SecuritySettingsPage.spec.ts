import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import SecuritySettingsPage from "./SecuritySettingsPage.vue";

vi.mock("@/api/settings", () => ({
  changePassword: vi.fn(),
  deleteAccount: vi.fn(),
  disableTwoFactor: vi.fn(),
  listAuthorizedApps: vi.fn(),
  listSessions: vi.fn(),
  regenerateBackupCodes: vi.fn(),
  revokeAuthorizedApp: vi.fn(),
  revokeSession: vi.fn(),
  totpConfirm: vi.fn(),
  totpDelete: vi.fn(),
  totpSetup: vi.fn(),
  twoFactorStatus: vi.fn(),
  webauthnDelete: vi.fn(),
  webauthnRegister: vi.fn(),
  webauthnRegisterBegin: vi.fn(),
  webauthnRename: vi.fn(),
}));
import {
  changePassword,
  listAuthorizedApps,
  listSessions,
  revokeSession,
  totpConfirm,
  totpDelete,
  totpSetup,
  twoFactorStatus,
} from "@/api/settings";

const mockedChange = vi.mocked(changePassword);
const mockedStatus = vi.mocked(twoFactorStatus);
const mockedSessions = vi.mocked(listSessions);
const mockedApps = vi.mocked(listAuthorizedApps);
const mockedSetup = vi.mocked(totpSetup);
const mockedConfirm = vi.mocked(totpConfirm);
const mockedDelete = vi.mocked(totpDelete);
const mockedRevokeSession = vi.mocked(revokeSession);

const disabledStatus: Record<string, unknown> = {
  enabled: false,
  methods: [],
  totp: { enabled: false, last_verified_at: null },
  webauthn: [],
  backup_codes_remaining: 0,
};

const totpEnabledStatus: Record<string, unknown> = {
  enabled: true,
  methods: ["totp", "backup_code"],
  totp: { enabled: true, last_verified_at: "2026-10-01T10:00:00Z" },
  webauthn: [],
  backup_codes_remaining: 9,
};

const sessionRows = [
  {
    id: "1",
    browser: "Firefox 130",
    ip: "10.0.0.1",
    created_at: "2026-10-01T09:00:00Z",
    last_activity_at: "2026-10-20T09:00:00Z",
    current: true,
  },
  {
    id: "2",
    browser: "Chromium 141",
    ip: "10.0.0.2",
    created_at: "2026-10-02T09:00:00Z",
    last_activity_at: "2026-10-21T09:00:00Z",
    current: false,
  },
];

async function mountPage(status: Record<string, unknown> = disabledStatus) {
  mockedStatus.mockResolvedValue(status as never);
  mockedSessions.mockResolvedValue(sessionRows as never);
  mockedApps.mockResolvedValue([] as never);
  const mounted = mountWith(SecuritySettingsPage);
  await flushPromises();
  return mounted;
}

describe("SecuritySettingsPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useAuthStore().setToken("tok");
  });

  it("changes the password through the new endpoint", async () => {
    mockedChange.mockResolvedValue({ ok: true });
    const { wrapper } = await mountPage();

    const inputs = wrapper.findAll(".security__form input");
    await inputs[0].setValue("old-pass");
    await inputs[1].setValue("new-secret");
    await inputs[2].setValue("new-secret");
    await wrapper.get(".security__form").trigger("submit");
    await flushPromises();

    expect(mockedChange).toHaveBeenCalledWith("old-pass", "new-secret");
    expect(wrapper.find(".security__saved").exists()).toBe(true);
  });

  it("rejects a mismatched confirmation without calling the API", async () => {
    const { wrapper } = await mountPage();
    const inputs = wrapper.findAll(".security__form input");
    await inputs[0].setValue("old-pass");
    await inputs[1].setValue("new-secret");
    await inputs[2].setValue("different");
    await wrapper.get(".security__form").trigger("submit");
    await flushPromises();

    expect(mockedChange).not.toHaveBeenCalled();
    expect(wrapper.find(".security__error").exists()).toBe(true);
  });

  it("walks the TOTP enrollment: setup → code → backup codes", async () => {
    mockedSetup.mockResolvedValue({
      secret: "JBSWY3DPEHPK3PXP",
      otpauth_uri: "otpauth://totp/x",
      qr_data_uri: "data:image/png;base64,AAAA",
    } as never);
    mockedConfirm.mockResolvedValue({
      backup_codes: ["aaaa-bbbb-cccc", "dddd-eeee-ffff"],
    } as never);
    const { wrapper } = await mountPage();

    await wrapper
      .findAll("button")
      .find((b) => b.text().includes("authenticator app"))!
      .trigger("click");
    await flushPromises();

    expect(mockedSetup).toHaveBeenCalled();
    expect(wrapper.find(".security__qr").exists()).toBe(true);
    expect(wrapper.text()).toContain("JBSWY3DPEHPK3PXP");

    await wrapper.get(".security__wizard input").setValue("123456");
    await wrapper.get(".security__wizard form").trigger("submit");
    await flushPromises();

    expect(mockedConfirm).toHaveBeenCalledWith("123456");
    expect(wrapper.text()).toContain("aaaa-bbbb-cccc");
    expect(wrapper.text()).toContain("dddd-eeee-ffff");
  });

  it("gates TOTP removal behind the account password", async () => {
    mockedDelete.mockResolvedValue({} as never);
    const { wrapper } = await mountPage(totpEnabledStatus);

    await wrapper
      .findAll("button")
      .find((b) => b.text().includes("Remove authenticator app"))!
      .trigger("click");
    await flushPromises();
    await wrapper.get(".security__gate input").setValue("hunter2");
    await wrapper.get(".security__gate").trigger("submit");
    await flushPromises();

    expect(mockedDelete).toHaveBeenCalledWith("hunter2");
  });

  it("lists sessions and revokes a non-current one", async () => {
    mockedRevokeSession.mockResolvedValue({} as never);
    const { wrapper } = await mountPage();

    const rows = wrapper.findAll(".security__list-row");
    expect(rows.length).toBe(2);
    expect(rows[0].text()).toContain("Firefox 130");
    expect(rows[0].text()).toContain("Current");

    const revokeButton = rows[1].find("button");
    expect(revokeButton.exists()).toBe(true);
    await revokeButton.trigger("click");
    await flushPromises();

    expect(mockedRevokeSession).toHaveBeenCalledWith("2");
    expect(wrapper.findAll(".security__list-row").length).toBe(1);
  });
});
