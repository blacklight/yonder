<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from "vue";
import { useRoute } from "vue-router";
import { useI18n } from "vue-i18n";
import Button from "@/components/ui/Button.vue";
import PollBuilder from "./PollBuilder.vue";
import EmojiPicker from "./EmojiPicker.vue";
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
import { useComposeStore } from "@/stores/compose";
import { useSettingsStore } from "@/stores/settings";
import { useTimelineStore } from "@/stores/timeline";
import type {
  MediaAttachment,
  ScheduledStatus,
  Status,
  StatusVisibility,
} from "@/api/entities";

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
const editId = ref<string | null>(null);
const textarea = ref<HTMLTextAreaElement | null>(null);

// One key per *submission body* — an unchanged retry reuses it so a
// repeat POST replays; editing the draft mints a fresh key so a
// committed-but-timed-out request can't swallow the new text.
const idemKey = ref(crypto.randomUUID());
let idemBody = "";

const d = computed(() => compose.draft);
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
  { code: "es", label: "Español" },
  { code: "it", label: "Italiano" },
  { code: "de", label: "Deutsch" },
  { code: "ar", label: "العربية" },
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
      });
      return;
    } catch (e) {
      error.value = e instanceof ApiError ? e.message : String(e);
    }
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
  // New drafts pick up the account defaults from settings.
  const s = settings.settings as Record<string, unknown> | null;
  if (s && !d.value.text && !d.value.inReplyToId) {
    compose.update({
      visibility: (s.default_visibility as StatusVisibility) ?? "public",
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

function insertEmoji(shortcode: string) {
  const el = textarea.value;
  const insert = `:${shortcode}:`;
  if (el) {
    const start = el.selectionStart ?? d.value.text.length;
    const end = el.selectionEnd ?? start;
    const text =
      d.value.text.slice(0, start) + insert + d.value.text.slice(end);
    compose.update({ text });
  } else {
    compose.update({ text: d.value.text + insert });
  }
  emojiOpen.value = false;
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
      visibility: d.value.visibility,
      language: d.value.language || undefined,
      sensitive: d.value.sensitive || d.value.cwEnabled,
      spoiler_text: d.value.cwEnabled ? d.value.spoilerText : undefined,
      in_reply_to_id: d.value.inReplyToId ?? undefined,
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
    compose.reset();
    media.value = [];
    editId.value = null;
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
    <div v-if="banner" class="compose__banner">
      <span>{{ banner }}</span>
      <button type="button" class="compose__banner-x" @click="compose.reset()">
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

    <textarea
      ref="textarea"
      :value="d.text"
      class="compose__text"
      :placeholder="t('compose.placeholder')"
      rows="5"
      data-testid="compose-text"
      @input="
        compose.update({ text: ($event.target as HTMLTextAreaElement).value })
      "
      @keydown.ctrl.enter.prevent="submit"
      @keydown.meta.enter.prevent="submit"
    />

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

    <div class="compose__controls">
      <select
        :value="d.language ?? ''"
        :aria-label="t('compose.language')"
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
      <select
        :value="d.visibility"
        :aria-label="t('compose.visibility')"
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
      <select
        :value="d.contentType"
        :aria-label="t('compose.contentType')"
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
      <button
        type="button"
        class="compose__ctl"
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
      <label
        class="compose__ctl compose__file"
        :title="t('compose.addMedia')"
        :aria-label="t('compose.addMedia')"
      >
        <FontAwesomeIcon icon="paperclip" />
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
          :title="t('compose.emoji')"
          :aria-label="t('compose.emoji')"
          @click="emojiOpen = !emojiOpen"
        >
          <FontAwesomeIcon :icon="['far', 'face-smile']" />
        </button>
        <EmojiPicker v-if="emojiOpen" @select="insertEmoji" />
      </span>
    </div>

    <div class="compose__schedule">
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

    <div class="compose__footer">
      <span
        class="compose__counter"
        :class="{ 'compose__counter--over': overLimit }"
        data-testid="counter"
        >{{ charLen }} / {{ maxChars }}</span
      >
      <span v-if="overLimit" class="compose__over">{{
        t("compose.overLimit")
      }}</span>
      <p v-if="done" class="compose__done">{{ done }}</p>
      <p v-if="error" class="compose__error">{{ error }}</p>
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
  </div>
</template>

<style scoped>
.compose {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
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
.compose__controls {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
  align-items: center;
}
.compose__controls select,
.compose input[type="datetime-local"] {
  border: 1px solid var(--border);
  background: var(--bg);
  color: var(--text);
  border-radius: var(--radius-sm);
  padding: var(--space-1) var(--space-2);
  font: inherit;
}
.compose__ctl {
  border: 1px solid var(--border);
  background: var(--bg-raised);
  border-radius: var(--radius-sm);
  padding: var(--space-1) var(--space-2);
  cursor: pointer;
  color: var(--text);
  font: inherit;
}
.compose__ctl--on {
  border-color: var(--accent);
  color: var(--accent-link);
}
.compose__file-input {
  display: none;
}
.compose__emoji-wrap {
  position: relative;
}
.compose__schedule {
  font-size: var(--text-sm);
  color: var(--text-muted);
}
.compose__footer {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.compose__counter {
  color: var(--text-muted);
  font-size: var(--text-sm);
  margin-left: auto;
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
