import { defineStore } from "pinia";
import type { Status, StatusVisibility } from "@/api/entities";

const DRAFT_KEY = "yonder.compose.draft";
const STASH_KEY = "yonder.compose.stashed";

export interface PollDraft {
  options: string[];
  expiresIn: number;
  multiple: boolean;
  hideTotals: boolean;
}

export interface ComposeDraft {
  text: string;
  contentType: string;
  visibility: StatusVisibility;
  language: string | null;
  sensitive: boolean;
  spoilerText: string;
  cwEnabled: boolean;
  /** Uploaded attachment ids, in order. */
  mediaIds: string[];
  poll: PollDraft | null;
  inReplyToId: string | null;
  /** Acct/name shown on the reply banner. */
  replyToAcct: string | null;
  quoteId: string | null;
  quotedAcct: string | null;
  scheduledAt: string | null;
}

function emptyDraft(): ComposeDraft {
  return {
    text: "",
    contentType: "text/plain",
    visibility: "public",
    language: null,
    sensitive: false,
    spoilerText: "",
    cwEnabled: false,
    mediaIds: [],
    poll: null,
    inReplyToId: null,
    replyToAcct: null,
    quoteId: null,
    quotedAcct: null,
    scheduledAt: null,
  };
}

function loadDraft(): ComposeDraft {
  try {
    const raw = localStorage.getItem(DRAFT_KEY);
    if (!raw) return emptyDraft();
    const parsed = JSON.parse(raw) as Partial<ComposeDraft>;
    // Merge over the defaults so fields added later can't be undefined.
    return { ...emptyDraft(), ...parsed };
  } catch {
    return emptyDraft();
  }
}

/**
 * The compose draft — persisted to localStorage so a reload (or an
 * auth round-trip in epic 25) doesn't lose it. Reply/quote/DM context
 * is set by `open*` actions and shown as a banner in ComposeBox.
 */
export const useComposeStore = defineStore("compose", {
  state: () => ({
    draft: loadDraft() as ComposeDraft,
    /** Bumped to force the compose box open (reply from a card). */
    openSignal: 0,
  }),
  getters: {
    /** Anything worth keeping — reply/quote/DM openers confirm first. */
    isDirty: (s) =>
      Boolean(
        s.draft.text.trim() ||
        s.draft.spoilerText.trim() ||
        s.draft.mediaIds.length ||
        s.draft.poll,
      ),
  },
  actions: {
    persist() {
      localStorage.setItem(DRAFT_KEY, JSON.stringify(this.draft));
    },

    update(patch: Partial<ComposeDraft>) {
      Object.assign(this.draft, patch);
      this.persist();
    },

    /** Reply — the server carries over the parent's mentions; the
     * client only seeds the `@author` prefix for context. */
    openReply(status: Status) {
      const target = status.reblog ?? status;
      const acct = target.account.acct;
      this.reset();
      this.update({
        inReplyToId: target.id,
        replyToAcct: acct,
        text: `@${acct} `,
      });
      this.openSignal++;
    },

    openQuote(status: Status) {
      const target = status.reblog ?? status;
      this.reset();
      this.update({
        quoteId: target.id,
        quotedAcct: target.account.acct,
      });
      this.openSignal++;
    },

    /** Mention — a fresh public draft seeded `@acct ` (not a DM;
     * that's `openDirect`). */
    openMention(acct: string) {
      this.reset();
      this.update({ text: `@${acct} ` });
      this.openSignal++;
    },

    /** Direct-message mode; seeds `@a @b ` for one or more recipients. */
    openDirect(acct?: string | string[]) {
      const accts = Array.isArray(acct) ? acct : acct ? [acct] : [];
      this.reset();
      this.update({
        visibility: "direct",
        text: accts.length ? `${accts.map((a) => `@${a}`).join(" ")} ` : "",
      });
      this.openSignal++;
    },

    /**
     * Reply inside a DM thread — `openReply` seeding plus
     * `visibility: "direct"` (openReply alone would post a public
     * "DM"). The server's mention carry-over keeps the participant
     * set — and therefore the conversation key — stable.
     */
    openDirectReply(status: Status) {
      const target = status.reblog ?? status;
      const acct = target.account.acct;
      this.reset();
      this.update({
        inReplyToId: target.id,
        replyToAcct: acct,
        text: `@${acct} `,
        visibility: "direct",
      });
      this.openSignal++;
    },

    /** Drop the persisted draft + stash (explicit logout / account deletion). */
    clearPersisted() {
      localStorage.removeItem(DRAFT_KEY);
      localStorage.removeItem(STASH_KEY);
    },

    reset() {
      this.draft = emptyDraft();
      this.persist();
    },

    /**
     * Park the current draft (`?edit=` replaces it) so a successful
     * edit can hand it back — `unstash` restores and clears.
     */
    stash() {
      localStorage.setItem(STASH_KEY, JSON.stringify(this.draft));
    },
    unstash() {
      const raw = localStorage.getItem(STASH_KEY);
      if (!raw) return;
      localStorage.removeItem(STASH_KEY);
      try {
        this.draft = {
          ...emptyDraft(),
          ...(JSON.parse(raw) as Partial<ComposeDraft>),
        };
        this.persist();
      } catch {
        /* corrupt stash — drop it */
      }
    },
  },
});
