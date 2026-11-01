import { describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { useAuthStore } from "@/stores/auth";
import { mountWith } from "@/test/render";
import { ApiError } from "@/api/errors";
import AboutPage from "./AboutPage.vue";

const api = vi.hoisted(() => ({
  getInstance: vi.fn(),
  getInstanceV1: vi.fn(),
  getExtendedDescription: vi.fn(),
  getInstanceDomainBlocks: vi.fn(),
}));
vi.mock("@/api/instance", async (importOriginal) => ({
  ...(await importOriginal<object>()),
  getInstance: api.getInstance,
  getInstanceV1: api.getInstanceV1,
  getExtendedDescription: api.getExtendedDescription,
  getInstanceDomainBlocks: api.getInstanceDomainBlocks,
}));

function entity(overrides: Record<string, unknown> = {}) {
  return {
    domain: "test.example",
    title: "Test Server",
    version: "Yonder/0.1.0",
    source_url: "https://example.com/source",
    short_description: "A nice place",
    languages: ["en", "de"],
    thumbnail: { url: "https://test.example/banner.png" },
    icon: [{ src: "https://test.example/logo.png", size: "1x" }],
    usage: { users: { active_month: 3 } },
    registrations: { enabled: true, approval_required: false },
    rules: [
      { id: "1", text: "Be kind" },
      { id: "2", text: "No spam" },
    ],
    contact: {
      email: "admin@test.example",
      name: "The Admin",
      url: "https://test.example/@admin",
      account: null,
    },
    ...overrides,
  };
}

async function mount() {
  const { wrapper, router } = mountWith(AboutPage);
  await router.push("/about");
  await flushPromises();
  return wrapper;
}

describe("AboutPage", () => {
  it("renders hero, stats, description, rules, contact and footer", async () => {
    api.getInstance.mockResolvedValue(entity());
    api.getInstanceV1.mockResolvedValue({
      stats: { user_count: 10, status_count: 200, domain_count: 5 },
      email: "admin@test.example",
    });
    api.getExtendedDescription.mockResolvedValue(null);
    api.getInstanceDomainBlocks.mockResolvedValue(null);
    const wrapper = await mount();
    expect(wrapper.get(".about__title").text()).toBe("Test Server");
    expect(wrapper.get(".about__domain").text()).toBe("test.example");
    expect(wrapper.get<HTMLImageElement>(".about__banner").element.src).toBe(
      "https://test.example/banner.png",
    );
    expect(wrapper.get<HTMLImageElement>(".about__logo").element.src).toBe(
      "https://test.example/logo.png",
    );
    const statValues = wrapper.findAll(".about__stat dd").map((d) => d.text());
    expect(statValues).toEqual(["3", "10", "200", "5"]);
    // No extended description → the plain description fills the card.
    expect(wrapper.get(".about__description").text()).toContain("A nice place");
    const rules = wrapper.findAll(".about__rules li").map((li) => li.text());
    expect(rules).toEqual(["Be kind", "No spam"]);
    expect(wrapper.text()).toContain("Administered by");
    expect(wrapper.get(".about__contact a").text()).toBe("The Admin");
    expect(wrapper.get(".about__contact-email").attributes("href")).toBe(
      "mailto:admin@test.example",
    );
    expect(wrapper.text()).toContain("Yonder/0.1.0");
    expect(wrapper.text()).toContain("en, de");
    expect(wrapper.get(".about__footer a").attributes("href")).toBe(
      "https://example.com/source",
    );
  });

  it("prefers the extended description over the short one", async () => {
    api.getInstance.mockResolvedValue(entity());
    api.getInstanceV1.mockResolvedValue({});
    api.getExtendedDescription.mockResolvedValue({
      content: "<p>Long form</p>",
    });
    api.getInstanceDomainBlocks.mockResolvedValue(null);
    const wrapper = await mount();
    expect(wrapper.get(".about__description").text()).toBe("Long form");
  });

  it("renders the moderated-servers section when exposed", async () => {
    api.getInstance.mockResolvedValue(entity({ rules: [] }));
    api.getInstanceV1.mockResolvedValue({});
    api.getExtendedDescription.mockResolvedValue(null);
    api.getInstanceDomainBlocks.mockResolvedValue([
      { domain: "bad.example", severity: "suspend", comment: "spam farm" },
    ]);
    const wrapper = await mount();
    expect(wrapper.text()).toContain("Moderated servers");
    expect(wrapper.text()).toContain("bad.example");
    expect(wrapper.text()).toContain("spam farm");
    expect(wrapper.findAll(".about__rules li")).toHaveLength(0);
  });

  it("offers sign-up/sign-in to anonymous visitors only", async () => {
    api.getInstance.mockResolvedValue(entity());
    api.getInstanceV1.mockResolvedValue({});
    api.getExtendedDescription.mockResolvedValue(null);
    api.getInstanceDomainBlocks.mockResolvedValue(null);
    let wrapper = await mount();
    const links = wrapper.findAll(".about__cta a").map((a) => a.text());
    expect(links).toEqual(["Sign up", "Sign in"]);
    wrapper.unmount();

    useAuthStore().setToken("tok");
    api.getInstance.mockResolvedValue(entity());
    wrapper = await mount();
    expect(wrapper.find(".about__cta").exists()).toBe(false);
  });

  it("links the resolved contact account to its profile", async () => {
    api.getInstance.mockResolvedValue(
      entity({
        contact: {
          email: "admin@test.example",
          account: {
            id: "1",
            acct: "admin",
            display_name: "Root",
            url: "https://test.example/@admin",
            avatar: "",
          },
        },
      }),
    );
    api.getInstanceV1.mockResolvedValue({});
    api.getExtendedDescription.mockResolvedValue(null);
    api.getInstanceDomainBlocks.mockResolvedValue(null);
    const wrapper = await mount();
    const link = wrapper.get(".about__contact-account");
    expect(link.attributes("href")).toBe("/@admin");
    expect(link.text()).toContain("Root");
  });

  it("shows a load error when the instance entity fails", async () => {
    api.getInstance.mockRejectedValue(new ApiError("boom", 500));
    api.getInstanceV1.mockResolvedValue({});
    api.getExtendedDescription.mockResolvedValue(null);
    api.getInstanceDomainBlocks.mockResolvedValue(null);
    const wrapper = await mount();
    expect(wrapper.get('[role="alert"]').text()).toContain("Could not load");
  });
});
