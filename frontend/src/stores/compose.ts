import { defineStore } from "pinia";
import type { Status, StatusVisibility } from "@/api/entities";
import { useAuthStore } from "./auth";

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
  /** Text an `open*` action seeded (mention/recipient line). When
   * `text` still equals it, the draft carries only context — not
   * user input — and a new context may replace it freely. */
  seededText: string;
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
    seededText: "",
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
 *
 * `replyTargetId`/`quoteTarget` coordinate the *embedded* composers —
 * the inline reply form unwrapped under a status card and the quote
 * modal. They're ephemeral on purpose: at most one embedded composer
 * is open at a time, and a reload never re-opens one.
 */
export const useComposeStore = defineStore("compose", {
  state: () => ({
    draft: loadDraft() as ComposeDraft,
    /** Bumped to force the compose box open (reply from a card). */
    openSignal: 0,
    /** Id of the status whose card shows the inline reply form. */
    replyTargetId: null as string | null,
    /** Status displayed (as a card) inside the quote compose modal. */
    quoteTarget: null as Status | null,
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
    /** The draft still holds only what an `open*` action seeded —
     *  context, not user input — so a new context can replace it
     *  without asking. */
    isUntouchedSeed: (s) =>
      Boolean(
        s.draft.seededText &&
        s.draft.text === s.draft.seededText &&
        !s.draft.spoilerText.trim() &&
        !s.draft.cwEnabled &&
        !s.draft.mediaIds.length &&
        !s.draft.poll,
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
     * client only seeds the `@author` prefix for context. The form
     * unwraps inline under the card (`replyTargetId`). */
    openReply(status: Status) {
      const target = status.reblog ?? status;
      const acct = target.account.acct;
      this.reset();
      this.update({
        inReplyToId: target.id,
        replyToAcct: acct,
        text: `@${acct} `,
        seededText: `@${acct} `,
      });
      this.replyTargetId = target.id;
      this.openSignal++;
    },

    /** Quote — the composer opens in a modal showing the quoted
     * card (`quoteTarget`). */
    openQuote(status: Status) {
      const target = status.reblog ?? status;
      this.reset();
      this.update({
        quoteId: target.id,
        quotedAcct: target.account.acct,
      });
      this.quoteTarget = target;
      this.openSignal++;
    },

    /** Mention — a fresh public draft seeded `@acct ` (not a DM;
     * that's `openDirect`). */
    openMention(acct: string) {
      this.reset();
      this.update({ text: `@${acct} `, seededText: `@${acct} ` });
      this.openSignal++;
    },

    /** Direct-message mode; seeds `@a @b ` for one or more recipients. */
    openDirect(acct?: string | string[]) {
      const accts = Array.isArray(acct) ? acct : acct ? [acct] : [];
      const seed = accts.length
        ? `${accts.map((a) => `@${a}`).join(" ")} `
        : "";
      this.reset();
      this.update({
        visibility: "direct",
        text: seed,
        seededText: seed,
      });
      this.openSignal++;
    },

    /**
     * Reply inside a DM thread — `visibility: "direct"` (openReply
     * alone would post a public "DM") and the full participant line
     * of the parent message: its author plus every mentioned account,
     * minus the viewer and duplicates. The server's mention
     * carry-over keeps the conversation key stable; the seed makes
     * the recipients visible to the author.
     */
    openDirectReply(status: Status) {
      const target = status.reblog ?? status;
      const self = useAuthStore().account?.acct;
      const seen = new Set<string>();
      const recipients = [target.account, ...(target.mentions ?? [])]
        .map((a) => a.acct)
        .filter((acct) => {
          if (acct === self || seen.has(acct)) return false;
          seen.add(acct);
          return true;
        });
      const seed = recipients.length
        ? `${recipients.map((a) => `@${a}`).join(" ")} `
        : "";
      this.reset();
      this.update({
        inReplyToId: target.id,
        replyToAcct: target.account.acct,
        text: seed,
        seededText: seed,
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
      // Posting (or cancelling) also dismisses any embedded composer.
      this.closeEmbed();
    },

    /** Close the inline reply form / quote modal without touching the
     * draft — the text survives for the `/compose` page or a reopen. */
    closeEmbed() {
      this.replyTargetId = null;
      this.quoteTarget = null;
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
