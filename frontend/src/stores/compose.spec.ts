import { describe, expect, it, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { useComposeStore } from "./compose";
import type { Status } from "@/api/entities";

function status(id: string, acct: string): Status {
  return { id, account: { acct } } as Status;
}

describe("compose store", () => {
  beforeEach(() => localStorage.removeItem("yonder.compose.draft"));

  it("openReply sets in_reply_to and seeds @author", () => {
    const c = useComposeStore();
    c.openReply(status("10", "bob@remote.test"));
    expect(c.draft.inReplyToId).toBe("10");
    expect(c.draft.replyToAcct).toBe("bob@remote.test");
    expect(c.draft.text).toBe("@bob@remote.test ");
    expect(c.openSignal).toBe(1);
  });

  it("openReply unwraps a boost", () => {
    const c = useComposeStore();
    const boosted = status("5", "carol");
    c.openReply({ id: "9", reblog: boosted } as Status);
    expect(c.draft.inReplyToId).toBe("5");
    expect(c.draft.replyToAcct).toBe("carol");
  });

  it("openQuote sets quote_id", () => {
    const c = useComposeStore();
    c.openQuote(status("7", "dan"));
    expect(c.draft.quoteId).toBe("7");
    expect(c.draft.quotedAcct).toBe("dan");
  });

  it("openMention seeds @acct and stays public", () => {
    const c = useComposeStore();
    c.openMention("frank@y.test");
    expect(c.draft.text).toBe("@frank@y.test ");
    expect(c.draft.visibility).toBe("public");
    expect(c.openSignal).toBe(1);
  });

  it("openDirect sets direct visibility and seeds the recipient", () => {
    const c = useComposeStore();
    c.openDirect("erin@x.test");
    expect(c.draft.visibility).toBe("direct");
    expect(c.draft.text).toBe("@erin@x.test ");
  });

  it("persists the draft to localStorage", () => {
    const c = useComposeStore();
    c.update({ text: "hello", visibility: "unlisted" });
    const raw = localStorage.getItem("yonder.compose.draft");
    expect(raw).toBeTruthy();
    expect(JSON.parse(raw!)).toMatchObject({
      text: "hello",
      visibility: "unlisted",
    });
  });

  it("isDirty reflects draft content", () => {
    const c = useComposeStore();
    expect(c.isDirty).toBe(false);
    c.update({ text: "x" });
    expect(c.isDirty).toBe(true);
    c.reset();
    expect(c.isDirty).toBe(false);
  });

  it("stash/unstash parks and restores a draft", () => {
    const c = useComposeStore();
    c.update({ text: "park me" });
    c.stash();
    c.reset();
    expect(c.draft.text).toBe("");
    c.unstash();
    expect(c.draft.text).toBe("park me");
    // The stash is single-use.
    c.reset();
    c.unstash();
    expect(c.draft.text).toBe("");
  });

  it("restores a persisted draft on a fresh store", () => {
    const c = useComposeStore();
    c.update({ text: "kept", spoilerText: "cw", cwEnabled: true });
    // A new pinia mimics a reload: loadDraft runs at store creation.
    setActivePinia(createPinia());
    const fresh = useComposeStore();
    expect(fresh.draft.text).toBe("kept");
    expect(fresh.draft.cwEnabled).toBe(true);
    fresh.reset();
    expect(fresh.draft.text).toBe("");
    expect(fresh.draft.inReplyToId).toBeNull();
    expect(JSON.parse(localStorage.getItem("yonder.compose.draft")!).text).toBe(
      "",
    );
  });
});
