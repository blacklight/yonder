// The BOOTSTRAP date ladder:
//   < 1 min  → "42s"
//   < 1 h    → "5m"
//   < 1 day  → "7h"
//   < 1 week → "3d"
//   < 1 year → "Jan 5"   (locale month abbreviation)
//   older    → "Jan 5, 2024"

const MONTH_FMT = new Map<string, Intl.DateTimeFormat>();

function monthAbbrev(d: Date, locale: string): string {
  let fmt = MONTH_FMT.get(locale);
  if (!fmt) {
    fmt = new Intl.DateTimeFormat(locale, { month: "short" });
    MONTH_FMT.set(locale, fmt);
  }
  return fmt.format(d);
}

function activeLocale(): string {
  return document.documentElement.lang || "en";
}

/** Absolute localised form for tooltips ("Jan 5, 2024, 13:45"). */
export function formatAbsoluteDate(
  iso: string,
  locale = activeLocale(),
): string {
  return new Intl.DateTimeFormat(locale, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(iso));
}

export function formatRelativeDate(
  iso: string,
  now: Date = new Date(),
  locale = activeLocale(),
): string {
  const then = new Date(iso);
  const seconds = Math.max(
    0,
    Math.floor((now.getTime() - then.getTime()) / 1000),
  );
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d`;
  const yearMs = 365.25 * 24 * 3600 * 1000;
  const month = monthAbbrev(then, locale);
  const label = `${month} ${then.getDate()}`;
  if (now.getTime() - then.getTime() < yearMs) return label;
  return `${label}, ${then.getFullYear()}`;
}
