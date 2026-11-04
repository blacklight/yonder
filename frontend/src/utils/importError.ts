const REASONS = ["forbidden", "invalid", "failed"] as const;

/** i18n key for the fixed `?import_error=` reason the OAuth callback
 * returns. Anything outside the allowlist is treated as `failed`. */
export function importErrorKey(raw: unknown): string {
  const reason = REASONS.find((r) => r === raw) ?? "failed";
  return `settingsData.importError.${reason}`;
}
