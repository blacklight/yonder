import { onBeforeUnmount, type Directive } from "vue";

/**
 * Infinite scroll, the way the main timelines do it: observe a 1px
 * sentinel under the list and page in the next batch as it enters
 * the viewport.
 *
 * - `hasMore()` reports whether another page *can* be fetched right
 *   now — it must also return `false` while a fetch is in flight so
 *   a still-visible sentinel can't stack duplicate requests.
 * - `load()` performs the page fetch (append semantics).
 *
 * Pagination is eager: the observer reports the current state as soon
 * as the sentinel is observed, so a list shorter than the viewport
 * pulls the next page immediately; after each fetch the sentinel's
 * position is re-checked and the next page follows while it stays in
 * view.
 *
 * Usage:
 *   const { vInfinite, ioSupported } = useInfiniteScroll(hasMore, load);
 *   <div v-infinite class="…-sentinel" aria-hidden="true" />
 *
 * The directive binds the element itself, so a sentinel that mounts
 * late (lists gated behind async state) starts being observed the
 * moment it renders. `ioSupported` lets pages render a manual
 * "Load more" button as the no-IntersectionObserver fallback.
 */
export function useInfiniteScroll(
  hasMore: () => boolean,
  load: () => void | Promise<void>,
): { vInfinite: Directive<HTMLElement>; ioSupported: boolean } {
  const ioSupported = "IntersectionObserver" in window;
  let observer: IntersectionObserver | null = null;
  let inflight = false;
  let el: HTMLElement | null = null;

  async function fire() {
    if (inflight || !hasMore()) return;
    inflight = true;
    try {
      await load();
    } finally {
      inflight = false;
    }
    // Still pinned to the viewport after the append (short pages) —
    // keep going until the list outgrows it or pages run out.
    if (el?.isConnected && hasMore()) {
      const r = el.getBoundingClientRect();
      if (r.top < window.innerHeight && r.bottom > 0) void fire();
    }
  }

  if (ioSupported) {
    observer = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) void fire();
    });
  }

  const vInfinite: Directive<HTMLElement> = {
    mounted(e) {
      el = e;
      observer?.observe(e);
    },
    unmounted(e) {
      observer?.unobserve(e);
      if (el === e) el = null;
    },
  };
  onBeforeUnmount(() => observer?.disconnect());

  return { vInfinite, ioSupported };
}
