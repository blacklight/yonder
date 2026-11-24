import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { defineComponent, h, ref } from "vue";
import { mountWith } from "@/test/render";
import { emptyEventDraft, type EventDraft } from "@/stores/compose";
import { geocodeSearch } from "@/api/geocoding";
import EventBuilder from "./EventBuilder.vue";

vi.mock("@/api/search", () => ({
  search: vi.fn().mockResolvedValue({
    accounts: [
      {
        id: "42",
        acct: "bob@remote.test",
        username: "bob",
        display_name: "Bob",
      },
    ],
    statuses: [],
    hashtags: [],
  }),
}));

const getInstanceMock = vi.fn();
vi.mock("@/api/instance", () => ({
  getInstance: (...a: unknown[]) => getInstanceMock(...a),
}));

vi.mock("@/api/geocoding", () => ({
  geocodeSearch: vi.fn(),
}));

function draft(over: Partial<EventDraft> = {}): EventDraft {
  return { ...emptyEventDraft(), timezone: "Europe/Rome", ...over };
}

function placeLocation() {
  return {
    kind: "place" as const,
    name: "",
    url: "",
    streetAddress: "",
    locality: "",
    region: "",
    postalCode: "",
    country: "",
    addressText: "",
    latitude: "",
    longitude: "",
  };
}

/** Stateful harness — applies `update` emissions back into the prop,
 * mirroring how ComposeBox feeds the draft store. */
function Harness(initial: EventDraft, visibility = "public") {
  return defineComponent({
    setup() {
      const event = ref<EventDraft>(initial);
      const title = ref("");
      return { event, title };
    },
    render() {
      return h(EventBuilder, {
        event: this.event,
        title: this.title,
        visibility,
        media: [],
        onUpdate: (e: EventDraft) => {
          this.event = e;
        },
        onUpdateTitle: (t: string) => {
          this.title = t;
        },
      });
    },
  });
}

function mountBuilder(event = draft(), visibility = "public") {
  return mountWith(Harness(event, visibility));
}

describe("EventBuilder", () => {
  beforeEach(() => {
    getInstanceMock.mockReset().mockResolvedValue({ yonder: {} });
    vi.mocked(geocodeSearch).mockReset();
  });

  it("renders the draft's timezone and emits wall-time patches", async () => {
    const { wrapper } = mountBuilder(draft({ startTime: "2030-06-01T18:00" }));
    const tz = wrapper.get("[data-testid='event-timezone']");
    expect((tz.element as HTMLSelectElement).value).toBe("Europe/Rome");
    await wrapper
      .get("[data-testid='event-start']")
      .setValue("2030-06-02T19:30");
    const builder = wrapper.findComponent(EventBuilder);
    const ev = builder.props("event") as EventDraft;
    expect(ev.startTime).toBe("2030-06-02T19:30");
    expect(ev.timezone).toBe("Europe/Rome");
  });

  it("toggles end time on and off", async () => {
    const { wrapper } = mountBuilder();
    const hasEnd = wrapper.get("[data-testid='event-has-end']");
    await hasEnd.setValue(true);
    let ev = wrapper.findComponent(EventBuilder).props("event") as EventDraft;
    expect(ev.endTime).not.toBe("");
    await hasEnd.setValue(false);
    ev = wrapper.findComponent(EventBuilder).props("event") as EventDraft;
    expect(ev.endTime).toBe("");
  });

  it("toggles categories and join mode", async () => {
    const { wrapper } = mountBuilder();
    await wrapper
      .findAll(".event__cat")
      .find((b) => b.text() === "Music")!
      .trigger("click");
    let ev = wrapper.findComponent(EventBuilder).props("event") as EventDraft;
    expect(ev.categories).toContain("MUSIC");
    const join = wrapper.get("[data-testid='event-join-mode']");
    await join.setValue("external");
    ev = wrapper.findComponent(EventBuilder).props("event") as EventDraft;
    expect(ev.joinMode).toBe("external");
    expect(wrapper.find("[data-testid='event-external-url']").exists()).toBe(
      true,
    );
  });

  it("adds place/virtual locations and edits the virtual URL", async () => {
    const { wrapper } = mountBuilder();
    const addBtns = wrapper.findAll(".event__loc-adds button");
    await addBtns[0].trigger("click");
    await addBtns[1].trigger("click");
    let ev = wrapper.findComponent(EventBuilder).props("event") as EventDraft;
    expect(ev.locations).toHaveLength(2);
    expect(ev.locations[0].kind).toBe("place");
    expect(ev.locations[1].kind).toBe("virtual");
    const locs = wrapper.findAll("[data-testid='event-location']");
    const urlInput = locs[1].find("input[type='url']");
    expect(urlInput.exists()).toBe(true);
    await urlInput.setValue("https://meet.example.com/x");
    ev = wrapper.findComponent(EventBuilder).props("event") as EventDraft;
    expect(ev.locations[1].url).toBe("https://meet.example.com/x");
  });

  it("shows the status picker only in edit mode", () => {
    const { wrapper: createW } = mountBuilder();
    expect(createW.find("[data-testid='event-status']").exists()).toBe(false);
    const EditHarness = defineComponent({
      setup() {
        const event = ref<EventDraft>(draft());
        return () =>
          h(EventBuilder, {
            event: event.value,
            title: "",
            visibility: "public",
            media: [],
            editing: true,
            onUpdate: (e: EventDraft) => {
              event.value = e;
            },
          });
      },
    });
    const { wrapper: editW } = mountWith(EditHarness);
    expect(editW.find("[data-testid='event-status']").exists()).toBe(true);
  });

  it("hides the invitee picker unless visibility is direct", () => {
    const { wrapper: pub } = mountBuilder();
    expect(pub.find("[data-testid='event-invitee-search']").exists()).toBe(
      false,
    );
    const { wrapper: dir } = mountBuilder(draft(), "direct");
    expect(dir.find("[data-testid='event-invitee-search']").exists()).toBe(
      true,
    );
  });

  it("invitee search adds account ids to the draft", async () => {
    const { wrapper } = mountBuilder(draft(), "direct");
    const input = wrapper.get("[data-testid='event-invitee-search']");
    await input.setValue("bob");
    await new Promise((r) => setTimeout(r, 350));
    await flushPromises();
    const result = wrapper.find(".event__invitee-results button");
    expect(result.exists()).toBe(true);
    await result.trigger("click");
    const ev = wrapper.findComponent(EventBuilder).props("event") as EventDraft;
    expect(ev.invitees).toEqual(["42"]);
    expect(wrapper.text()).toContain("@bob@remote.test");
  });

  it("hides the geocoding search unless a provider is configured", async () => {
    getInstanceMock.mockResolvedValue({ yonder: {} });
    const { wrapper } = mountBuilder(draft({ locations: [placeLocation()] }));
    await flushPromises();
    expect(wrapper.find("[data-testid='event-geo-search']").exists()).toBe(
      false,
    );
  });

  it("shows the geocoding search when a provider is configured", async () => {
    getInstanceMock.mockResolvedValue({
      yonder: { events_geocoding: "nominatim" },
    });
    const { wrapper } = mountBuilder(draft({ locations: [placeLocation()] }));
    await flushPromises();
    expect(wrapper.find("[data-testid='event-geo-search']").exists()).toBe(
      true,
    );
  });

  it("picking a geocoding suggestion fills the place location", async () => {
    getInstanceMock.mockResolvedValue({
      yonder: { events_geocoding: "nominatim" },
    });
    vi.mocked(geocodeSearch).mockResolvedValue([
      {
        name: "Cafe Central",
        street_address: "1 Main St",
        locality: "Berlin",
        region: "Berlin",
        postal_code: "10115",
        country: "Germany",
        address_text: "Cafe Central, 1 Main St, Berlin",
        latitude: 52.52,
        longitude: 13.405,
      },
    ]);
    const { wrapper } = mountBuilder(draft({ locations: [placeLocation()] }));
    await flushPromises();
    await wrapper.get("[data-testid='event-geo-search']").setValue("berlin");
    await new Promise((r) => setTimeout(r, 350));
    await flushPromises();
    const result = wrapper.find("[data-testid='event-geo-results'] button");
    expect(result.exists()).toBe(true);
    await result.trigger("click");
    const ev = wrapper.findComponent(EventBuilder).props("event") as EventDraft;
    expect(ev.locations[0].streetAddress).toBe("1 Main St");
    expect(ev.locations[0].locality).toBe("Berlin");
    expect(ev.locations[0].latitude).toBe("52.52");
    expect(ev.locations[0].longitude).toBe("13.405");
  });

  it("emits remove and title updates", async () => {
    const { wrapper } = mountBuilder();
    const builder = wrapper.findComponent(EventBuilder);
    await wrapper.get(".event__remove").trigger("click");
    expect(builder.emitted("remove")).toHaveLength(1);
    await wrapper.get("[data-testid='event-name']").setValue("Gig night");
    expect(builder.emitted("updateTitle")!.at(-1)![0]).toBe("Gig night");
  });
});
