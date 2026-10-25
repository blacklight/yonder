import { createI18n } from "vue-i18n";
import en from "./locales/en.json";

const STORAGE_LOCALE_KEY = "yonder.locale";

const localeLoaders: Record<string, () => Promise<Record<string, unknown>>> = {
  en: () => Promise.resolve(en as Record<string, unknown>),
  es: () => import("./locales/es.json").then((m) => m.default),
  it: () => import("./locales/it.json").then((m) => m.default),
  de: () => import("./locales/de.json").then((m) => m.default),
  ar: () => import("./locales/ar.json").then((m) => m.default),
};

const RTL_LOCALES = new Set([
  "ar",
  "ckb",
  "dv",
  "fa",
  "he",
  "ps",
  "sd",
  "ug",
  "ur",
  "yi",
]);

export function isRtlLocale(locale: string): boolean {
  return RTL_LOCALES.has(locale.toLowerCase().split("-")[0]);
}

// CLDR plural categories for Arabic, in pipe order:
// zero | one | two | few (n%100 in 3-10) | many (n%100 in 11-99) | other.
function arabicPluralRule(choice: number, choicesLength: number): number {
  let index: number;
  if (choice === 0) index = 0;
  else if (choice === 1) index = 1;
  else if (choice === 2) index = 2;
  else {
    const mod100 = choice % 100;
    index = mod100 >= 3 && mod100 <= 10 ? 3 : mod100 >= 11 ? 4 : 5;
  }
  return Math.min(index, choicesLength - 1);
}

export function getSupportedLocales(): string[] {
  return Object.keys(localeLoaders);
}

export function getStoredLocale(): string | null {
  return localStorage.getItem(STORAGE_LOCALE_KEY) || null;
}

export function storeLocale(locale: string): void {
  localStorage.setItem(STORAGE_LOCALE_KEY, locale);
}

function navigatorLocale(): string {
  for (const lang of navigator.languages ?? []) {
    const base = lang.toLowerCase().split("-")[0];
    if (base in localeLoaders) return base;
  }
  return "en";
}

export const i18n = createI18n({
  legacy: false,
  locale: "en",
  fallbackLocale: "en",
  messages: { en },
  pluralRules: { ar: arabicPluralRule },
});

// Resolution order: stored choice → navigator.languages → "en".
// The server's `interface_language` is applied by the settings sync
// once the session is known (epic 25).
export function initialLocale(): string {
  return getStoredLocale() ?? navigatorLocale();
}

export async function applyLocale(locale: string): Promise<void> {
  if (!(locale in localeLoaders)) locale = "en";
  const messages = await localeLoaders[locale]();
  // Cast to the locale shape inferred from en.json — every locale
  // ships the same keys (the completeness spec enforces it).
  i18n.global.setLocaleMessage(locale as "en", messages as typeof en);
  i18n.global.locale.value = locale as "en";
  storeLocale(locale);
  document.documentElement.lang = locale;
  document.documentElement.dir = isRtlLocale(locale) ? "rtl" : "ltr";
}

export async function loadInitialLocale(): Promise<void> {
  await applyLocale(initialLocale());
}
