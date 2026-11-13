import { onBeforeUnmount, watch, type WatchSource } from "vue";
import type { Status } from "@/api/entities";

// Search-engine opt-out (`users.indexable = false` → Account.noindex):
// the served HTML can't know the account at request time, so pages
// that render a noindex account's content set
// `<meta name="robots" content="noindex">` client-side for
// JS-rendering crawlers. The backend injects the same tag into the
// raw shell for profile/status paths; this covers client-side
// navigation between them.

const SELECTOR = 'meta[name="robots"][content="noindex"]';

let el: HTMLMetaElement | null = null;

/** Reflect the current page's noindex state in `<head>`. Adopts a
 *  server-injected tag when one is present so its lifecycle is the
 *  page's, not the document's. */
export function setRobotsNoindex(on: boolean): void {
  if (on) {
    if (!el || !el.isConnected) {
      el =
        document.head.querySelector<HTMLMetaElement>(SELECTOR) ??
        (() => {
          const meta = document.createElement("meta");
          meta.name = "robots";
          meta.content = "noindex";
          document.head.appendChild(meta);
          return meta;
        })();
    }
    el.content = "noindex";
  } else {
    el?.remove();
    el = null;
  }
}

/** Drop every noindex robots tag — called on route changes so a tag
 *  injected for (or set by) the previous page can't go stale. The
 *  incoming page's own watcher re-adds it when still needed. */
export function clearRobotsNoindex(): void {
  document.head
    .querySelectorAll<HTMLMetaElement>(SELECTOR)
    .forEach((meta) => meta.remove());
  el = null;
}

/** Keep the robots meta in sync with `source` for the calling
 *  component's lifetime; cleared on unmount. Call during `setup`. */
export function useRobotsNoindex(source: WatchSource<boolean>): void {
  watch(source, setRobotsNoindex, { immediate: true });
  onBeforeUnmount(() => setRobotsNoindex(false));
}

/** Whether `status` (or anything it embeds — boost wrapper, quote)
 *  belongs to a noindex account. */
export function statusNoindex(status: Status | null | undefined): boolean {
  if (!status) return false;
  return Boolean(
    status.account?.noindex ||
    statusNoindex(status.reblog) ||
    statusNoindex(status.quote?.quoted_status),
  );
}
