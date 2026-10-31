/**
 * Compare two entity ids (decimal snowflake strings) numerically
 * without going through `Number` — values exceed
 * `Number.MAX_SAFE_INTEGER`, so `Number()` would silently corrupt
 * them. Snowflakes order chronologically, so this doubles as a
 * time comparator. Shorter decimals are always smaller; equal
 * lengths compare lexicographically.
 */
export function compareIds(a: string, b: string): number {
  return a.length - b.length || (a < b ? -1 : a > b ? 1 : 0);
}
