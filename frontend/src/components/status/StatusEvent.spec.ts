import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createMemoryHistory, createRouter } from "vue-router";
import { i18n } from "@/i18n";
import { createPinia, setActivePinia } from "pinia";
import StatusEvent from "./StatusEvent.vue";
import { useAuthStore } from "@/stores/auth";
import type { Status } from "@/api/entities";

const rsvpMock = vi.fn();
const participationsMock = vi.fn();
const decideMock = vi.fn();
const inviteesMock = vi.fn();
const removeMock = vi.fn();
vi.mock("@/api/events", () => ({
  eventRsvp: (...a: unknown[]) => rsvpMock(...a),
  eventParticipations: (...a: unknown[]) => participationsMock(...a),
  eventDecideRequest: (...a: unknown[]) => decideMock(...a),
  eventInvitees: (...a: unknown[]) => inviteesMock(...a),
  eventRemoveParticipant: (...a: unknown[]) => removeMock(...a),
}));
const searchMock = vi.fn();
vi.mock("@/api/search", () => ({
  search: (...a: unknown[]) => searchMock(...a),
}));
vi.mock("@/api/client", () => ({
  requestBlob: vi.fn().mockResolvedValue({
    blob: new Blob(["BEGIN:VCALENDAR"]),
    filename: "event.ics",
  }),
}));
// `instance.ensure()` runs on mount — no instance entity means no
// map provider override (osm default) and geocoding stays off.
vi.mock("@/api/instance", () => ({
  getInstance: vi.fn().mockRejectedValue(new Error("no instance")),
}));
vi.mock("@/composables/useExports", () => ({ saveBlob: vi.fn() }));

function statusWith(
  event: Record<string, unknown>,
  extra: Record<string, unknown> = {},
): Status {
  return {
    id: "7",
    url: "https://yonder.test/@alice/7",
    visibility: "public",
    account: { acct: "alice", id: "1" },
    yonder: { event },
    ...extra,
  } as unknown as Status;
}

const BASE_EVENT = {
  name: "Gig night",
  status: "scheduled",
  start_time: "2030-06-01T18:00:00+00:00",
  end_time: "2030-06-01T22:00:00+00:00",
  timezone: "UTC",
  join_mode: "free",
  rsvp_protocol: "join",
  ics_url: "https://yonder.test/api/v1/yonder/events/7/ics",
  participation: { state: null, can_join: true, reason: null },
};

async function mountCard(
  event: Record<string, unknown> = {},
  extra: Record<string, unknown> = {},
) {
  const pinia = createPinia();
  setActivePinia(pinia);
  useAuthStore().token = "test-token";
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [{ path: "/", component: { template: "<div/>" } }],
  });
  const wrapper = mount(StatusEvent, {
    props: { status: statusWith({ ...BASE_EVENT, ...event }, extra) },
    global: { plugins: [i18n, router, pinia] },
  });
  await flushPromises();
  return wrapper;
}

function rsvpButtons(wrapper: ReturnType<typeof mount>) {
  return wrapper
    .findAll("[data-testid^='rsvp-']")
    .map((b) => b.attributes("data-testid")!);
}

describe("StatusEvent", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    setActivePinia(createPinia());
    useAuthStore().token = "test-token";
    searchMock.mockResolvedValue({ accounts: [], statuses: [], hashtags: [] });
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText: vi.fn().mockResolvedValue(undefined) },
      configurable: true,
    });
  });

  it("renders name, date and locations", async () => {
    const w = await mountCard({
      locations: [
        {
          type: "place",
          name: "Club",
          latitude: 41.9,
          longitude: 12.5,
          address: {
            street: "Via Appia 1",
            locality: "Rome",
            country: "Italy",
          },
        },
      ],
    });
    expect(w.text()).toContain("Gig night");
    expect(w.text()).toContain("Club");
    expect(w.text()).toContain("Via Appia 1, Rome, Italy");
    expect(w.find("a[href*='openstreetmap.org']").exists()).toBe(true);
    // Start/End on separate labelled lines with the IANA zone name.
    expect(w.text()).toContain("Start");
    expect(w.text()).toContain("End");
    expect(w.text()).toContain("(UTC)");
  });

  it("opens the embedded OSM map modal from the map button", async () => {
    const w = await mountCard({
      locations: [
        {
          type: "place",
          name: "Club",
          latitude: 41.9,
          longitude: 12.5,
        },
      ],
    });
    await w.get("[data-testid='event-show-map']").trigger("click");
    await flushPromises();
    const iframe =
      document.querySelector<HTMLIFrameElement>(".status-event__map");
    expect(iframe?.src).toContain("openstreetmap.org/export/embed.html");
    expect(iframe?.src).toContain("marker=41.9,12.5");
  });

  it("shows Join for a fresh viewer on a join-protocol event", async () => {
    const w = await mountCard();
    expect(rsvpButtons(w)).toEqual(["rsvp-join"]);
    await w.get("[data-testid='rsvp-join']").trigger("click");
    await flushPromises();
    expect(rsvpMock).toHaveBeenCalledWith("7", "accepted");
  });

  it("shows Request to join for invite_response protocol", async () => {
    const w = await mountCard({ rsvp_protocol: "invite_response" });
    expect(rsvpButtons(w)).toEqual(["rsvp-request"]);
  });

  it("shows Going + Decline for an accepted participant", async () => {
    const w = await mountCard({
      participation: { state: "accepted", can_join: false, reason: null },
    });
    expect(rsvpButtons(w)).toEqual(["rsvp-accept_done", "rsvp-decline"]);
  });

  it("shows Maybe + Decline for an invited viewer (invite_response)", async () => {
    const w = await mountCard({
      rsvp_protocol: "invite_response",
      participation: { state: "invited", can_join: false, reason: null },
    });
    expect(rsvpButtons(w)).toEqual([
      "rsvp-accept",
      "rsvp-tentative",
      "rsvp-decline",
    ]);
  });

  it("shows Pending + Decline for a pending join", async () => {
    const w = await mountCard({
      rsvp_protocol: "invite_response",
      participation: { state: "pending", can_join: false, reason: null },
    });
    expect(rsvpButtons(w)).toEqual(["rsvp-pending_done", "rsvp-decline"]);
  });

  it("hides RSVP for external protocol and shows Register", async () => {
    const w = await mountCard({
      rsvp_protocol: "external",
      external_participation_url: "https://tickets.example.com/x",
    });
    expect(rsvpButtons(w)).toEqual([]);
    expect(w.find("a[href='https://tickets.example.com/x']").exists()).toBe(
      true,
    );
  });

  it("shows the disabled reason for a full event", async () => {
    const w = await mountCard({
      participation: { state: null, can_join: false, reason: "full" },
    });
    expect(rsvpButtons(w)).toEqual([]);
    expect(w.text()).toContain("Event is full");
  });

  it("hides RSVP entirely for the organizer and shows the panel", async () => {
    const w = await mountCard({
      participation: {
        state: "accepted",
        can_join: false,
        reason: "own_event",
      },
    });
    expect(rsvpButtons(w)).toEqual([]);
    expect(w.find("[data-testid='organizer-panel-toggle']").exists()).toBe(
      true,
    );
  });

  it("organizer panel lists pending requests with approve/reject", async () => {
    participationsMock.mockResolvedValue({
      participations: [
        {
          account_id: "9",
          account: {
            id: "9",
            username: "bob",
            acct: "bob",
            display_name: "Bobby",
            avatar: "https://img.test/bob.png",
            emojis: [],
          },
          state: "pending",
          participation_message: "hi",
        },
        {
          account_id: "10",
          account: {
            id: "10",
            username: "cat",
            acct: "cat@remote.test",
            display_name: "Cat",
            avatar: "",
            emojis: [],
          },
          state: "accepted",
        },
      ],
    });
    const w = await mountCard({
      participation: {
        state: "accepted",
        can_join: false,
        reason: "own_event",
      },
    });
    await w.get("[data-testid='organizer-panel-toggle']").trigger("click");
    await flushPromises();
    expect(w.text()).toContain("Bobby");
    expect(w.text()).toContain("@bob");
    expect(w.text()).toContain("Cat");
    expect(w.text()).toContain("@cat@remote.test");
    expect(w.find('img[src="https://img.test/bob.png"]').exists()).toBe(true);
    const approve = w.findAll("button").find((b) => b.text() === "Approve")!;
    await approve.trigger("click");
    await flushPromises();
    expect(decideMock).toHaveBeenCalledWith("7", "9", "authorize");
  });

  it("hides the invite box unless the event is direct", async () => {
    participationsMock.mockResolvedValue({ participations: [] });
    const own = {
      participation: {
        state: "accepted",
        can_join: false,
        reason: "own_event",
      },
    };
    const pub = await mountCard(own, { visibility: "public" });
    await pub.get("[data-testid='organizer-panel-toggle']").trigger("click");
    await flushPromises();
    expect(pub.find("[data-testid='invite-search']").exists()).toBe(false);
    const dir = await mountCard(own, { visibility: "direct" });
    await dir.get("[data-testid='organizer-panel-toggle']").trigger("click");
    await flushPromises();
    expect(dir.find("[data-testid='invite-search']").exists()).toBe(true);
  });

  it("invite picker searches accounts and invites the picked id as a string", async () => {
    searchMock.mockResolvedValue({
      accounts: [
        {
          id: "117518582417719632",
          username: "testuser1",
          acct: "testuser1",
          display_name: "Test User 1",
        },
      ],
      statuses: [],
      hashtags: [],
    });
    participationsMock.mockResolvedValue({ participations: [] });
    inviteesMock.mockResolvedValue({ ok: true });
    const w = await mountCard(
      {
        participation: {
          state: "accepted",
          can_join: false,
          reason: "own_event",
        },
      },
      { visibility: "direct" },
    );
    await w.get("[data-testid='organizer-panel-toggle']").trigger("click");
    await flushPromises();
    const input = w.get("[data-testid='invite-search']");
    await input.setValue("test");
    // Enter with no results yet runs the search immediately.
    await input.trigger("keydown.enter");
    await flushPromises();
    expect(searchMock).toHaveBeenCalledWith({
      q: "test",
      type: "accounts",
      limit: 5,
      resolve: true,
    });
    // Enter again picks the top suggestion — the snowflake id must
    // reach the API as a string, never a JS number.
    await input.trigger("keydown.enter");
    await flushPromises();
    expect(inviteesMock).toHaveBeenCalledWith("7", ["117518582417719632"]);
  });

  it("invite picker result click invites that account", async () => {
    searchMock.mockResolvedValue({
      accounts: [
        {
          id: "42",
          username: "bob",
          acct: "bob",
          display_name: "Bob",
        },
      ],
      statuses: [],
      hashtags: [],
    });
    participationsMock.mockResolvedValue({ participations: [] });
    inviteesMock.mockResolvedValue({ ok: true });
    const w = await mountCard(
      {
        participation: {
          state: "accepted",
          can_join: false,
          reason: "own_event",
        },
      },
      { visibility: "direct" },
    );
    await w.get("[data-testid='organizer-panel-toggle']").trigger("click");
    await flushPromises();
    const input = w.get("[data-testid='invite-search']");
    await input.setValue("bob");
    // Enter runs the search immediately (no debounce wait).
    await input.trigger("keydown.enter");
    await flushPromises();
    const btn = w
      .findAll(".status-event__invite-results button")
      .find((b) => b.text().includes("bob"))!;
    await btn.trigger("click");
    await flushPromises();
    expect(inviteesMock).toHaveBeenCalledWith("7", ["42"]);
  });

  it("copy link writes the permalink; public events get a direct ICS link", async () => {
    const w = await mountCard();
    await w
      .findAll("button")
      .find((b) => b.text().includes("Copy link"))!
      .trigger("click");
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith(
      "https://yonder.test/@alice/7",
    );
    const link = w.get("[data-testid='download-calendar-link']");
    expect(link.attributes("href")).toBe(
      "https://yonder.test/api/v1/yonder/events/7/ics",
    );
  });

  it("non-public events keep the authenticated ICS blob download", async () => {
    const { saveBlob } = await import("@/composables/useExports");
    const { requestBlob } = await import("@/api/client");
    const w = await mountCard({}, { visibility: "private" });
    expect(w.find("[data-testid='download-calendar-link']").exists()).toBe(
      false,
    );
    await w
      .findAll("button")
      .find((b) => b.text().includes("Download calendar"))!
      .trigger("click");
    await flushPromises();
    expect(requestBlob).toHaveBeenCalled();
    expect(saveBlob).toHaveBeenCalledWith(expect.any(Blob), "event.ics");
  });

  it("cancelled events render the badge and strikethrough name", async () => {
    const w = await mountCard({ status: "cancelled" });
    expect(w.find(".status-event__badge--cancelled").exists()).toBe(true);
    expect(w.find(".status-event__name--cancelled").exists()).toBe(true);
  });
});
