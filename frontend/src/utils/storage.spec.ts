import { afterEach, describe, expect, it, vi } from "vitest";
import { storageGet, storageRemove, storageSet } from "./storage";

// Firefox ETP / Tor Browser block all storage in third-party frames:
// the property exists but every access throws SecurityError.
function stubBlockedStorage() {
  const denied = () => {
    throw new DOMException("The operation is insecure.", "SecurityError");
  };
  vi.stubGlobal("localStorage", {
    getItem: denied,
    setItem: denied,
    removeItem: denied,
    clear: denied,
    key: denied,
    length: 0,
  });
}

const realStorage = localStorage;
afterEach(() => vi.stubGlobal("localStorage", realStorage));

describe("storage helpers", () => {
  it("read, write and remove values when storage is available", () => {
    storageSet("k", "v");
    expect(storageGet("k")).toBe("v");
    storageRemove("k");
    expect(storageGet("k")).toBeNull();
  });

  it("degrade to no-ops when storage access is blocked", () => {
    stubBlockedStorage();
    expect(storageGet("k")).toBeNull();
    expect(() => storageSet("k", "v")).not.toThrow();
    expect(() => storageRemove("k")).not.toThrow();
  });
});
