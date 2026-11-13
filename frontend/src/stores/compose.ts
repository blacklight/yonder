import { defineStore } from "pinia";
import type { Status, StatusVisibility } from "@/api/entities";
import { storageGet, storageRemove, storageSet } from "@/utils/storage";
import { useAuthStore } from "./auth";
import { useSettingsStore } from "./settings";

const DRAFT_KEY = "yonder.compose.draft";
const STASH_KEY = "yonder.compose.stashed";
const DRAFTS_KEY = "yonder.compose.drafts";
const DRAFTS_MAX = 50;

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
  /** `note`/`article` — the AP object type (FEP-b2b8 long-form). */
  postType: string;
  /** Article headline (AS2 `name`) — only sent for `article`. */
  title: string;
  /** Article teaser (AS2 `summary`), plain text. */
  summary: string;
  /** Representative image attachment id (AS2 `image`). */
  coverMediaId: string | null;
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
  /** Saved-draft entry this working draft was loaded from. */
  refId: string | null;
}

/** A named draft kept in the drafts list. */
export interface DraftEntry extends ComposeDraft {
  id: string;
  /** ISO timestamp of the last save. */
  updatedAt: string;
}

function emptyDraft(): ComposeDraft {
  return {
    text: "",
    seededText: "",
    contentType: "text/plain",
    postType: "note",
    title: "",
    summary: "",
    coverMediaId: null,
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
    refId: null,
  };
}

function loadDraft(): ComposeDraft {
  try {
    const raw = storageGet(DRAFT_KEY);
    if (!raw) return emptyDraft();
    const parsed = JSON.parse(raw) as Partial<ComposeDraft>;
    // Merge over the defaults so fields added later can't be undefined.
    return { ...emptyDraft(), ...parsed };
  } catch {
    return emptyDraft();
  }
}

/** Author + mentioned participants of `status`, deduped and minus
 *  the viewer — the seeded `@… @… ` recipient line a reply opens
 *  with (Mastodon's reply prefill). */
function replySeed(target: Status, self: string | undefined): string {
  const seen = new Set<string>();
  const recipients = [target.account, ...(target.mentions ?? [])]
    .map((a) => a.acct)
    .filter((acct) => {
      if (acct === self || seen.has(acct)) return false;
      seen.add(acct);
      return true;
    });
  return recipients.length
    ? `${recipients.map((a) => `@${a}`).join(" ")} `
    : "";
}

function loadDrafts(): DraftEntry[] {
  try {
    const raw = storageGet(DRAFTS_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter((e): e is DraftEntry => !!e && typeof e.id === "string")
      .map((e) => ({ ...emptyDraft(), ...e }));
  } catch {
    return [];
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
    /** Explicitly-saved drafts — the dedicated drafts list on /compose. */
    drafts: loadDrafts() as DraftEntry[],
    /**
     * Session flag: `true` while the draft's context was set by an
     * `open*` action (or a deliberate draft load) *in this session*.
     * Not persisted — after a reload a draft's reply/quote/DM context
     * is stale, and a fresh compose surface strips it (`clearContext`)
     * instead of letting a submission mis-target another thread.
     */
    contextIsLive: false,
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
        s.draft.title.trim() ||
        s.draft.summary.trim() ||
        s.draft.coverMediaId !== null ||
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
    /** The draft carries reply/quote/DM targeting context. */
    hasContext: (s) =>
      Boolean(
        s.draft.inReplyToId ||
        s.draft.quoteId ||
        s.draft.seededText ||
        s.draft.visibility === "direct",
      ),
  },
  actions: {
    persist() {
      storageSet(DRAFT_KEY, JSON.stringify(this.draft));
    },

    persistDrafts() {
      storageSet(DRAFTS_KEY, JSON.stringify(this.drafts));
    },

    update(patch: Partial<ComposeDraft>) {
      Object.assign(this.draft, patch);
      this.persist();
    },

    /**
     * Strip reply/quote/DM targeting — a fresh compose window must
     * never inherit another conversation's `in_reply_to_id`. Text the
     * user actually typed survives; a seed-only shell collapses
     * fully (seeded mention line, direct visibility).
     */
    clearContext() {
      const d = this.draft;
      const seedOnly = Boolean(d.seededText) && d.text === d.seededText;
      this.update({
        inReplyToId: null,
        replyToAcct: null,
        quoteId: null,
        quotedAcct: null,
        seededText: "",
        text: seedOnly ? "" : d.text,
        visibility:
          d.visibility === "direct" && seedOnly ? "public" : d.visibility,
      });
      this.contextIsLive = false;
    },

    /** Reply — seeds the full participant line (`@author` plus the
     * parent's mentions, minus the viewer), the same set the server
     * carries into the reply's mentions; seeding them keeps the
     * recipients visible and editable. The form unwraps inline under
     * the card (`replyTargetId`). */
    openReply(status: Status) {
      const target = status.reblog ?? status;
      const seed = replySeed(target, useAuthStore().account?.acct);
      this.reset();
      this.update({
        inReplyToId: target.id,
        replyToAcct: target.account.acct,
        text: seed,
        seededText: seed,
      });
      this.replyTargetId = target.id;
      this.contextIsLive = true;
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
      this.contextIsLive = true;
      this.openSignal++;
    },

    /** Mention — a fresh public draft seeded `@acct ` (not a DM;
     * that's `openDirect`). */
    openMention(acct: string) {
      this.reset();
      this.update({ text: `@${acct} `, seededText: `@${acct} ` });
      this.contextIsLive = true;
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
      this.contextIsLive = true;
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
      const seed = replySeed(target, useAuthStore().account?.acct);
      this.reset();
      this.update({
        inReplyToId: target.id,
        replyToAcct: target.account.acct,
        text: seed,
        seededText: seed,
        visibility: "direct",
      });
      this.contextIsLive = true;
      this.openSignal++;
    },

    /** Drop the persisted draft + stash + drafts (explicit logout /
     * account deletion). */
    clearPersisted() {
      storageRemove(DRAFT_KEY);
      storageRemove(STASH_KEY);
      storageRemove(DRAFTS_KEY);
      this.drafts = [];
    },

    reset() {
      this.draft = emptyDraft();
      this.contextIsLive = false;
      this.persist();
      this.applyContentTypeDefault();
      this.applyPostTypeDefault();
      // Posting (or cancelling) also dismisses any embedded composer.
      this.closeEmbed();
    },

    /**
     * Apply the account's default content type to a fresh draft. The
     * "text/plain" sentinel means "never chosen" and a seed-only draft
     * (`text === seededText`) holds no user input yet, so both take
     * `effective_content_type`; a draft with real text or an explicit
     * non-default pick is left alone. No-op until settings have
     * loaded — ComposeBox watches for them landing and re-applies.
     */
    applyContentTypeDefault() {
      const effective = useSettingsStore().settings?.effective_content_type;
      const d = this.draft;
      if (
        typeof effective === "string" &&
        d.contentType === "text/plain" &&
        (!d.text || d.text === d.seededText)
      ) {
        this.update({ contentType: effective });
      }
    },

    /**
     * Apply the account's default post type to a fresh draft — same
     * "untouched shell" rule as `applyContentTypeDefault`: only while
     * the draft still says `note`, holds no user-typed text and no
     * article fields. `?edit=` and explicit picks are left alone.
     */
    applyPostTypeDefault() {
      const effective = useSettingsStore().settings?.effective_post_type;
      const d = this.draft;
      if (
        typeof effective === "string" &&
        effective !== "note" &&
        d.postType === "note" &&
        (!d.text || d.text === d.seededText) &&
        !d.title &&
        !d.summary &&
        d.coverMediaId === null
      ) {
        this.update({ postType: effective });
      }
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
      storageSet(STASH_KEY, JSON.stringify(this.draft));
    },
    unstash() {
      const raw = storageGet(STASH_KEY);
      if (!raw) return;
      storageRemove(STASH_KEY);
      try {
        this.draft = {
          ...emptyDraft(),
          ...(JSON.parse(raw) as Partial<ComposeDraft>),
        };
        this.persist();
        this.contextIsLive = this.hasContext;
      } catch {
        /* corrupt stash — drop it */
      }
    },

    /** Save the current draft into the drafts list — updates in place
     * when it was loaded from an entry, appends otherwise. */
    saveAsDraft(): string | null {
      if (!this.isDirty && !this.hasContext) return null;
      const id = this.draft.refId ?? crypto.randomUUID();
      const entry: DraftEntry = {
        ...this.draft,
        refId: id,
        id,
        updatedAt: new Date().toISOString(),
      };
      const i = this.drafts.findIndex((e) => e.id === id);
      if (i !== -1) this.drafts[i] = entry;
      else this.drafts.unshift(entry);
      if (this.drafts.length > DRAFTS_MAX) this.drafts.length = DRAFTS_MAX;
      this.persistDrafts();
      this.update({ refId: id });
      return id;
    },

    /** Load a saved draft into the composer — a deliberate action,
     *  so its context is live even though it came from storage. */
    loadDraftEntry(id: string) {
      const entry = this.drafts.find((e) => e.id === id);
      if (!entry) return;
      const fields: Partial<DraftEntry> = { ...entry };
      delete fields.id;
      delete fields.updatedAt;
      this.draft = { ...emptyDraft(), ...fields, refId: id };
      this.persist();
      this.contextIsLive = this.hasContext;
      this.openSignal++;
    },

    /** Delete a saved-draft entry — the explicit drafts-list action. */
    removeDraft(id: string) {
      this.drafts = this.drafts.filter((e) => e.id !== id);
      this.persistDrafts();
      if (this.draft.refId === id) this.update({ refId: null });
    },
  },
});
