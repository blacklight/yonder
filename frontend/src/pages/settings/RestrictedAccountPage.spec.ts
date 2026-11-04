import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import RestrictedAccountPage from "./RestrictedAccountPage.vue";

vi.mock("@/api/settings", () => ({
  restrictedSession: vi.fn(),
  restrictedLogout: vi.fn(),
  listExports: vi.fn(),
  requestArchiveExport: vi.fn(),
  downloadExport: vi.fn(),
  downloadInlineExport: vi.fn(),
}));
import {
  restrictedSession,
  restrictedLogout,
  listExports,
  requestArchiveExport,
  downloadExport,
  downloadInlineExport,
} from "@/api/settings";

const session = {
  restricted: true,
  account: { username: "banned", acct: "banned" },
  email: "banned@example.com",
  suspended_at: "2026-01-01T00:00:00Z",
  contact_email: "staff@example.com",
  contact_account: null,
};

const archive = {
  id: "7",
  kind: "archive",
  state: "done",
  processed: 1,
  total: 1,
  failed: 0,
  file_size: 100,
  error: null,
  created_at: "2026-01-02T00:00:00Z",
  completed_at: "2026-01-02T00:01:00Z",
  expires_at: "2026-01-09T00:00:00Z",
};

describe("RestrictedAccountPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useAuthStore().setToken("restricted-tok");
    vi.mocked(restrictedSession).mockResolvedValue(session as never);
    vi.mocked(listExports).mockResolvedValue([archive] as never);
  });

  it("shows the suspension notice and contact", async () => {
    const { wrapper } = mountWith(RestrictedAccountPage);
    await flushPromises();
    expect(wrapper.text()).toContain("suspended");
    expect(wrapper.text()).toContain("@banned");
    expect(wrapper.text()).toContain("staff@example.com");
  });

  it("requests and downloads an archive export", async () => {
    vi.mocked(requestArchiveExport).mockResolvedValue(archive as never);
    vi.mocked(downloadExport).mockResolvedValue({
      blob: new Blob(["zip"]),
      filename: "archive.zip",
    } as never);
    const { wrapper } = mountWith(RestrictedAccountPage);
    await flushPromises();
    const buttons = wrapper.findAll("button");
    const request = buttons.find((b) => b.text().includes("Request archive"));
    expect(request).toBeTruthy();
    await request!.trigger("click");
    await flushPromises();
    expect(requestArchiveExport).toHaveBeenCalled();
    const download = wrapper
      .findAll("button")
      .find((b) => b.text() === "Download");
    expect(download).toBeTruthy();
    const click = vi
      .spyOn(HTMLAnchorElement.prototype, "click")
      .mockImplementation(() => {});
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:x");
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    await download!.trigger("click");
    await flushPromises();
    expect(downloadExport).toHaveBeenCalledWith("7");
    click.mockRestore();
  });

  it("logout revokes the restricted token and signs out", async () => {
    vi.mocked(restrictedLogout).mockResolvedValue({ ok: true } as never);
    const { wrapper, router } = mountWith(RestrictedAccountPage);
    await flushPromises();
    const logout = wrapper
      .findAll("button")
      .find((b) => b.text().includes("Sign out"));
    expect(logout).toBeTruthy();
    await logout!.trigger("click");
    await flushPromises();
    expect(restrictedLogout).toHaveBeenCalled();
    expect(useAuthStore().token).toBeNull();
    expect(router.currentRoute.value.name).toBe("sign-in");
  });

  it("a download failure surfaces the error instead of a loop", async () => {
    const { ApiError } = await import("@/api/errors");
    vi.mocked(downloadInlineExport).mockRejectedValue(
      new ApiError("nope", 403) as never,
    );
    const { wrapper } = mountWith(RestrictedAccountPage);
    await flushPromises();
    const kind = wrapper
      .findAll("button")
      .find((b) => b.text().includes("Following"));
    expect(kind).toBeTruthy();
    await kind!.trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("nope");
    expect(useAuthStore().token).toBe("restricted-tok");
  });
});
