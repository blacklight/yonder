import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useAuthStore } from "@/stores/auth";
import StatusPoll from "./StatusPoll.vue";
import type { Poll, Status } from "@/api/entities";

function poll(over: Partial<Poll> = {}): Poll {
  return {
    id: "9",
    expires_at: null,
    expired: false,
    multiple: false,
    votes_count: 3,
    voters_count: 3,
    voted: false,
    own_votes: [],
    options: [
      { title: "yes", votes_count: 1 },
      { title: "no", votes_count: 2 },
    ],
    ...over,
  };
}

function statusWithPoll(p: Poll): Status {
  return { id: "1", poll: p } as Status;
}

describe("StatusPoll", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAuthStore().setToken("t");
  });

  it("namespaces the input group per poll id", () => {
    const { wrapper } = mountWith(StatusPoll, {
      props: { status: statusWithPoll(poll()) },
    });
    for (const input of wrapper.findAll("input")) {
      expect(input.attributes("name")).toBe("poll-9");
    }
  });

  it("renders options for a multiple poll as checkboxes", () => {
    const { wrapper } = mountWith(StatusPoll, {
      props: { status: statusWithPoll(poll({ multiple: true })) },
    });
    for (const input of wrapper.findAll("input")) {
      expect(input.attributes("type")).toBe("checkbox");
    }
  });

  it("shows results for an already-voted or expired poll", () => {
    const { wrapper } = mountWith(StatusPoll, {
      props: { status: statusWithPoll(poll({ voted: true })) },
    });
    expect(wrapper.find("input").exists()).toBe(false);
    expect(wrapper.text()).toContain("3 votes");
  });

  it("votes and applies the returned counts", async () => {
    const updated = poll({
      votes_count: 4,
      voters_count: 4,
      voted: true,
      own_votes: [0],
      options: [
        { title: "yes", votes_count: 2 },
        { title: "no", votes_count: 2 },
      ],
    });
    const spy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() =>
        Promise.resolve(new Response(JSON.stringify(updated), { status: 200 })),
      );
    const status = statusWithPoll(poll());
    const { wrapper } = mountWith(StatusPoll, { props: { status } });
    await wrapper.findAll("input")[0].trigger("change");
    await wrapper.get(".poll__vote").trigger("click");
    await flushPromises();
    const body = JSON.parse(spy.mock.calls[0][1]!.body as string) as {
      choices: number[];
    };
    expect(body.choices).toEqual([0]);
    // The shared entity was replaced — fresh counts render immediately.
    expect(status.poll).toEqual(updated);
    expect(wrapper.text()).toContain("4 votes");
    expect(wrapper.findAll(".poll__result")).toHaveLength(2);
  });

  it("shows an error and keeps the form on a failed vote", async () => {
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(
        new Response(JSON.stringify({ error: "nope" }), { status: 500 }),
      ),
    );
    const { wrapper } = mountWith(StatusPoll, {
      props: { status: statusWithPoll(poll()) },
    });
    await wrapper.findAll("input")[0].trigger("change");
    await wrapper.get(".poll__vote").trigger("click");
    await flushPromises();
    expect(wrapper.find(".poll__error").text()).toBeTruthy();
    expect(wrapper.find("input").exists()).toBe(true);
  });
});
