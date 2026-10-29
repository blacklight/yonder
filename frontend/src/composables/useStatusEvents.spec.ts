import { describe, expect, it, afterEach } from "vitest";
import { defineComponent, h } from "vue";
import { mountWith } from "@/test/render";
import { useStreamingStore } from "@/stores/streaming";
import { useAuthStore } from "@/stores/auth";
import { useStatusEvents } from "./useStatusEvents";
import type { Status } from "@/api/entities";

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

function frame(stream: string[], event: string, payload: unknown) {
  return { stream, event, payload: JSON.stringify(payload) };
}

describe("useStatusEvents", () => {
  afterEach(() => {
    const s = useStreamingStore();
    s.disconnect();
    s.setSocketFactory(null);
    FakeSocket.instances = [];
  });

  function probe() {
    const seen: Status[] = [];
    const deleted: string[] = [];
    const Probe = defineComponent({
      setup() {
        useStatusEvents({
          onStatus: (s) => seen.push(s),
          onDelete: (id) => deleted.push(id),
        });
        return () => h("div");
      },
    });
    const mounted = mountWith(Probe);
    return { seen, deleted, wrapper: mounted.wrapper };
  }

  it("subscribes public, and user when logged in", () => {
    useAuthStore().setToken("tk");
    const s = useStreamingStore();
    s.setSocketFactory((url) => new FakeSocket(url) as unknown as WebSocket);
    probe();
    latest().open();
    expect(latest().sent).toEqual(
      expect.arrayContaining([
        JSON.stringify({ type: "subscribe", stream: "public" }),
        JSON.stringify({ type: "subscribe", stream: "user" }),
      ]),
    );
  });

  it("routes update/status.update to onStatus and delete to onDelete", () => {
    useAuthStore().setToken("tk");
    const s = useStreamingStore();
    s.setSocketFactory((url) => new FakeSocket(url) as unknown as WebSocket);
    const { seen, deleted } = probe();
    latest().open();
    latest().emit(frame(["public"], "update", { id: "1" }));
    latest().emit(frame(["user"], "status.update", { id: "2" }));
    latest().emit(frame(["public"], "delete", "3"));
    latest().emit(frame(["user"], "notification", { id: "9" }));
    expect(seen.map((x) => x.id)).toEqual(["1", "2"]);
    expect(deleted).toEqual(["3"]);
  });

  it("unsubscribes on unmount", () => {
    const s = useStreamingStore();
    s.setSocketFactory((url) => new FakeSocket(url) as unknown as WebSocket);
    const { seen, wrapper } = probe();
    latest().open();
    wrapper.unmount();
    latest().emit(frame(["public"], "update", { id: "1" }));
    expect(seen).toHaveLength(0);
  });
});
