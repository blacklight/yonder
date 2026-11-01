/**
 * Third-party contexts (a cross-site iframe under Firefox ETP, Tor
 * Browser, LibreWolf hardened mode) block Web Storage: reading the
 * `localStorage` getter itself throws a SecurityError DOMException.
 * These helpers degrade storage to a best-effort no-op so the app
 * still boots — anonymously and without persistence — inside e.g.
 * `/statuses/{id}/embed`.
 */
export function storageGet(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function storageSet(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* storage unavailable — persistence silently skipped */
  }
}

export function storageRemove(key: string): void {
  try {
    localStorage.removeItem(key);
  } catch {
    /* storage unavailable */
  }
}
