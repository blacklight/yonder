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

  it("scales result bars against the leading option and marks it winner", () => {
    const { wrapper } = mountWith(StatusPoll, {
      props: { status: statusWithPoll(poll({ voted: true })) },
    });
    const results = wrapper.findAll(".poll__result");
    expect(results).toHaveLength(2);
    // "no" leads 2:1 — its bar spans the row, "yes" takes half.
    const bars = wrapper.findAll(".poll__result-bar");
    expect(bars[0].attributes("style")).toContain("width: 50%");
    expect(bars[1].attributes("style")).toContain("width: 100%");
    expect(results[1].classes()).toContain("poll__result--winner");
    expect(results[0].classes()).not.toContain("poll__result--winner");
    // Percentage share of the poll total sits next to the raw count.
    expect(results[0].find(".poll__result-percent").text()).toBe("33%");
    expect(results[1].find(".poll__result-percent").text()).toBe("67%");
    expect(results[1].find(".poll__result-votes").text()).toBe("2");
    // The poll is still open — no winner check yet.
    expect(wrapper.find(".poll__result-check").exists()).toBe(false);
  });

  it("marks tied leaders as winners without a check while open", () => {
    const { wrapper } = mountWith(StatusPoll, {
      props: {
        status: statusWithPoll(
          poll({
            votes_count: 4,
            voters_count: 4,
            voted: true,
            options: [
              { title: "yes", votes_count: 2 },
              { title: "no", votes_count: 2 },
            ],
          }),
        ),
      },
    });
    for (const result of wrapper.findAll(".poll__result")) {
      expect(result.classes()).toContain("poll__result--winner");
    }
    for (const bar of wrapper.findAll(".poll__result-bar")) {
      expect(bar.attributes("style")).toContain("width: 100%");
    }
    expect(wrapper.find(".poll__result-check").exists()).toBe(false);
  });

  it("marks the winning option with a check once the poll closed", () => {
    const { wrapper } = mountWith(StatusPoll, {
      props: {
        status: statusWithPoll(
          poll({ expired: true, expires_at: "2026-11-01T23:34:00Z" }),
        ),
      },
    });
    const checks = wrapper.findAll(".poll__result-check");
    expect(checks).toHaveLength(1);
    const winner = wrapper.findAll(".poll__result--winner");
    expect(winner).toHaveLength(1);
    expect(winner[0].find(".poll__result-check").exists()).toBe(true);
  });

  it("renders zero-width bars and no winner on an empty poll", () => {
    const { wrapper } = mountWith(StatusPoll, {
      props: {
        status: statusWithPoll(
          poll({
            expired: true,
            votes_count: 0,
            voters_count: 0,
            options: [
              { title: "yes", votes_count: 0 },
              { title: "no", votes_count: null },
            ],
          }),
        ),
      },
    });
    for (const bar of wrapper.findAll(".poll__result-bar")) {
      expect(bar.attributes("style")).toContain("width: 0%");
    }
    expect(wrapper.find(".poll__result--winner").exists()).toBe(false);
    expect(wrapper.find(".poll__result-check").exists()).toBe(false);
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
