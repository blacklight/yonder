import { defineStore } from "pinia";
import { ref } from "vue";
import { useAuthStore } from "./auth";

/**
 * Mastodon `/api/v1/streaming` protocol:
 * - connect: `ws(s)://<host>/api/v1/streaming?access_token=<token>`
 * - client frames: `{"type":"subscribe","stream":"<name>", …params}`
 *   (parameterized streams fold a param: `hashtag` + `tag`,
 *   `list` + `list`)
 * - server frames: `{"stream": [...], "event": "<name>",
 *   "payload": "<json-string>"}` — `payload` is a JSON-encoded
 *   *string* and needs a second `JSON.parse`. The `stream` array is
 *   the subscription's wire form: `["public"]`, `["hashtag","rust"]`,
 *   `["list","7"]` — joined with `:` they match the timeline keys.
 */

export interface StreamEvent {
  /** Folded subscription name (`public`, `hashtag:rust`, `list:7`). */
  stream: string;
  /** `update` | `delete` | `notification` | `notifications_count`. */
  event: string;
  /** The twice-parsed payload (entity, or id string for `delete`). */
  payload: unknown;
}

export type StreamHandler = (event: StreamEvent) => void;

export type SocketFactory = (url: string) => WebSocket;

interface Subscription {
  count: number;
  handlers: Set<StreamHandler>;
}

const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 30000;

/** Folded name → the subscribe frame's `stream` + params. */
function frameFor(key: string): {
  stream: string;
  params: Record<string, string>;
} {
  const [head, ...rest] = key.split(":");
  if (head === "hashtag" && rest.length === 1)
    return { stream: "hashtag", params: { tag: rest[0] } };
  if (head === "hashtag" && rest[0] === "local" && rest.length === 2)
    return { stream: "hashtag:local", params: { tag: rest[1] } };
  if (head === "list" && rest.length === 1)
    return { stream: "list", params: { list: rest[0] } };
  return { stream: key, params: {} };
}

/**
 * One shared socket per tab. Subscriptions are reference-counted so
 * several mounted timelines can share a stream; the socket reconnects
 * with exponential backoff and replays active subscriptions.
 */
export const useStreamingStore = defineStore("streaming", () => {
  const status = ref<"idle" | "connecting" | "open" | "closed">("idle");

  const subs = new Map<string, Subscription>();
  let socket: WebSocket | null = null;
  let factory: SocketFactory | null = null;
  let attempt = 0;
  let timer: ReturnType<typeof setTimeout> | null = null;
  // When true, a `close` event reconnects (set on subscribe, cleared
  // when the last subscription goes away).
  let wantOpen = false;

  /** Test hook: substitute a fake socket. */
  function setSocketFactory(f: SocketFactory | null) {
    factory = f;
  }

  function url(): string {
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    const u = new URL(`/api/v1/streaming`, `${proto}//${window.location.host}`);
    const token = useAuthStore().token;
    if (token) u.searchParams.set("access_token", token);
    return u.toString();
  }

  function send(obj: Record<string, unknown>) {
    if (socket?.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(obj));
    }
  }

  function sendSubscribe(key: string) {
    const { stream, params } = frameFor(key);
    send({ type: "subscribe", stream, ...params });
  }

  function resubscribeAll() {
    for (const key of subs.keys()) {
      sendSubscribe(key);
    }
  }

  function scheduleReconnect() {
    if (timer !== null || !wantOpen) return;
    const delay = Math.min(RECONNECT_MAX_MS, RECONNECT_BASE_MS * 2 ** attempt);
    attempt++;
    timer = setTimeout(() => {
      timer = null;
      connect();
    }, delay);
  }

  function dispatch(raw: unknown) {
    const frame = raw as {
      stream?: string[];
      event?: string;
      payload?: string;
    };
    if (!Array.isArray(frame.stream) || typeof frame.event !== "string") return;
    const name = frame.stream.join(":");
    let payload: unknown = frame.payload;
    if (typeof frame.payload === "string") {
      try {
        payload = JSON.parse(frame.payload);
      } catch {
        payload = frame.payload;
      }
    }
    const event: StreamEvent = { stream: name, event: frame.event, payload };
    for (const h of subs.get(name)?.handlers ?? []) h(event);
  }

  function connect() {
    if (
      socket &&
      (socket.readyState === WebSocket.OPEN ||
        socket.readyState === WebSocket.CONNECTING)
    ) {
      return;
    }
    status.value = "connecting";
    const make = factory ?? ((u: string) => new WebSocket(u));
    const ws = make(url());
    socket = ws;
    ws.onopen = () => {
      if (socket !== ws) return;
      status.value = "open";
      attempt = 0;
      resubscribeAll();
    };
    ws.onmessage = (ev) => {
      if (socket !== ws) return;
      try {
        dispatch(JSON.parse(ev.data as string));
      } catch {
        // Unparseable frame — ignore.
      }
    };
    ws.onclose = () => {
      if (socket !== ws) return;
      socket = null;
      status.value = "closed";
      scheduleReconnect();
    };
    ws.onerror = () => {
      // `close` follows and drives the reconnect.
      if (socket === ws) ws.close();
    };
  }

  function disconnect() {
    wantOpen = false;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    const ws = socket;
    socket = null;
    status.value = "idle";
    ws?.close();
  }

  /**
   * Subscribe to a folded stream name (`public`, `user`,
   * `hashtag:rust`, `list:7`). Returns the unsubscribe function.
   */
  function subscribe(key: string, handler: StreamHandler): () => void {
    let sub = subs.get(key);
    if (!sub) {
      sub = { count: 0, handlers: new Set() };
      subs.set(key, sub);
    }
    sub.count++;
    sub.handlers.add(handler);
    wantOpen = true;
    connect();
    if (status.value === "open") sendSubscribe(key);

    return () => {
      const s = subs.get(key);
      if (!s) return;
      s.handlers.delete(handler);
      s.count--;
      if (s.count <= 0) {
        subs.delete(key);
        const { stream, params } = frameFor(key);
        send({ type: "unsubscribe", stream, ...params });
        if (subs.size === 0) disconnect();
      }
    };
  }

  return { status, subscribe, setSocketFactory, connect, disconnect };
});
