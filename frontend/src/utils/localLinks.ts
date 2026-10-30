import type { Router } from "vue-router";

/**
 * Delegated click handler for sanitized-content links. `sanitizeHtml`
 * rewrites mention/hashtag/same-origin anchors to `/`-paths — this
 * routes them through the SPA (no full reload, no new tab). Mounted
 * once at the app root: any anchor click that nobody else handled
 * (`router-link` prevents default), isn't a download/`_blank`, and is
 * a plain unmodified left click on a local path becomes a router push.
 */
export function localLinkNavHandler(
  router: Pick<Router, "push">,
): (e: MouseEvent) => void {
  return (e) => {
    if (
      e.defaultPrevented ||
      e.button !== 0 ||
      e.metaKey ||
      e.ctrlKey ||
      e.shiftKey ||
      e.altKey
    )
      return;
    const a = (e.target as Element | null)?.closest?.("a[href]");
    if (!a || a.hasAttribute("download") || a.getAttribute("target")) return;
    const href = a.getAttribute("href") ?? "";
    if (!href.startsWith("/")) return;
    e.preventDefault();
    void router.push(href);
  };
}
