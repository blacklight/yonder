export const API_PREFIX = "/api/v1";

export function buildUrl(
  path: string,
  query?: Record<
    string,
    string | number | boolean | Array<string | number> | undefined | null
  >,
): string {
  if (!query) return path;
  const pairs: string[] = [];
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined || value === null) continue;
    // Arrays serialize as repeated params (`types[]=a&types[]=b`) —
    // Mastodon's batch/filter query convention.
    const values = Array.isArray(value) ? value : [value];
    for (const v of values) {
      pairs.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(v))}`);
    }
  }
  if (pairs.length === 0) return path;
  const sep = path.includes("?") ? "&" : "?";
  return `${path}${sep}${pairs.join("&")}`;
}
