import { beforeEach, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";

function createFakeStorage(): Storage {
  const store: Record<string, string> = {};
  return {
    getItem: (k: string) => store[k] ?? null,
    setItem: (k: string, v: string) => {
      store[k] = v;
    },
    removeItem: (k: string) => {
      delete store[k];
    },
    clear: () => {
      for (const k of Object.keys(store)) delete store[k];
    },
    key: (i: number) => Object.keys(store)[i] ?? null,
    get length() {
      return Object.keys(store).length;
    },
  } as Storage;
}

const local = createFakeStorage();
const session = createFakeStorage();
vi.stubGlobal("localStorage", local);
vi.stubGlobal("sessionStorage", session);

Object.defineProperty(window, "matchMedia", {
  writable: true,
  value: vi.fn((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addListener: vi.fn(),
    removeListener: vi.fn(),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  })),
});

// No real WebSocket in jsdom tests — the streaming store injects its
// own factory in specs; anywhere else a silent stub prevents noise.
class StubWebSocket {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  readyState = StubWebSocket.CLOSED;
  onopen: unknown = null;
  onmessage: unknown = null;
  onclose: unknown = null;
  onerror: unknown = null;
  send() {}
  close() {}
}
vi.stubGlobal("WebSocket", StubWebSocket);

beforeEach(() => {
  setActivePinia(createPinia());
  local.clear();
  session.clear();
});
