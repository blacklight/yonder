import { afterEach, describe, expect, it, vi } from "vitest";
import {
  applyLocale,
  isRtlLocale,
  getSupportedLocales,
  initialLocale,
  loadInitialLocale,
} from "./index";
import en from "./locales/en.json";
import es from "./locales/es.json";
import it_ from "./locales/it.json";
import de from "./locales/de.json";
import ar from "./locales/ar.json";

function keys(obj: Record<string, unknown>, prefix = ""): string[] {
  return Object.entries(obj).flatMap(([k, v]) =>
    v && typeof v === "object" && !Array.isArray(v)
      ? keys(v as Record<string, unknown>, `${prefix}${k}.`)
      : [`${prefix}${k}`],
  );
}

describe("locale completeness", () => {
  const locales = { es, it: it_, de, ar } as const;
  it.each(Object.keys(locales))("locale %s has every en.json key", (code) => {
    const missing = keys(en as Record<string, unknown>).filter(
      (k) => !(k in flatten(locales[code as keyof typeof locales])),
    );
    expect(missing).toEqual([]);
  });
});

function flatten(
  obj: Record<string, unknown>,
  prefix = "",
  out: Record<string, true> = {},
) {
  for (const [k, v] of Object.entries(obj)) {
    if (v && typeof v === "object" && !Array.isArray(v))
      flatten(v as Record<string, unknown>, `${prefix}${k}.`, out);
    else out[`${prefix}${k}`] = true;
  }
  return out;
}

describe("applyLocale", () => {
  it("sets lang and dir for an RTL locale", async () => {
    await applyLocale("ar");
    expect(document.documentElement.lang).toBe("ar");
    expect(document.documentElement.dir).toBe("rtl");
    expect(localStorage.getItem("yonder.locale")).toBe("ar");
  });
  it("falls back to en for unknown locales", async () => {
    await applyLocale("fr");
    expect(document.documentElement.lang).toBe("en");
    expect(document.documentElement.dir).toBe("ltr");
  });
});

describe("storage-blocked contexts (third-party iframe)", () => {
  const realStorage = localStorage;
  afterEach(() => vi.stubGlobal("localStorage", realStorage));

  it("falls back to the navigator locale without throwing", async () => {
    // Firefox ETP / Tor deny Web Storage in cross-site frames.
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
    expect(initialLocale()).toBe("en");
    await expect(loadInitialLocale()).resolves.toBeUndefined();
  });
});

describe("locale set", () => {
  it("matches the backend's SUPPORTED_LOCALES", () => {
    expect(getSupportedLocales().sort()).toEqual([
      "ar",
      "de",
      "en",
      "es",
      "it",
    ]);
  });
  it("recognizes RTL languages", () => {
    expect(isRtlLocale("ar")).toBe(true);
    expect(isRtlLocale("he-IL")).toBe(true);
    expect(isRtlLocale("en")).toBe(false);
  });
});
