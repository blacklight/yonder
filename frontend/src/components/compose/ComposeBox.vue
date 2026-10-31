<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from "vue";
import { useRoute } from "vue-router";
import { useI18n } from "vue-i18n";
import Button from "@/components/ui/Button.vue";
import PollBuilder from "./PollBuilder.vue";
import EmojiPicker from "./EmojiPicker.vue";
import MentionAutocomplete from "./MentionAutocomplete.vue";
import { FontAwesomeIcon } from "@/icons";
import {
  createStatus,
  editStatus,
  getStatus,
  mediaProcessing,
  uploadMedia,
  updateMedia,
} from "@/api/statuses";
import { ApiError } from "@/api/errors";
import { composeLimits } from "@/api/instance";
import { getInstance } from "@/api/instance";
import { search } from "@/api/search";
import { useComposeStore } from "@/stores/compose";
import { useSettingsStore } from "@/stores/settings";
import { useTimelineStore } from "@/stores/timeline";
import type { EmojiSelection } from "@/utils/unicodeEmoji";
import type {
  Account,
  MediaAttachment,
  ScheduledStatus,
  Status,
  StatusVisibility,
} from "@/api/entities";

const props = defineProps<{
  /** Composer embedded in a DM thread — the visibility is pinned to
   * `direct` and the visibility picker is never shown, so a reply
   * can't leak out of the private conversation. */
  direct?: boolean;
  /** The standalone `/compose` surface — a *new* compose window.
   *  A draft's reply/quote/DM context is only live while the session
   *  that opened it is running; a stale persisted context is stripped
   *  on mount so a submission can't mis-target another thread. */
  standalone?: boolean;
}>();

const { t, locale } = useI18n();
const route = useRoute();
const compose = useComposeStore();
const settings = useSettingsStore();
const timelines = useTimelineStore();

const limits = ref(composeLimits(null));
const media = ref<MediaAttachment[]>([]);
const uploading = ref(false);
const posting = ref(false);
const error = ref("");
const done = ref("");
const emojiOpen = ref(false);
const scheduleOpen = ref(false);
const editId = ref<string | null>(null);
const textarea = ref<HTMLTextAreaElement | null>(null);

// One key per *submission body* — an unchanged retry reuses it so a
// repeat POST replays; editing the draft mints a fresh key so a
// committed-but-timed-out request can't swallow the new text.
const idemKey = ref(crypto.randomUUID());
let idemBody = "";

const d = computed(() => compose.draft);
// "Private conversation" — either the host pinned it (thread page)
// or the draft itself is a DM. Either way the visibility picker is
// replaced by a static label: there is no public side to flip to.
const isDirect = computed(
  () => props.direct || d.value.visibility === "direct",
);
const maxChars = computed(() => limits.value.maxCharacters);
const overLimit = computed(() => charLen.value > maxChars.value);
const charLen = computed(() => d.value.text.length);
const pollInvalid = computed(
  () =>
    d.value.poll !== null &&
    d.value.poll.options.filter((o) => o.trim()).length < 2,
);
// `min` only constrains the picker widget — a typed past value still
// reaches us. Block submit instead of relying on the server's 422.
function scheduledIsPast() {
  return (
    d.value.scheduledAt !== null &&
    new Date(d.value.scheduledAt).getTime() <= Date.now()
  );
}
const schedulePast = computed(scheduledIsPast);
const canPost = computed(
  () =>
    !posting.value &&
    !overLimit.value &&
    !pollInvalid.value &&
    !schedulePast.value &&
    !uploading.value &&
    (d.value.text.trim().length > 0 || media.value.length > 0),
);

const banner = computed(() => {
  if (editId.value) return t("compose.editing");
  if (d.value.inReplyToId)
    return t("compose.replyBanner", { user: d.value.replyToAcct });
  if (d.value.quoteId)
    return t("compose.quoteBanner", { user: d.value.quotedAcct });
  if (d.value.visibility === "direct" && !d.value.inReplyToId)
    return t("compose.directBanner");
  return null;
});

const LANGUAGES = [
  { code: "", label: "compose.detect" },
  { code: "en", label: "English" },
  { code: "ar", label: "العربية" },
  { code: "bg", label: "Български" },
  { code: "ca", label: "Catalan" },
  { code: "cn", label: "简体中文" },
  { code: "cs", label: "Czech" },
  { code: "da", label: "Dansk" },
  { code: "de", label: "Deutsch" },
  { code: "el", label: "Ελληνικα" },
  { code: "es", label: "Español" },
  { code: "fa", label: "فارسی" },
  { code: "fi", label: "Suomi" },
  { code: "fr", label: "Français" },
  { code: "he", label: "עברית" },
  { code: "hi", label: "हिन्दी" },
  { code: "hr", label: "Hrvatski" },
  { code: "hu", label: "Magyar" },
  { code: "id", label: "Bahasa Indonesia" },
  { code: "is", label: "Icelandic" },
  { code: "it", label: "Italiano" },
  { code: "ja", label: "日本語" },
  { code: "jv", label: "Basa Jawa" },
  { code: "ka", label: "ქართული" },
  { code: "km", label: "ភាសាខ្មែរ" },
  { code: "ko", label: "한국어" },
  { code: "lt", label: "Lietuvių" },
  { code: "lv", label: "Latviesu" },
  { code: "ms", label: "Bahasa Melayu" },
  { code: "nb", label: "Norsk" },
  { code: "nl", label: "Nederlands" },
  { code: "pl", label: "Polski" },
  { code: "pt", label: "Português" },
  { code: "ro", label: "Romenian" },
  { code: "ru", label: "Русский" },
  { code: "sr", label: "Srpski" },
  { code: "sl", label: "Slovenški" },
  { code: "sk", label: "Slovensky" },
  { code: "sq", label: "Shqip" },
  { code: "sv", label: "Svenska" },
  { code: "sw", label: "Swahili" },
  { code: "ta", label: "தமிழ்" },
  { code: "te", label: "తెలుగు" },
  { code: "th", label: "ไทย" },
  { code: "tl", label: "Tagalog" },
  { code: "tr", label: "Türkçe" },
  { code: "tw", label: "中文(繁體)" },
  { code: "uk", label: "Українська" },
  { code: "ur", label: "اردو" },
  { code: "uz", label: "O'zbekcha" },
  { code: "vi", label: "Tiếng Việt" },
  { code: "zh", label: "中文" },
];
const VISIBILITIES: StatusVisibility[] = [
  "public",
  "unlisted",
  "private",
  "direct",
];
const CONTENT_TYPES = ["text/plain", "text/markdown"];

onMounted(async () => {
  // Embedded composers (inline reply, quote modal) mount *after* the
  // open signal fired, so the watcher below can't focus them. After
  // nextTick the parent's mount hooks (e.g. Modal's focus) have run.
  await nextTick();
  textarea.value?.focus();
  try {
    limits.value = composeLimits(await getInstance());
  } catch {
    /* defaults */
  }
  // `?edit=<id>` reopens the draft of an own post for `PUT /statuses`.
  const edit = route.query.edit;
  if (typeof edit === "string" && edit) {
    try {
      const s = await getStatus(edit);
      editId.value = edit;
      media.value = s.media_attachments;
      // A dirty draft is parked, not destroyed — restored after the
      // edit is submitted (see `unstash` in the reset path below).
      if (compose.isDirty) compose.stash();
      compose.update({
        text: s.yonder?.source ?? s.content.replace(/<[^>]+>/g, ""),
        contentType: s.yonder?.content_type ?? "text/plain",
        visibility: s.visibility,
        language: s.language,
        sensitive: s.sensitive,
        cwEnabled: Boolean(s.spoiler_text),
        spoilerText: s.spoiler_text,
        mediaIds: s.media_attachments.map((m) => m.id),
        // An edit never carries reply/quote context.
        inReplyToId: null,
        replyToAcct: null,
        quoteId: null,
        quotedAcct: null,
        seededText: "",
        refId: null,
      });
      return;
    } catch (e) {
      error.value = e instanceof ApiError ? e.message : String(e);
    }
  }
  // A fresh standalone window drops stale reply/quote/DM context
  // (persisted from a previous session — no live opener owns it).
  // Typed text, media and settings survive; only the targeting goes.
  if (props.standalone && compose.hasContext && !compose.contextIsLive) {
    compose.clearContext();
  }
  // A persisted draft keeps attachment ids across reloads — rehydrate
  // them via the owner-only media endpoint so the previews come back.
  // A definitive "gone" (404/403) drops the id; any other failure is
  // transient, so the id survives as a placeholder tile (no preview,
  // still submitted) instead of being lost to a network blip.
  if (d.value.mediaIds.length && media.value.length === 0) {
    const found = await Promise.all(
      d.value.mediaIds.map((id) =>
        mediaProcessing(id).catch((e: unknown) =>
          e instanceof ApiError && (e.status === 404 || e.status === 403)
            ? null
            : ({ id } as MediaAttachment),
        ),
      ),
    );
    media.value = found.filter((m): m is MediaAttachment => m !== null);
    if (media.value.length !== d.value.mediaIds.length) syncMediaIds();
  }
  // New drafts pick up the account defaults from settings. A seeded
  // visibility (`direct` from a DM shortcut, or a pinned composer)
  // is never overwritten — an untouched draft is the only one that
  // takes the account default.
  const s = settings.settings as Record<string, unknown> | null;
  if (s && !props.direct && !d.value.text && !d.value.inReplyToId) {
    compose.update({
      visibility:
        d.value.visibility === "public"
          ? ((s.default_visibility as StatusVisibility) ?? "public")
          : d.value.visibility,
      language:
        (s.effective_posting_language as string | null) ??
        (s.default_posting_language as string | null) ??
        locale.value,
      sensitive: Boolean(s.default_sensitive),
    });
  }
});

// Compose opens from card actions via openSignal.
watch(
  () => compose.openSignal,
  () => {
    media.value = [];
    editId.value = null;
    scheduleOpen.value = false;
    idemKey.value = crypto.randomUUID();
    textarea.value?.focus();
  },
);

async function onFiles(e: Event) {
  const files = (e.target as HTMLInputElement).files;
  if (!files) return;
  // Media and poll are mutually exclusive server-side — pick media.
  if (d.value.poll) compose.update({ poll: null });
  uploading.value = true;
  error.value = "";
  try {
    for (const f of Array.from(files)) {
      if (media.value.length >= limits.value.maxMediaAttachments) {
        error.value = t("compose.mediaLimit", {
          n: limits.value.maxMediaAttachments,
        });
        break;
      }
      media.value.push(await uploadMedia(f));
    }
    syncMediaIds();
  } catch (err) {
    error.value = err instanceof ApiError ? err.message : String(err);
  } finally {
    uploading.value = false;
    (e.target as HTMLInputElement).value = "";
  }
}

function syncMediaIds() {
  compose.update({ mediaIds: media.value.map((m) => m.id) });
}

async function setAlt(m: MediaAttachment, description: string) {
  m.description = description;
  await updateMedia(m.id, { description });
}

function removeMedia(m: MediaAttachment) {
  media.value = media.value.filter((x) => x.id !== m.id);
  syncMediaIds();
}

/** `datetime-local` values are local time — the minimum must be too
 * (a bare `toISOString()` is UTC and lets past times through east of
 * Greenwich while blocking valid ones west of it). */
function minLocal() {
  const now = new Date();
  return new Date(now.getTime() - now.getTimezoneOffset() * 60000)
    .toISOString()
    .slice(0, 16);
}

// The picker stays open while a time is set — hiding it would make a
// pending schedule invisible. Toggling off clears the time instead.
const scheduleVisible = computed(
  () => scheduleOpen.value || d.value.scheduledAt !== null,
);
function toggleSchedule() {
  if (scheduleVisible.value) {
    scheduleOpen.value = false;
    if (d.value.scheduledAt) compose.update({ scheduledAt: null });
  } else {
    scheduleOpen.value = true;
  }
}

function togglePoll() {
  if (d.value.poll) {
    compose.update({ poll: null });
  } else {
    // Poll clears media (mutual exclusion).
    media.value = [];
    syncMediaIds();
    compose.update({
      poll: {
        options: ["", ""],
        expiresIn: limits.value.poll.minExpiration >= 86400 ? 86400 : 3600,
        multiple: false,
        hideTotals: false,
      },
    });
  }
}

// Unicode picks insert the literal character; custom emoji insert
// their `:shortcode:` (resolved to an image at render time).
function insertEmoji(emoji: EmojiSelection) {
  const el = textarea.value;
  const insert = emoji.kind === "unicode" ? emoji.value : `:${emoji.value}:`;
  const start = el?.selectionStart ?? d.value.text.length;
  const end = el?.selectionEnd ?? start;
  compose.update({
    text: d.value.text.slice(0, start) + insert + d.value.text.slice(end),
  });
  emojiOpen.value = false;
  void nextTick(() => {
    const pos = start + insert.length;
    el?.focus();
    el?.setSelectionRange(pos, pos);
  });
}

// --- @-mention autocomplete ----------------------------------------
// A `@word` (optionally `@word@domain`) at the caret queries the
// indexed accounts via the search endpoint's autocomplete mode
// (`type` + small `limit` ⇒ prefix matching server-side).
const MENTION_RE = /(?:^|[\s([{"'])@([A-Za-z0-9_.]+(?:@[A-Za-z0-9_.-]*)?)$/;
const mentionToken = ref<{ start: number; token: string } | null>(null);
const mentionResults = ref<Account[]>([]);
const mentionActive = ref(0);
// Escape suppresses the dropdown until the token changes — otherwise
// the next caret-scan (any keyup) would reopen it.
const mentionDismissed = ref("");
let mentionSeq = 0;
let mentionTimer: ReturnType<typeof setTimeout> | undefined;

function closeMention() {
  mentionToken.value = null;
  mentionResults.value = [];
  clearTimeout(mentionTimer);
}

/** Re-scan the text before the caret for an `@`-token; debounces the
 * search request. Called on input, click and keyup so caret moves
 * without edits still update or dismiss the list. */
function updateMention() {
  const el = textarea.value;
  if (!el) return;
  const caret = el.selectionStart ?? d.value.text.length;
  const m = d.value.text.slice(0, caret).match(MENTION_RE);
  if (!m || mentionDismissed.value === `${caret}:${m[1]}`) {
    if (!m) closeMention();
    return;
  }
  const token = m[1];
  const start = caret - token.length - 1;
  // An unchanged token (arrow keys, clicks inside it) keeps the
  // current list and highlight — no refetch.
  if (mentionToken.value?.start === start && mentionToken.value.token === token)
    return;
  mentionDismissed.value = "";
  mentionToken.value = { start, token };
  mentionActive.value = 0;
  const seq = ++mentionSeq;
  clearTimeout(mentionTimer);
  mentionTimer = setTimeout(async () => {
    try {
      const res = await search({ q: token, type: "accounts", limit: 6 });
      // A later keystroke made this response stale — drop it.
      if (seq === mentionSeq) mentionResults.value = res.accounts;
    } catch {
      if (seq === mentionSeq) mentionResults.value = [];
    }
  }, 200);
}

function pickMention(a: Account) {
  const tk = mentionToken.value;
  if (!tk) return;
  const insert = `@${a.acct} `;
  const end = tk.start + tk.token.length + 1;
  compose.update({
    text: d.value.text.slice(0, tk.start) + insert + d.value.text.slice(end),
  });
  closeMention();
  void nextTick(() => {
    const pos = tk.start + insert.length;
    textarea.value?.focus();
    textarea.value?.setSelectionRange(pos, pos);
  });
}

/** Textarea keydown: Ctrl/Cmd+Enter submits; while the mention list
 * is open, arrows/Tab/Enter/Escape are consumed for navigation.
 * Everything else (plain Enter → newline) passes through. */
function onTextKeydown(e: KeyboardEvent) {
  if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
    e.preventDefault();
    void submit();
    return;
  }
  if (!mentionResults.value.length) return;
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const n = mentionResults.value.length;
    mentionActive.value =
      (mentionActive.value + (e.key === "ArrowDown" ? 1 : -1) + n) % n;
  } else if ((e.key === "Tab" && !e.shiftKey) || e.key === "Enter") {
    e.preventDefault();
    const a = mentionResults.value[mentionActive.value];
    if (a) pickMention(a);
  } else if (e.key === "Escape") {
    e.preventDefault();
    const tk = mentionToken.value;
    if (tk)
      mentionDismissed.value = `${tk.start + tk.token.length + 1}:${tk.token}`;
    closeMention();
  }
}

async function submit() {
  if (!canPost.value) return;
  // `schedulePast` computes over non-reactive `Date.now()` — the
  // picked time may have passed while the page sat open. Re-check.
  if (scheduledIsPast()) {
    error.value = t("compose.schedulePast");
    return;
  }
  posting.value = true;
  error.value = "";
  try {
    const body = {
      status: d.value.text,
      content_type: d.value.contentType,
      // A thread composer can never leave the conversation — pin the
      // wire value even if the draft somehow drifted.
      visibility: props.direct ? "direct" : d.value.visibility,
      language: d.value.language || undefined,
      sensitive: d.value.sensitive || d.value.cwEnabled,
      spoiler_text: d.value.cwEnabled ? d.value.spoilerText : undefined,
      in_reply_to_id: editId.value
        ? undefined
        : (d.value.inReplyToId ?? undefined),
      media_ids: media.value.length ? media.value.map((m) => m.id) : undefined,
      poll: d.value.poll
        ? {
            options: d.value.poll.options.filter((o) => o.trim()),
            expires_in: d.value.poll.expiresIn,
            multiple: d.value.poll.multiple,
            hide_totals: d.value.poll.hideTotals,
          }
        : undefined,
      quote_id: d.value.quoteId ?? undefined,
      scheduled_at: d.value.scheduledAt ?? undefined,
    };
    const sig = JSON.stringify(body);
    if (sig !== idemBody) {
      idemBody = sig;
      idemKey.value = crypto.randomUUID();
    }
    let created: Status | ScheduledStatus;
    if (editId.value) {
      created = await editStatus(editId.value, body);
      // The edited row is replaced in place wherever it is displayed —
      // an edit must not reorder it to the top of a feed.
      timelines.replace(created as Status);
    } else {
      created = await createStatus(body, idemKey.value);
      if (!d.value.scheduledAt) {
        // `scheduled_at` was unset → the response is a `Status`.
        // Surface it immediately in every timeline it belongs to —
        // home (own posts) plus public/tag feeds for public posts.
        const s = created as Status;
        timelines.prepend("home", s);
        if (s.visibility === "public") {
          timelines.prepend("public", s);
          timelines.prepend("public:local", s);
          for (const key of Object.keys(timelines.timelines)) {
            if (
              key.startsWith("hashtag:") &&
              s.tags.some(
                (tag) => tag.name.toLowerCase() === key.slice(8).toLowerCase(),
              )
            ) {
              timelines.prepend(key, s);
            }
          }
        }
        done.value = t("compose.posted");
      } else {
        done.value = t("compose.scheduled");
      }
    }
    const wasEdit = editId.value !== null;
    // A posted draft leaves the drafts list — it's been sent.
    if (d.value.refId) compose.removeDraft(d.value.refId);
    compose.reset();
    media.value = [];
    editId.value = null;
    scheduleOpen.value = false;
    idemBody = "";
    idemKey.value = crypto.randomUUID();
    // Editing parked the previous draft — hand it back.
    if (wasEdit) compose.unstash();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    posting.value = false;
  }
}
</script>

<template>
  <div class="compose" data-testid="compose">
    <div class="compose__top">
      <div v-if="banner" class="compose__banner">
        <span>{{ banner }}</span>
        <button
          type="button"
          class="compose__banner-x"
          @click="compose.reset()"
        >
          {{ t("compose.cancelContext") }}
        </button>
      </div>

      <div v-if="d.cwEnabled" class="compose__cw">
        <input
          :value="d.spoilerText"
          class="compose__cw-input"
          :placeholder="t('compose.cwPlaceholder')"
          @input="
            compose.update({
              spoilerText: ($event.target as HTMLInputElement).value,
            })
          "
        />
      </div>

      <div class="compose__text-wrap">
        <textarea
          ref="textarea"
          :value="d.text"
          class="compose__text"
          :placeholder="t('compose.placeholder')"
          rows="5"
          data-testid="compose-text"
          role="combobox"
          :aria-expanded="mentionResults.length > 0"
          aria-controls="mention-ac"
          :aria-activedescendant="
            mentionResults.length
              ? `mention-opt-${mentionResults[mentionActive]?.id}`
              : undefined
          "
          @input="
            compose.update({
              text: ($event.target as HTMLTextAreaElement).value,
            });
            updateMention();
          "
          @keydown="onTextKeydown"
          @keyup="updateMention"
          @click="updateMention"
          @blur="closeMention"
        />
        <MentionAutocomplete
          v-if="mentionResults.length"
          :accounts="mentionResults"
          :active="mentionActive"
          :textarea="textarea"
          @select="pickMention"
        />
      </div>

      <div v-if="media.length" class="compose__media">
        <figure v-for="m in media" :key="m.id" class="compose__media-item">
          <img :src="m.preview_url ?? m.url" :alt="m.description ?? ''" />
          <input
            class="compose__alt"
            :value="m.description ?? ''"
            :placeholder="t('compose.altPlaceholder')"
            @change="setAlt(m, ($event.target as HTMLInputElement).value)"
          />
          <button
            type="button"
            class="compose__media-x"
            :aria-label="t('compose.mediaRemove')"
            @click="removeMedia(m)"
          >
            <FontAwesomeIcon icon="xmark" />
          </button>
        </figure>
      </div>

      <PollBuilder
        v-if="d.poll"
        :poll="d.poll"
        :max-options="limits.poll.maxOptions"
        :max-characters-per-option="limits.poll.maxCharactersPerOption"
        :min-expiration="limits.poll.minExpiration"
        :max-expiration="limits.poll.maxExpiration"
        @update="compose.update({ poll: $event })"
        @remove="compose.update({ poll: null })"
      />
    </div>

    <div class="compose__bottom">
      <div class="compose__toolbar">
        <label
          class="compose__ctl"
          :title="t('compose.addMedia')"
          :aria-label="t('compose.addMedia')"
        >
          <FontAwesomeIcon icon="image" />
          <input
            type="file"
            multiple
            accept="image/*,video/*,audio/*"
            class="compose__file-input"
            data-testid="media-input"
            @change="onFiles"
          />
        </label>
        <button
          type="button"
          class="compose__ctl"
          :class="{ 'compose__ctl--on': d.poll }"
          :title="t('compose.pollAdd')"
          :aria-label="t('compose.pollAdd')"
          :disabled="media.length > 0"
          data-testid="poll-toggle"
          @click="togglePoll"
        >
          <FontAwesomeIcon icon="square-poll-vertical" />
        </button>
        <span class="compose__emoji-wrap">
          <button
            type="button"
            class="compose__ctl"
            :class="{ 'compose__ctl--on': emojiOpen }"
            :title="t('compose.emoji')"
            :aria-label="t('compose.emoji')"
            @click="emojiOpen = !emojiOpen"
          >
            <FontAwesomeIcon :icon="['far', 'face-smile']" />
          </button>
          <EmojiPicker v-if="emojiOpen" @select="insertEmoji" />
        </span>
        <button
          type="button"
          class="compose__ctl compose__ctl--text"
          :class="{ 'compose__ctl--on': d.cwEnabled }"
          :aria-pressed="d.cwEnabled"
          :aria-label="t('compose.cw')"
          :title="t('compose.cw')"
          @click="
            compose.update({
              cwEnabled: !d.cwEnabled,
              spoilerText: d.spoilerText,
            })
          "
        >
          CW
        </button>
        <button
          type="button"
          class="compose__ctl"
          :class="{ 'compose__ctl--on': d.sensitive }"
          :aria-pressed="d.sensitive"
          :aria-label="t('compose.sensitive')"
          :title="t('compose.sensitive')"
          @click="compose.update({ sensitive: !d.sensitive })"
        >
          <FontAwesomeIcon icon="triangle-exclamation" />
        </button>
        <button
          type="button"
          class="compose__ctl"
          :class="{ 'compose__ctl--on': scheduleVisible }"
          :aria-pressed="scheduleVisible"
          :aria-label="t('compose.schedule')"
          :title="t('compose.schedule')"
          data-testid="schedule-toggle"
          @click="toggleSchedule"
        >
          <FontAwesomeIcon icon="clock" />
        </button>

        <span
          class="compose__counter"
          :class="{ 'compose__counter--over': overLimit }"
          data-testid="counter"
          >{{ charLen }} / {{ maxChars }}</span
        >
        <span v-if="overLimit" class="compose__over">{{
          t("compose.overLimit")
        }}</span>
        <Button
          type="button"
          :loading="posting"
          :disabled="!canPost"
          data-testid="post"
          @click="submit"
        >
          <FontAwesomeIcon icon="paper-plane" />
          {{ d.scheduledAt ? t("compose.schedule") : t("compose.post") }}
        </Button>
      </div>

      <div class="compose__options">
        <span v-if="isDirect" class="compose__private">
          <FontAwesomeIcon icon="lock" />
          {{ t("compose.vis_direct") }}
        </span>
        <span v-else class="compose__select">
          <select
            :value="d.visibility"
            :aria-label="t('compose.visibility')"
            :title="t('compose.visibility')"
            data-testid="visibility"
            @change="
              compose.update({
                visibility: ($event.target as HTMLSelectElement)
                  .value as StatusVisibility,
              })
            "
          >
            <option v-for="v in VISIBILITIES" :key="v" :value="v">
              {{ t(`compose.vis_${v}`) }}
            </option>
          </select>
          <FontAwesomeIcon icon="chevron-down" class="compose__select-caret" />
        </span>
        <span class="compose__select">
          <select
            :value="d.language ?? ''"
            :aria-label="t('compose.language')"
            :title="t('compose.language')"
            @change="
              compose.update({
                language: ($event.target as HTMLSelectElement).value || null,
              })
            "
          >
            <option v-for="l in LANGUAGES" :key="l.code" :value="l.code">
              {{ l.label.startsWith("compose.") ? t(l.label) : l.label }}
            </option>
          </select>
          <FontAwesomeIcon icon="chevron-down" class="compose__select-caret" />
        </span>
        <span class="compose__select">
          <select
            :value="d.contentType"
            :aria-label="t('compose.contentType')"
            :title="t('compose.contentType')"
            @change="
              compose.update({
                contentType: ($event.target as HTMLSelectElement).value,
              })
            "
          >
            <option v-for="ct in CONTENT_TYPES" :key="ct" :value="ct">
              {{ ct }}
            </option>
          </select>
          <FontAwesomeIcon icon="chevron-down" class="compose__select-caret" />
        </span>

        <div v-if="scheduleVisible" class="compose__schedule">
          <label>
            {{ t("compose.scheduleAt") }}
            <input
              type="datetime-local"
              :value="d.scheduledAt?.slice(0, 16) ?? ''"
              :min="minLocal()"
              @change="
                compose.update({
                  scheduledAt: ($event.target as HTMLInputElement).value
                    ? new Date(
                        ($event.target as HTMLInputElement).value,
                      ).toISOString()
                    : null,
                })
              "
            />
          </label>
          <p v-if="schedulePast" class="compose__error">
            {{ t("compose.schedulePast") }}
          </p>
        </div>
      </div>

      <p v-if="done" class="compose__done">{{ done }}</p>
      <p v-if="error" class="compose__error">{{ error }}</p>
    </div>
  </div>
</template>

<style scoped>
/* The box fills whatever height its host gives it — ComposePage
   assigns a viewport fraction, embedded contexts (inline reply,
   quote modal, thread) let it size to content. The top region takes
   all free space and scrolls when it overflows, so the controls can
   never be pushed or clipped out of view. */
.compose {
  height: 100%;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.compose__top {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.compose__banner {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: var(--space-2) var(--space-3);
  background: var(--bg-sunken);
  border-radius: var(--radius-md);
  font-size: var(--text-sm);
}
.compose__banner-x {
  border: 0;
  background: none;
  color: var(--text-muted);
  cursor: pointer;
  font: inherit;
}
.compose__cw-input,
.compose__text {
  width: 100%;
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  font: inherit;
  background: var(--bg);
  color: var(--text);
  resize: vertical;
}
.compose__text-wrap {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.compose__text {
  flex: 1 1 auto;
  min-height: 4.5rem;
}
.compose__media {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(8rem, 1fr));
  gap: var(--space-2);
}
.compose__media-item {
  margin: 0;
  position: relative;
}
.compose__media-item img {
  width: 100%;
  border-radius: var(--radius-md);
  display: block;
}
.compose__alt {
  width: 100%;
  margin-top: var(--space-1);
  font-size: var(--text-xs);
  padding: var(--space-1);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
}
.compose__media-x {
  position: absolute;
  top: var(--space-1);
  right: var(--space-1);
  border: 0;
  border-radius: var(--radius-sm);
  background: rgb(0 0 0 / 70%);
  color: #fff;
  cursor: pointer;
}
.compose__bottom {
  flex: 0 0 auto;
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.compose__toolbar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-1);
}
.compose__ctl {
  width: 2.25rem;
  height: 2.25rem;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border: 0;
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--text-muted);
  font-size: var(--text-base);
  cursor: pointer;
  transition:
    background-color var(--duration-fast) ease,
    color var(--duration-fast) ease;
}
.compose__ctl--text {
  font-size: var(--text-xs);
  font-weight: 700;
}
.compose__ctl:hover:not(:disabled) {
  background: var(--bg-sunken);
  color: var(--accent-link);
}
.compose__ctl--on {
  background: var(--accent-soft);
  color: var(--accent-link);
}
.compose__ctl:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.compose__file-input {
  display: none;
}
.compose__emoji-wrap {
  position: relative;
  display: inline-flex;
}
.compose__counter {
  margin-inline-start: auto;
  color: var(--text-muted);
  font-size: var(--text-sm);
  font-variant-numeric: tabular-nums;
}
.compose__options {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--text-sm);
  color: var(--text-muted);
}
/* Static "Private" label standing in for the hidden visibility
   picker on DM drafts — same footprint as a select row. */
.compose__private {
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
  padding: var(--space-1) var(--space-2);
  color: var(--text-muted);
}
.compose__select {
  position: relative;
  display: inline-flex;
  align-items: center;
}
.compose__select select {
  appearance: none;
  border: 0;
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--text-muted);
  padding: var(--space-1) var(--space-2);
  padding-inline-end: 1.5rem;
  font-size: var(--text-sm);
  cursor: pointer;
  max-width: 10rem;
}
.compose__select select:hover {
  background: var(--bg-sunken);
}
.compose__select-caret {
  position: absolute;
  inset-inline-end: var(--space-2);
  pointer-events: none;
  font-size: var(--text-xs);
}
.compose__schedule {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-2);
}
.compose__schedule label {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
}
.compose input[type="datetime-local"] {
  border: 1px solid var(--border);
  background: var(--bg);
  color: var(--text);
  border-radius: var(--radius-sm);
  padding: var(--space-1) var(--space-2);
  font: inherit;
  font-size: var(--text-sm);
}
.compose__counter--over,
.compose__over,
.compose__error {
  color: #c0392b;
}
.compose__error {
  margin: 0;
  font-size: var(--text-sm);
}
</style>
<style>
.compose__done {
  color: var(--accent-link);
  margin: 0;
  font-size: var(--text-sm);
}
</style>
