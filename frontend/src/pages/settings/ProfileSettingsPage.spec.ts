import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import type { DOMWrapper, VueWrapper } from "@vue/test-utils";
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

// jsdom doesn't implement object URLs — stub them so preview picks are
// observable and revocation can be asserted.
const objectUrls = vi.hoisted(() => ({ n: 0 }));
URL.createObjectURL = vi.fn(() => `blob:preview-${++objectUrls.n}`);
URL.revokeObjectURL = vi.fn();

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

function fileInputs(wrapper: VueWrapper) {
  return wrapper.findAll<HTMLInputElement>(".profile-edit__file");
}

async function pickFile(input: DOMWrapper<HTMLInputElement>) {
  const file = new File(["img"], "pic.png", { type: "image/png" });
  Object.defineProperty(input.element, "files", {
    value: [file],
    configurable: true,
  });
  await input.trigger("change");
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

  it("previews picked avatar and header files before saving", async () => {
    const { wrapper } = await mountOk();

    // Stored URLs render until a file is picked.
    expect(wrapper.get(".avatar__img").attributes("src")).toBe(
      "https://example.com/avatar.png",
    );
    expect(wrapper.get(".profile-edit__header").attributes("src")).toBe(
      "https://example.com/header.png",
    );

    const [avatarIn, headerIn] = fileInputs(wrapper);
    await pickFile(avatarIn);
    expect(wrapper.get(".avatar__img").attributes("src")).toMatch(
      /^blob:preview-/,
    );

    await pickFile(headerIn);
    expect(wrapper.get(".profile-edit__header").attributes("src")).toMatch(
      /^blob:preview-/,
    );
  });

  it("removes the preview and revokes the object URL on remove", async () => {
    const { wrapper } = await mountOk();
    const [avatarIn] = fileInputs(wrapper);
    await pickFile(avatarIn);
    const blob = wrapper.get(".avatar__img").attributes("src");

    // [Change, Remove] inside the avatar media block.
    const remove = wrapper
      .findAll(".profile-edit__media")[0]
      .findAll("button")[1];
    await remove.trigger("click");

    expect(URL.revokeObjectURL).toHaveBeenCalledWith(blob);
    expect(wrapper.find(".avatar__img").exists()).toBe(false);
  });

  it("swaps the preview to the saved URL after upload", async () => {
    const updated = {
      ...credentialAccount,
      avatar: "https://example.com/avatar-new.png",
    };
    mockedUpdate.mockResolvedValue(updated as never);
    const { wrapper } = await mountOk();

    await pickFile(fileInputs(wrapper)[0]);
    const blob = wrapper.get(".avatar__img").attributes("src");

    await wrapper.get("form").trigger("submit");
    await flushPromises();

    const form = mockedUpdate.mock.calls[0][0] as FormData;
    expect(form.get("avatar")).toBeInstanceOf(File);
    expect(URL.revokeObjectURL).toHaveBeenCalledWith(blob);
    expect(wrapper.get(".avatar__img").attributes("src")).toBe(
      "https://example.com/avatar-new.png",
    );
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
