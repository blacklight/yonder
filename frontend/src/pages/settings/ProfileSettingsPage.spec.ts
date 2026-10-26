import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import ProfileSettingsPage from "./ProfileSettingsPage.vue";

vi.mock("@/api/accounts", () => ({
  verifyCredentials: vi.fn(),
  updateCredentials: vi.fn(),
}));
import { updateCredentials, verifyCredentials } from "@/api/accounts";

const mockedVerify = vi.mocked(verifyCredentials);
const mockedUpdate = vi.mocked(updateCredentials);

const credentialAccount = {
  id: "1",
  username: "alice",
  acct: "alice",
  display_name: "Alice A.",
  note: "<p>rendered bio</p>",
  avatar: "https://example.com/avatar.png",
  header: "https://example.com/header.png",
  locked: true,
  bot: false,
  discoverable: false,
  hide_collections: true,
  indexable: false,
  fields: [{ name: "Site", value: "<a>rendered</a>" }],
  source: {
    privacy: "public",
    sensitive: false,
    language: "en",
    note: "raw bio",
    fields: [{ name: "Site", value: "https://x.example" }],
  },
};

async function mountOk() {
  mockedVerify.mockResolvedValue(credentialAccount as never);
  const mounted = mountWith(ProfileSettingsPage);
  await flushPromises();
  return mounted;
}

describe("ProfileSettingsPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("prefills the form from the credential account's raw source", async () => {
    const { wrapper } = await mountOk();

    const name = wrapper.get<HTMLInputElement>(".profile-edit__sr-label input");
    expect(name.element.value).toBe("Alice A.");
    // Raw `source.note`, not the rendered HTML in `account.note`.
    expect(
      wrapper.get<HTMLTextAreaElement>(".profile-edit__bio").element.value,
    ).toBe("raw bio");
    const fieldInputs = wrapper.findAll(".profile-edit__field-row input");
    expect(fieldInputs).toHaveLength(2);
    expect((fieldInputs[0].element as HTMLInputElement).value).toBe("Site");
    expect((fieldInputs[1].element as HTMLInputElement).value).toBe(
      "https://x.example",
    );
    const flags = wrapper.findAll(".profile-edit__flag input");
    expect(flags.map((f) => (f.element as HTMLInputElement).checked)).toEqual([
      true, // locked
      false, // bot
      false, // discoverable
      true, // hide_collections
      false, // indexable
    ]);
  });

  it("submits multipart credentials and syncs the auth store", async () => {
    const updated = { ...credentialAccount, display_name: "Alice B." };
    mockedUpdate.mockResolvedValue(updated as never);
    const { wrapper } = await mountOk();

    await wrapper.get(".profile-edit__sr-label input").setValue("Alice B.");
    await wrapper.get("form").trigger("submit");
    await flushPromises();

    const form = mockedUpdate.mock.calls[0][0] as FormData;
    expect(form.get("display_name")).toBe("Alice B.");
    expect(form.get("note")).toBe("raw bio");
    expect(form.get("locked")).toBe("true");
    expect(form.get("bot")).toBe("false");
    expect(form.get("hide_collections")).toBe("true");
    expect(form.get("fields_attributes[0][name]")).toBe("Site");
    expect(form.get("fields_attributes[0][value]")).toBe("https://x.example");
    expect(useAuthStore().account?.display_name).toBe("Alice B.");
    expect(wrapper.find(".profile-edit__saved").exists()).toBe(true);
  });

  it("sends a blank fields_attributes pair to clear all fields", async () => {
    mockedUpdate.mockResolvedValue(credentialAccount as never);
    const { wrapper } = await mountOk();

    await wrapper.get(".profile-edit__field-remove").trigger("click");
    await wrapper.get("form").trigger("submit");
    await flushPromises();

    const form = mockedUpdate.mock.calls[0][0] as FormData;
    expect(form.get("fields_attributes[0][name]")).toBe("");
    expect(form.get("fields_attributes[0][value]")).toBe("");
  });

  it("surfaces API errors on save", async () => {
    mockedUpdate.mockRejectedValue(new Error("nope"));
    const { wrapper } = await mountOk();

    await wrapper.get("form").trigger("submit");
    await flushPromises();

    expect(wrapper.get(".profile-edit__error").text()).toBe("nope");
    expect(wrapper.find(".profile-edit__saved").exists()).toBe(false);
  });

  it("shows an error state when the account cannot be loaded", async () => {
    mockedVerify.mockRejectedValue(new Error("offline"));
    const { wrapper } = mountWith(ProfileSettingsPage);
    await flushPromises();

    expect(wrapper.find("form").exists()).toBe(false);
    expect(wrapper.get(".profile-edit__error").text()).toBe("offline");
  });
});
