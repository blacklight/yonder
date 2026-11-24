import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import { createMemoryHistory, createRouter } from "vue-router";
import AccountChip from "./AccountChip.vue";
import type { Account } from "@/api/entities";

function account(overrides: Partial<Account> = {}): Account {
  return {
    id: "1",
    username: "alice",
    acct: "alice",
    display_name: "Alice A.",
    avatar: "https://img.test/alice.png",
    emojis: [],
    ...overrides,
  } as Account;
}

async function mountChip(props: Record<string, unknown>) {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [{ path: "/", component: { template: "<div/>" } }],
  });
  return mount(AccountChip, {
    props,
    global: { plugins: [router] },
  });
}

describe("AccountChip", () => {
  it("renders avatar, display name and acct linked to the profile", async () => {
    const w = await mountChip({ account: account() });
    expect(w.find('img[src="https://img.test/alice.png"]').exists()).toBe(true);
    expect(w.text()).toContain("Alice A.");
    expect(w.text()).toContain("@alice");
    const link = w.find("a");
    expect(link.attributes("href")).toBe("/@alice");
  });

  it("shows the remote domain in acct", async () => {
    const w = await mountChip({
      account: account({ acct: "bob@remote.test", username: "bob" }),
    });
    expect(w.text()).toContain("@bob@remote.test");
  });

  it("falls back to plain text when the account is missing", async () => {
    const w = await mountChip({ account: null, fallback: "@42" });
    expect(w.find("a").exists()).toBe(false);
    expect(w.text()).toContain("@42");
  });
});
