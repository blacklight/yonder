import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { useStreamingStore } from "./streaming";
import { useAuthStore } from "./auth";

class FakeSocket {
  static instances: FakeSocket[] = [];
  readyState: number = WebSocket.CONNECTING;
  sent: string[] = [];
  url: string;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(url: string) {
    this.url = url;
    FakeSocket.instances.push(this);
  }
  send(data: string) {
    this.sent.push(data);
  }
  close() {
    this.readyState = WebSocket.CLOSED;
    this.onclose?.();
  }
  open() {
    this.readyState = WebSocket.OPEN;
    this.onopen?.();
  }
  emit(frame: unknown) {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }
}

function latest(): FakeSocket {
  return FakeSocket.instances[FakeSocket.instances.length - 1];
}

describe("streaming store", () => {
  beforeEach(() => {
    FakeSocket.instances = [];
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
    const s = useStreamingStore();
    s.disconnect();
    s.setSocketFactory(null);
  });

  function store() {
    const s = useStreamingStore();
    s.setSocketFactory((url) => new FakeSocket(url) as unknown as WebSocket);
    return s;
  }

  it("connects with the access token and subscribes on open", () => {
    useAuthStore().setToken("tk");
    const s = store();
    const seen: string[] = [];
    s.subscribe("public", (e) => seen.push(e.event));
    expect(latest().url).toContain("/api/v1/streaming");
    expect(latest().url).toContain("access_token=tk");
    latest().open();
    expect(latest().sent).toEqual([
      JSON.stringify({ type: "subscribe", stream: "public" }),
    ]);
  });

  it("omits the token for anonymous sockets", () => {
    const s = store();
    s.subscribe("public", () => {});
    expect(latest().url).not.toContain("access_token");
  });

  it("folds hashtag/list params into the subscribe frame", () => {
    const s = store();
    s.subscribe("hashtag:rust", () => {});
    s.subscribe("list:7", () => {});
    latest().open();
    expect(latest().sent).toContain(
      JSON.stringify({ type: "subscribe", stream: "hashtag", tag: "rust" }),
    );
    expect(latest().sent).toContain(
      JSON.stringify({ type: "subscribe", stream: "list", list: "7" }),
    );
  });

  it("reference-counts shared subscriptions", () => {
    const s = store();
    const off1 = s.subscribe("public", () => {});
    const off2 = s.subscribe("public", () => {});
    latest().open();
    expect(latest().sent.filter((f) => f.includes("subscribe")).length).toBe(1);
    off1();
    // Still held by off2 — no unsubscribe frame yet.
    expect(latest().sent.filter((f) => f.includes("unsubscribe")).length).toBe(
      0,
    );
    off2();
    expect(latest().sent.filter((f) => f.includes("unsubscribe")).length).toBe(
      1,
    );
  });

  it("routes events to subscribers with a twice-parsed payload", () => {
    const s = store();
    const seen: { stream: string; event: string; payload: unknown }[] = [];
    s.subscribe("public", (e) => seen.push(e));
    s.subscribe("hashtag:rust", (e) => seen.push(e));
    latest().open();
    latest().emit({
      stream: ["public"],
      event: "update",
      payload: JSON.stringify({ id: "1" }),
    });
    latest().emit({
      stream: ["hashtag", "rust"],
      event: "delete",
      payload: JSON.stringify("9"),
    });
    expect(seen).toHaveLength(2);
    expect(seen[0]).toMatchObject({
      stream: "public",
      event: "update",
      payload: { id: "1" },
    });
    expect(seen[1]).toMatchObject({
      stream: "hashtag:rust",
      event: "delete",
      payload: "9",
    });
  });

  it("reconnects with backoff and resubscribes", () => {
    const s = store();
    s.subscribe("public", () => {});
    latest().open();
    latest().close();
    expect(FakeSocket.instances).toHaveLength(1);
    // ~1 s to the first retry.
    vi.advanceTimersByTime(999);
    expect(FakeSocket.instances).toHaveLength(1);
    vi.advanceTimersByTime(2);
    expect(FakeSocket.instances).toHaveLength(2);

    // A socket that never opened keeps the backoff counter — the
    // delay grows: ~2 s, then ~4 s.
    latest().close();
    vi.advanceTimersByTime(1999);
    expect(FakeSocket.instances).toHaveLength(2);
    vi.advanceTimersByTime(2);
    expect(FakeSocket.instances).toHaveLength(3);
    latest().close();
    vi.advanceTimersByTime(3999);
    expect(FakeSocket.instances).toHaveLength(3);
    vi.advanceTimersByTime(2);
    expect(FakeSocket.instances).toHaveLength(4);

    // On open the active subscriptions are replayed and the counter
    // resets.
    latest().open();
    expect(latest().sent).toContain(
      JSON.stringify({ type: "subscribe", stream: "public" }),
    );
  });

  it("does not reconnect after the last unsubscribe", () => {
    const s = store();
    const off = s.subscribe("public", () => {});
    latest().open();
    off();
    vi.advanceTimersByTime(60_000);
    expect(FakeSocket.instances).toHaveLength(1);
  });
});
