/** IANA timezone helpers for the event composer — `datetime-local`
 * inputs are wall-clock strings in the event's zone; the wire format
 * is RFC 3339 with the zone's UTC offset. */

/** The browser's IANA zone (`Intl` fallback chain). */
export function browserTimeZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
}

/** All IANA zones the runtime knows, for the timezone `<select>`.
 * `supportedValuesOf` is baseline in modern browsers; the fallback
 * keeps a minimal list working. */
export function timeZones(): string[] {
  try {
    const supported = (
      Intl as unknown as { supportedValuesOf?: (k: string) => string[] }
    ).supportedValuesOf;
    if (supported) return supported.call(Intl, "timeZone");
  } catch {
    /* fall through */
  }
  return ["UTC"];
}

/** Milliseconds the given IANA zone is ahead of UTC at `utcMs`. */
function tzOffsetMs(utcMs: number, timeZone: string): number {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone,
    hourCycle: "h23",
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).formatToParts(new Date(utcMs));
  const get = (type: string) =>
    Number(parts.find((p) => p.type === type)?.value ?? 0);
  const asUtc = Date.UTC(
    get("year"),
    get("month") - 1,
    get("day"),
    get("hour"),
    get("minute"),
    get("second"),
  );
  return asUtc - utcMs;
}

const LOCAL_RE = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})/;

/** `datetime-local` wall time in `timeZone` → RFC 3339 with the
 * zone's offset (`2030-06-01T18:00` + `Europe/Rome` →
 * `2030-06-01T18:00:00+02:00`). The offset is resolved at the target
 * instant and re-checked once so DST boundaries land correctly.
 * `null` on malformed input. */
export function zonedLocalToRfc3339(
  local: string,
  timeZone: string,
): string | null {
  const m = LOCAL_RE.exec(local.trim());
  if (!m) return null;
  const [, y, mo, d, h, mi] = m;
  const wallAsUtc = Date.UTC(
    Number(y),
    Number(mo) - 1,
    Number(d),
    Number(h),
    Number(mi),
  );
  // First guess at the instant, then refine once — the offset can
  // differ across a DST transition inside the guess window.
  let utc = wallAsUtc - tzOffsetMs(wallAsUtc, timeZone);
  utc = wallAsUtc - tzOffsetMs(utc, timeZone);
  const offsetSec = Math.round((wallAsUtc - utc) / 1000);
  const sign = offsetSec < 0 ? "-" : "+";
  const abs = Math.abs(offsetSec);
  const oh = String(Math.floor(abs / 3600)).padStart(2, "0");
  const om = String(Math.floor((abs % 3600) / 60)).padStart(2, "0");
  return `${y}-${mo}-${d}T${h}:${mi}:00${sign}${oh}:${om}`;
}

/** RFC 3339 instant → `datetime-local` wall time in `timeZone`
 * (edit hydration). `null` on unparseable input. */
export function rfc3339ToZonedLocal(
  iso: string,
  timeZone: string,
): string | null {
  const utcMs = Date.parse(iso);
  if (Number.isNaN(utcMs)) return null;
  const shifted = new Date(utcMs + tzOffsetMs(utcMs, timeZone));
  const p = (n: number) => String(n).padStart(2, "0");
  return (
    `${shifted.getUTCFullYear()}-${p(shifted.getUTCMonth() + 1)}-${p(shifted.getUTCDate())}` +
    `T${p(shifted.getUTCHours())}:${p(shifted.getUTCMinutes())}`
  );
}
