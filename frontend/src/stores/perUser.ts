import { watch } from "vue";
import { useAuthStore } from "./auth";

const resets = new Set<() => void>();
let wired = false;

/**
 * Register a per-user state reset. Fires whenever the auth token
 * changes or is cleared — sign-out, the client's 401 eviction, and
 * account deletion — so a shared browser never shows the previous
 * user's DMs, notifications or drafts.
 */
export function resetOnUserChange(reset: () => void) {
  resets.add(reset);
  if (wired) return;
  wired = true;
  const auth = useAuthStore();
  watch(
    () => auth.token,
    () => {
      for (const r of resets) r();
    },
  );
}
