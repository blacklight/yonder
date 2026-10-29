import type { Status } from "@/api/entities";

/**
 * Helpers for applying live stream entities (`update`,
 * `status.update`, `delete`) to already-displayed status lists.
 *
 * Stream payloads render unpersonalized (`viewer: null`): a wholesale
 * replace would wipe the viewer's interaction flags, so every replace
 * keeps them from the displayed copy — the same merge the thread page
 * has always done.
 */

/** Carry the viewer's own interaction state onto a streamed entity. */
export function mergeViewerFlags(next: Status, prev: Status): Status {
  const poll =
    next.poll && prev.poll
      ? {
          ...next.poll,
          voted: prev.poll.voted,
          own_votes: prev.poll.own_votes,
        }
      : next.poll;
  const prevReactions = new Map(
    (prev.pleroma?.emoji_reactions ?? []).map((r) => [r.name, r]),
  );
  const pleroma = next.pleroma?.emoji_reactions
    ? {
        ...next.pleroma,
        emoji_reactions: next.pleroma.emoji_reactions.map((r) => ({
          ...r,
          me: prevReactions.get(r.name)?.me ?? r.me,
        })),
      }
    : next.pleroma;
  return {
    ...next,
    favourited: prev.favourited,
    reblogged: prev.reblogged,
    bookmarked: prev.bookmarked,
    pinned: prev.pinned,
    muted: prev.muted,
    poll,
    pleroma,
  };
}

/** `id` names this status or the original inside a boost wrapper. */
export function statusMatchesId(s: Status, id: string): boolean {
  return s.id === id || s.reblog?.id === id;
}

/**
 * Replace `s` with `next` when they are the same status — or when
 * `next` is the original inside `s`'s boost wrapper (the wrapper row
 * stays; only its `reblog` refreshes).
 */
export function patchStatus(s: Status, next: Status): Status {
  if (s.id === next.id) return mergeViewerFlags(next, s);
  if (s.reblog?.id === next.id)
    return { ...s, reblog: mergeViewerFlags(next, s.reblog) };
  return s;
}

/** `patchStatus` over a list — non-matching entries pass through. */
export function patchStatusList(list: Status[], next: Status): Status[] {
  return list.map((s) => patchStatus(s, next));
}

/** Drop a status and every boost wrapper pointing at it. */
export function dropFromStatusList(list: Status[], id: string): Status[] {
  return list.filter((s) => !statusMatchesId(s, id));
}
