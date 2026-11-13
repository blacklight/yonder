import { afterEach, describe, expect, it } from "vitest";
import { setRobotsNoindex, statusNoindex } from "./robots";
import type { Account, Status } from "@/api/entities";

const account = (noindex?: boolean) =>
  ({ id: "1", username: "u", acct: "u", noindex }) as Account;

const status = (over: Partial<Status>): Status =>
  ({ id: "1", account: account(), ...over }) as Status;

describe("setRobotsNoindex", () => {
  afterEach(() => {
    setRobotsNoindex(false);
    document.head.innerHTML = "";
  });

  it("adds and removes a robots meta tag", () => {
    expect(document.querySelector('meta[name="robots"]')).toBeNull();
    setRobotsNoindex(true);
    const meta = document.querySelector<HTMLMetaElement>('meta[name="robots"]');
    expect(meta?.content).toBe("noindex");
    setRobotsNoindex(false);
    expect(document.querySelector('meta[name="robots"]')).toBeNull();
  });

  it("does not strip a pre-existing robots tag it did not create", () => {
    const foreign = document.createElement("meta");
    foreign.name = "robots";
    foreign.content = "noindex, nofollow";
    document.head.appendChild(foreign);
    setRobotsNoindex(true);
    setRobotsNoindex(false);
    const metas = document.querySelectorAll('meta[name="robots"]');
    // Ours is removed; the foreign one survives untouched.
    expect(metas).toHaveLength(1);
    expect(metas[0].getAttribute("content")).toBe("noindex, nofollow");
  });
});

describe("statusNoindex", () => {
  it("is false without a status or a flagged account", () => {
    expect(statusNoindex(null)).toBe(false);
    expect(statusNoindex(status({}))).toBe(false);
  });

  it("flags the author, a boost wrapper and a quoted status", () => {
    expect(statusNoindex(status({ account: account(true) }))).toBe(true);
    expect(
      statusNoindex(status({ reblog: status({ account: account(true) }) })),
    ).toBe(true);
    expect(
      statusNoindex(
        status({
          quote: {
            state: "accepted",
            quoted_status: status({ account: account(true) }),
          },
        }),
      ),
    ).toBe(true);
    // An indexable quote inside a noindex author's post still flags —
    // the page presents the author's own content too.
    expect(
      statusNoindex(
        status({
          account: account(true),
          quote: { state: "accepted", quoted_status: status({}) },
        }),
      ),
    ).toBe(true);
  });
});
