import { defineStore } from "pinia";
import type { Status } from "@/api/entities";
import {
  dropFromStatusList,
  patchStatusList,
  statusMatchesId,
} from "@/utils/statusStream";
import {
  homeTimeline,
  listTimeline,
  publicTimeline,
  tagTimeline,
  type TimelineQuery,
} from "@/api/statuses";
import type { Page } from "@/api/pagination";

/**
 * Timeline keys mirror the live stream names so the streaming store
 * routes events by key: `home` (stream `user`), `public`,
 * `public:local`, `hashtag:<name>`, `list:<id>`.
 */
export type TimelineKey = string;

export interface TimelineState {
  items: Status[];
  /** Cursor query for the next page (parsed from the Link header). */
  next?: TimelineQuery;
  loading: boolean;
  error: string | null;
  loaded: boolean;
  /** Live events held back until the user flushes them in. */
  pending: Status[];
}

function empty(): TimelineState {
  return {
    items: [],
    loading: false,
    error: null,
    loaded: false,
    pending: [],
  };
}

function fetchPage(key: TimelineKey, query?: TimelineQuery) {
  if (key === "home") return homeTimeline(query);
  if (key === "public") return publicTimeline(query);
  if (key === "public:local") return publicTimeline({ ...query, local: true });
  if (key.startsWith("hashtag:")) return tagTimeline(key.slice(8), query);
  if (key.startsWith("list:")) return listTimeline(key.slice(5), query);
  throw new Error(`unknown timeline key: ${key}`);
}

/**
 * The Link header's `next` is an absolute URL; only the cursor
 * params matter — pull them into a TimelineQuery.
 */
function nextQuery(next: string | undefined): TimelineQuery | undefined {
  if (!next) return undefined;
  try {
    const url = new URL(next, window.location.origin);
    const q: TimelineQuery = {};
    for (const k of ["max_id", "min_id", "since_id", "limit"] as const) {
      const v = url.searchParams.get(k);
      if (v !== null) {
        if (k === "limit") q[k] = Number(v);
        else q[k] = v;
      }
    }
    return q;
  } catch {
    return undefined;
  }
}

function dedupe(items: Status[]): Status[] {
  const seen = new Set<string>();
  return items.filter((s) => (seen.has(s.id) ? false : (seen.add(s.id), true)));
}

export const useTimelineStore = defineStore("timeline", {
  state: () => ({
    timelines: {} as Record<TimelineKey, TimelineState>,
  }),
  getters: {
    get: (s) => (key: TimelineKey) => s.timelines[key] ?? empty(),
    hasMore: (s) => (key: TimelineKey) => s.timelines[key]?.next !== undefined,
    pendingCount: (s) => (key: TimelineKey) =>
      s.timelines[key]?.pending.length ?? 0,
  },
  actions: {
    ensure(key: TimelineKey): TimelineState {
      if (!this.timelines[key]) this.timelines[key] = empty();
      return this.timelines[key];
    },

    async load(key: TimelineKey, query?: TimelineQuery) {
      const tl = this.ensure(key);
      if (tl.loading) return;
      tl.loading = true;
      tl.error = null;
      try {
        const page: Page<Status> = await fetchPage(key, query);
        tl.items = dedupe(page.items);
        tl.next = nextQuery(page.next);
        tl.loaded = true;
        tl.pending = [];
      } catch (e) {
        tl.error = e instanceof Error ? e.message : String(e);
      } finally {
        tl.loading = false;
      }
    },

    async loadMore(key: TimelineKey) {
      const tl = this.ensure(key);
      if (tl.loading || tl.next === undefined) return;
      tl.loading = true;
      tl.error = null;
      try {
        const page = await fetchPage(key, tl.next);
        tl.items = dedupe([...tl.items, ...page.items]);
        tl.next = nextQuery(page.next);
      } catch (e) {
        tl.error = e instanceof Error ? e.message : String(e);
      } finally {
        tl.loading = false;
      }
    },

    /** Insert at the top (own post, flushed live item). */
    prepend(key: TimelineKey, status: Status) {
      const tl = this.ensure(key);
      tl.items = dedupe([status, ...tl.items]);
    },

    /**
     * Buffer a live event; the visible list is untouched. An event
     * for an already-visible or already-buffered status — including
     * the original inside a displayed boost wrapper — is an edit:
     * replace it in place rather than dropping it.
     */
    buffer(key: TimelineKey, status: Status) {
      const tl = this.ensure(key);
      if (tl.items.some((s) => statusMatchesId(s, status.id))) {
        tl.items = patchStatusList(tl.items, status);
        return;
      }
      if (tl.pending.some((s) => statusMatchesId(s, status.id))) {
        tl.pending = patchStatusList(tl.pending, status);
        return;
      }
      tl.pending.push(status);
    },

    /** Fold buffered live items into the visible list. */
    flushPending(key: TimelineKey) {
      const tl = this.ensure(key);
      if (tl.pending.length === 0) return;
      tl.items = dedupe([...[...tl.pending].reverse(), ...tl.items]);
      tl.pending = [];
    },

    /**
     * Drop a status everywhere it is displayed — the row itself and
     * every boost wrapper pointing at it (the wrapper's `reblog`
     * renders null once the original is gone).
     */
    remove(id: string) {
      for (const tl of Object.values(this.timelines)) {
        tl.items = dropFromStatusList(tl.items, id);
        tl.pending = dropFromStatusList(tl.pending, id);
      }
    },

    /**
     * Replace a status everywhere it is displayed — after an own edit
     * the returned entity lands in every timeline that carries it
     * (home, public, hashtag, list), not just the one it was edited
     * from. Live `update`/`status.update` events reach the same
     * slots; a match inside a boost wrapper refreshes the wrapper's
     * `reblog` in place.
     */
    replace(status: Status) {
      for (const tl of Object.values(this.timelines)) {
        tl.items = patchStatusList(tl.items, status);
        tl.pending = patchStatusList(tl.pending, status);
      }
    },
  },
});
