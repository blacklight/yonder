<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import Avatar from "@/components/ui/Avatar.vue";
import Menu, { type MenuItem } from "@/components/ui/Menu.vue";
import Modal from "@/components/ui/Modal.vue";
import Tooltip from "@/components/ui/Tooltip.vue";
import CwBlock from "./CwBlock.vue";
import ShowMore from "./ShowMore.vue";
import StatusMedia from "./StatusMedia.vue";
import StatusQuote from "./StatusQuote.vue";
import StatusPoll from "./StatusPoll.vue";
import StatusFooter from "./StatusFooter.vue";
import ReactionPicker from "./ReactionPicker.vue";
import ComposeBox from "@/components/compose/ComposeBox.vue";
import { sanitizeHtml } from "@/utils/sanitize";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";
import { ApiError } from "@/api/errors";
import { FontAwesomeIcon } from "@/icons";
import {
  deleteStatus,
  pin,
  react,
  translateStatus,
  unpin,
  unreact,
} from "@/api/statuses";
import { blockAccount, muteAccount } from "@/api/accounts";
import { blockDomain } from "@/api/domains";
import { fileReport } from "@/api/reports";
import { adminAccountAction, adminStatusAction } from "@/api/admin";
import { useAuthStore } from "@/stores/auth";
import { useComposeStore } from "@/stores/compose";
import { useInstanceStore } from "@/stores/instance";
import type { Status, StatusVisibility, Translation } from "@/api/entities";

const props = defineProps<{
  status: Status;
  /** DM threads reply through the page's own composer as a direct
   * message — no inline form and never a public reply to a DM. */
  directReply?: boolean;
  /** Render the body in full — the focal status on its permalink page
   * opts out of the long-post clamp. */
  expanded?: boolean;
  /** Profile pages set this so an author-pinned status carries the
   * "Pinned" marker line — Mastodon only annotates pins there, not on
   * timelines where the same flag is also true. */
  pinnedContext?: boolean;
}>();
const emit = defineEmits<{
  removed: [id: string];
  /** Own-status pin/unpin succeeded — the profile re-reads its pinned
   * list; everywhere else it's ignored. */
  pinned: [status: Status];
}>();
const { t, locale } = useI18n();
const router = useRouter();
const auth = useAuthStore();
const compose = useComposeStore();
const instance = useInstanceStore();

// Boosts wrap the inner status — display it, keep the booster line.
const shown = computed(() => props.status.reblog ?? props.status);
// The federated `RE:` paragraph stays in the HTML for Pleroma-family
// renderers that don't read the quote field — when our embedded card
// renders the target it would only duplicate it.
const shownContent = computed(() =>
  shown.value.quote?.quoted_status
    ? shown.value.content.replace(
        /<p>\s*RE:\s*<a\b[^>]*>[\s\S]*?<\/a>\s*<\/p>\s*$/,
        "",
      )
    : shown.value.content,
);

// The whole card is the permalink's click target, Mastodon-style —
// except interactive elements (links, buttons, the composer) and an
// in-progress text selection.
function onCardClick(event: MouseEvent) {
  const target = event.target as HTMLElement | null;
  if (
    target?.closest(
      "a, button, input, textarea, select, label, audio, video, .status-card__reply, .modal__backdrop, .menu",
    )
  ) {
    return;
  }
  if (window.getSelection()?.toString()) return;
  void router.push(`/statuses/${shown.value.id}`);
}
const booster = computed(() =>
  props.status.reblog ? props.status.account : null,
);
// Long bodies clamp behind a "Show more" fade — except CW'd posts
// (already gated by CwBlock) and the expanded focal status.
const clampBody = computed(() => !props.expanded && !shown.value.spoiler_text);

const account = computed(() => shown.value.account);
// A reply to a direct status is itself a DM — regardless of where the
// card renders (thread page passes `directReply`, the home timeline
// doesn't). Going through `openReply` would seed a public reply to a
// private message.
const isDirect = computed(
  () => props.directReply || shown.value.visibility === "direct",
);
// Mastodon's header glyph: globe / open lock / lock / envelope. The
// labels reuse the composer's `vis_*` keys — `private` on the wire is
// "Followers only" there, which is what the tooltip should say too.
const VISIBILITY_ICONS: Record<StatusVisibility, string> = {
  public: "globe",
  unlisted: "lock-open",
  private: "lock",
  direct: "envelope",
};
const visIcon = computed(
  () => VISIBILITY_ICONS[shown.value.visibility] ?? null,
);

// Mastodon's translate affordance: offered when the post's language
// differs from the interface locale (base subtag — `pt-BR` vs `pt`
// doesn't count as foreign) and the backend advertises translation.
// Any status the viewer can see qualifies — the admin chose the
// backend gated content is sent to.
const baseLang = (code: string | null | undefined) =>
  code?.split("-")[0]?.toLowerCase() ?? null;
const wantsTranslate = computed(() => {
  const lang = baseLang(shown.value.language);
  return !!lang && lang !== baseLang(locale.value);
});
const canTranslate = computed(
  () => instance.translationEnabled && wantsTranslate.value,
);
const translation = ref<Translation | null>(null);
const translating = ref(false);

onMounted(() => {
  // Only cards that could offer the button need the flag; `ensure()`
  // dedupes the fetch across every card on the page.
  if (wantsTranslate.value) void instance.ensure();
});

async function doTranslate() {
  translating.value = true;
  try {
    translation.value = await translateStatus(
      String(shown.value.id),
      locale.value,
    );
  } catch (e) {
    fail(e);
  } finally {
    translating.value = false;
  }
}

/** Language name for the "Translated from" line, localized via
 * `Intl.DisplayNames` with the raw code as fallback. */
const translatedFromName = computed(() => {
  const lang =
    translation.value?.detected_source_language ?? shown.value.language;
  if (!lang) return "";
  try {
    return (
      new Intl.DisplayNames([locale.value], { type: "language" }).of(lang) ??
      lang
    );
  } catch {
    return lang;
  }
});

// Translated content swaps in for the body, CW, poll options and alt
// text — the card's other props (media, footer actions) stay on the
// original entity.
const displayedContent = computed(() =>
  sanitizeHtml(translation.value?.content ?? shownContent.value, {
    mentions: shown.value.mentions,
    tags: shown.value.tags,
  }),
);
const displayedSpoiler = computed(
  () => translation.value?.spoiler_text || shown.value.spoiler_text,
);
const displayedStatus = computed(() => {
  const tr = translation.value;
  if (!tr) return shown.value;
  const out = { ...shown.value };
  if (out.poll && tr.poll?.options?.length) {
    out.poll = {
      ...out.poll,
      options: out.poll.options.map((o, i) => ({
        ...o,
        title: tr.poll!.options[i]?.title ?? o.title,
      })),
    };
  }
  if (tr.media_attachments.length) {
    const desc = new Map(
      tr.media_attachments.map((m) => [m.id, m.description]),
    );
    out.media_attachments = out.media_attachments.map((m) =>
      desc.has(m.id) ? { ...m, description: desc.get(m.id)! } : m,
    );
  }
  return out;
});
// Expanded (permalink) cards get a stats strip — "N boosts · N
// quotes · N likes" — that links to the per-interaction lists.
// Zero counters render as plain text: nothing to list.
const stats = computed(() => [
  {
    path: "boosts",
    key: "status.boostsCount",
    count: shown.value.reblogs_count,
  },
  {
    path: "quotes",
    key: "status.quotesCount",
    count: shown.value.quotes_count,
  },
  {
    path: "favourites",
    key: "status.likesCount",
    count: shown.value.favourites_count,
  },
]);

const remoteDomain = computed(() => {
  const acct = account.value.acct;
  return acct.includes("@") ? acct.split("@").slice(1).join("@") : null;
});
const faviconFailed = ref(false);
const pickerOpen = ref(false);
const reportOpen = ref(false);
const reportComment = ref("");
const actionError = ref("");

// Card URLs are scheme-checked upstream (`preview.rs` only writes
// `preview_cards` rows for `valid_url_schemes` links) — re-checked
// at the sink so a bad or future card payload can't bind an unsafe
// scheme to `href`.
const card = computed(() => {
  const c = shown.value.card;
  return c && /^https?:\/\//i.test(c.url) ? c : null;
});

// Video cards (YouTube, Vimeo, … — `twitter:player`/`og:video`
// metadata) render click-to-play: the thumbnail with a play button,
// and the third-party iframe only loads once requested — never
// eagerly per card on a timeline.
const cardPlaying = ref(false);
watch(
  () => shown.value.id,
  () => (cardPlaying.value = false),
);
const videoEmbed = computed(() => {
  const c = card.value;
  if (c?.type !== "video" || !c.embed_url) return null;
  return /^https?:\/\//i.test(c.embed_url) ? c.embed_url : null;
});
// Clicking play should actually play — `autoplay=1` is honored by
// YouTube/Vimeo and ignored by players that don't know it.
const embedSrc = computed(() => {
  if (!videoEmbed.value) return "";
  try {
    const u = new URL(videoEmbed.value);
    u.searchParams.set("autoplay", "1");
    return u.toString();
  } catch {
    return videoEmbed.value;
  }
});
const playerStyle = computed(() => {
  const w = card.value?.width ?? 0;
  const h = card.value?.height ?? 0;
  return { aspectRatio: w > 0 && h > 0 ? `${w} / ${h}` : "16 / 9" };
});

// Destructive menu entries are routed through a confirm dialog first.
const CONFIRM_KEYS: Record<string, string> = {
  delete: "delete",
  "admin-delete": "adminDeleteInstance",
  "admin-delete-profile": "adminDeleteProfile",
  "admin-suspend": "adminSuspend",
  "admin-delete-user": "adminDeleteUser",
  "admin-limit": "adminLimit",
};
const pendingAction = ref<{ text: string; run: () => void } | null>(null);

function confirmThen(text: string, run: () => void) {
  pendingAction.value = { text, run };
}

async function runPending() {
  const action = pendingAction.value;
  pendingAction.value = null;
  action?.run();
}

/** Reply/quote/mention/DM replace the compose draft — ask first when
 * the user has an unsaved draft. */
function guardDraft(action: () => void) {
  if (compose.isDirty) confirmThen(t("compose.discardText"), action);
  else action();
}

/** Reply on a status — a direct status seeds a DM reply. The thread
 * page has its own pinned composer; elsewhere there's no inline form
 * for a DM, so land on /compose with the seeded draft. */
function replyTo(s: Status) {
  if (isDirect.value) {
    compose.openDirectReply(s);
    if (!props.directReply) void router.push("/compose");
  } else {
    compose.openReply(s);
  }
}

function fail(e: unknown) {
  actionError.value = e instanceof ApiError ? e.message : String(e);
}

const menuItems = computed<MenuItem[]>(() => {
  const items: MenuItem[] = [
    { id: "copy", label: t("status.copyLink"), icon: "link" },
    {
      id: "open",
      label: t("status.openTab"),
      icon: "arrow-up-right-from-square",
    },
  ];
  if (auth.isLoggedIn) {
    const isOwn = auth.account?.id === account.value.id;
    items.push(
      { id: "mention", label: t("status.mention"), icon: "at" },
      { id: "dm", label: t("status.dm"), icon: "envelope" },
    );
    // Relationship actions target the author — muting, blocking or
    // reporting yourself is nonsense (and the backend rejects it).
    if (!isOwn) {
      items.push(
        {
          id: "mute",
          label: t("status.mute", { user: `@${account.value.acct}` }),
          icon: "volume-xmark",
          danger: true,
        },
        {
          id: "block",
          label: t("status.block", { user: `@${account.value.acct}` }),
          icon: "ban",
          danger: true,
        },
        {
          id: "report",
          label: t("status.report", { user: `@${account.value.acct}` }),
          icon: "flag",
          danger: true,
        },
      );
      if (remoteDomain.value)
        items.push({
          id: "block-domain",
          label: t("status.blockDomain", { domain: remoteDomain.value }),
          icon: "globe",
          danger: true,
        });
    }
    if (isOwn) {
      // The backend rejects direct pins and boost pins — mirror that
      // here so the item never offers a doomed action.
      if (shown.value.visibility !== "direct" && !props.status.reblog) {
        items.push({
          id: "pin",
          label: shown.value.pinned ? t("status.unpin") : t("status.pin"),
          icon: "thumbtack",
        });
      }
      items.push(
        { id: "edit", label: t("status.edit"), icon: "pen" },
        {
          id: "delete",
          label: t("status.delete"),
          icon: "trash-can",
          danger: true,
        },
      );
    }
    if (auth.isStaff) {
      // Status-targeted admin verbs stay on own posts (they apply to
      // the post, not the person); account-targeted ones don't —
      // limiting, suspending or deleting yourself must never be a
      // misclick away. `admin-delete` is the exception: it deletes
      // the status too, so on own posts it's redundant with `delete`.
      items.push({
        id: "mark-sensitive",
        label: t("status.adminMarkSensitive"),
        icon: "eye-slash",
      });
      if (!isOwn) {
        items.push({
          id: "admin-delete",
          label: t("status.adminDeleteInstance"),
          icon: "trash-can",
          danger: true,
        });
        if (!remoteDomain.value)
          items.push({
            id: "admin-delete-profile",
            label: t("status.adminDeleteProfile"),
            icon: "user-xmark",
            danger: true,
          });
        items.push(
          {
            id: "admin-limit",
            label: t("status.adminLimit"),
            icon: "comment-slash",
            danger: true,
          },
          {
            id: "admin-suspend",
            label: t("status.adminSuspend"),
            icon: "user-slash",
            danger: true,
          },
        );
        if (auth.isAdmin)
          items.push({
            id: "admin-delete-user",
            label: t("status.adminDeleteUser"),
            icon: "user-minus",
            danger: true,
          });
      }
    }
  }
  return items;
});

function onMenu(item: MenuItem) {
  actionError.value = "";
  const confirmKey = CONFIRM_KEYS[item.id];
  if (confirmKey) {
    confirmThen(
      t(`status.confirm.${confirmKey}`, { user: `@${account.value.acct}` }),
      () => void runAction(item),
    );
    return;
  }
  if (item.id === "mention" || item.id === "dm") {
    guardDraft(() => void runAction(item));
    return;
  }
  void runAction(item);
}

async function runAction(item: MenuItem) {
  const s = shown.value;
  const id = s.account.id;
  try {
    switch (item.id) {
      case "copy":
        await navigator.clipboard.writeText(s.url ?? s.uri);
        break;
      case "open":
        window.open(s.url ?? s.uri, "_blank", "noopener");
        break;
      case "mention":
        compose.openMention(s.account.acct);
        await router.push("/compose");
        break;
      case "dm":
        compose.openDirect(s.account.acct);
        await router.push("/compose");
        break;
      case "mute":
        await muteAccount(id);
        break;
      case "block":
        await blockAccount(id);
        break;
      case "block-domain":
        if (remoteDomain.value) await blockDomain(remoteDomain.value);
        break;
      case "report":
        reportComment.value = "";
        reportOpen.value = true;
        break;
      case "pin": {
        const updated = s.pinned ? await unpin(s.id) : await pin(s.id);
        // Write-through like `mark-sensitive`: every card sharing the
        // entity picks up the flag; the profile listens for `pinned`
        // to refresh its pinned section.
        s.pinned = updated.pinned;
        emit("pinned", updated);
        break;
      }
      case "edit":
        await router.push({ path: "/compose", query: { edit: s.id } });
        break;
      case "delete":
        await deleteStatus(s.id);
        emit("removed", s.id);
        break;
      case "mark-sensitive":
        await adminStatusAction(s.id, "sensitive");
        // `shown` aliases the timeline store's reactive entity — the
        // write-through updates every card sharing it (see the same
        // pattern in StatusFooter).
        s.sensitive = true;
        break;
      case "admin-delete":
      case "admin-delete-profile":
        await adminStatusAction(s.id, "delete");
        emit("removed", s.id);
        break;
      case "admin-limit":
        await adminAccountAction(id, { type: "limit" });
        break;
      case "admin-suspend":
        await adminAccountAction(id, { type: "suspend" });
        break;
      case "admin-delete-user":
        await adminAccountAction(id, { type: "delete" });
        break;
    }
  } catch (e) {
    fail(e);
  }
}

async function submitReport() {
  try {
    await fileReport({
      account_id: Number(shown.value.account.id),
      status_ids: [Number(shown.value.id)],
      comment: reportComment.value || undefined,
    });
    reportOpen.value = false;
  } catch (e) {
    fail(e);
  }
}

async function onReact(emoji: string) {
  pickerOpen.value = false;
  const reactions = shown.value.pleroma?.emoji_reactions ?? [];
  const existing = reactions.find(
    (r) => r.name === emoji || r.name === `:${emoji}:`,
  );
  try {
    if (existing?.me) await unreact(shown.value.id, emoji);
    else await react(shown.value.id, emoji);
  } catch (e) {
    fail(e);
  }
}
</script>

<template>
  <article class="status-card" :data-status-id="status.id" @click="onCardClick">
    <p v-if="booster" class="status-card__boost">
      <FontAwesomeIcon icon="retweet" />
      {{
        t("status.boosted", { name: booster.display_name || booster.username })
      }}
    </p>
    <p
      v-else-if="pinnedContext && shown.pinned"
      class="status-card__boost status-card__pin"
    >
      <FontAwesomeIcon icon="thumbtack" />
      {{ t("status.pinned") }}
    </p>

    <header class="status-card__header">
      <router-link :to="`/@${account.acct}`" class="status-card__author">
        <Avatar
          :src="account.avatar"
          :name="account.display_name || account.username"
          size="md"
        />
        <div class="status-card__names">
          <span class="status-card__display">{{
            account.display_name || account.username
          }}</span>
          <span class="status-card__acct">@{{ account.acct }}</span>
        </div>
      </router-link>
      <img
        v-if="remoteDomain && !faviconFailed"
        :src="`https://${remoteDomain}/favicon.ico`"
        class="status-card__favicon"
        :alt="remoteDomain"
        loading="lazy"
        @error="faviconFailed = true"
      />
      <Tooltip :text="formatAbsoluteDate(shown.created_at)">
        <time class="status-card__date" :datetime="shown.created_at">{{
          formatRelativeDate(shown.created_at)
        }}</time>
      </Tooltip>
      <Tooltip
        v-if="shown.edited_at"
        :text="
          t('status.editedAt', {
            date: formatAbsoluteDate(shown.edited_at),
          })
        "
      >
        <time
          class="status-card__edited"
          :datetime="shown.edited_at"
          data-testid="edited"
        >
          <FontAwesomeIcon icon="pen-to-square" />
          <span class="visually-hidden">{{ t("status.edited") }}</span>
        </time>
      </Tooltip>
      <Tooltip v-if="visIcon" :text="t(`compose.vis_${shown.visibility}`)">
        <span
          class="status-card__visibility"
          role="img"
          :aria-label="t(`compose.vis_${shown.visibility}`)"
          data-testid="visibility"
        >
          <FontAwesomeIcon :icon="visIcon" />
        </span>
      </Tooltip>
      <Menu :items="menuItems" :label="t('status.more')" @select="onMenu" />
    </header>

    <p v-if="actionError" class="status-card__error">{{ actionError }}</p>

    <CwBlock :spoiler-text="displayedSpoiler">
      <!-- Only the text clamps — media, quotes and polls always
           render whole, never sliced by the fade. -->
      <ShowMore :disabled="!clampBody">
        <!-- eslint-disable-next-line vue/no-v-html -->
        <div class="status-card__content" v-html="displayedContent" />
      </ShowMore>

      <StatusMedia
        :media="displayedStatus.media_attachments"
        :sensitive="shown.sensitive && !shown.spoiler_text"
      />
      <StatusQuote
        :status="shown"
        @reply="(s) => guardDraft(() => replyTo(s))"
        @quote="(s) => guardDraft(() => compose.openQuote(s))"
      />
      <StatusPoll :status="displayedStatus" />
    </CwBlock>

    <div v-if="canTranslate || translation" class="status-card__translation">
      <template v-if="translation">
        <span class="status-card__translated">
          {{
            t("status.translatedFrom", {
              from: translatedFromName,
              provider: translation.provider,
            })
          }}
        </span>
        <button
          type="button"
          class="status-card__translate"
          data-testid="show-original"
          @click="translation = null"
        >
          {{ t("status.showOriginal") }}
        </button>
      </template>
      <button
        v-else
        type="button"
        class="status-card__translate"
        data-testid="translate"
        :disabled="translating"
        @click="doTranslate"
      >
        <FontAwesomeIcon icon="language" />
        {{ translating ? t("status.translating") : t("status.translate") }}
      </button>
    </div>

    <div
      v-if="card && videoEmbed"
      class="status-card__player"
      :style="playerStyle"
    >
      <iframe
        v-if="cardPlaying"
        :src="embedSrc"
        :title="card.title || card.provider_name || t('status.media.play')"
        class="status-card__player-iframe"
        allow="autoplay; fullscreen; encrypted-media; picture-in-picture"
        sandbox="allow-scripts allow-same-origin allow-presentation allow-popups allow-popups-to-escape-sandbox"
        allowfullscreen
      />
      <button
        v-else
        type="button"
        class="status-card__player-btn"
        :aria-label="t('status.media.play')"
        @click="cardPlaying = true"
      >
        <img
          v-if="card.image"
          :src="card.image"
          alt=""
          class="status-card__player-img"
        />
        <span class="status-card__player-play" aria-hidden="true">
          <FontAwesomeIcon icon="play" />
        </span>
        <span class="status-card__player-info">
          <strong v-if="card.title">{{ card.title }}</strong>
          <span v-if="card.provider_name">{{ card.provider_name }}</span>
        </span>
      </button>
    </div>
    <a
      v-else-if="card"
      :href="card.url"
      class="status-card__preview"
      target="_blank"
      rel="noopener"
    >
      <img
        v-if="card.image"
        :src="card.image"
        alt=""
        class="status-card__preview-img"
      />
      <span class="status-card__preview-body">
        <strong>{{ card.title }}</strong>
        <span class="status-card__preview-desc">{{ card.description }}</span>
        <span class="status-card__preview-provider">{{
          card.provider_name
        }}</span>
      </span>
    </a>

    <div
      v-if="shown.pleroma?.emoji_reactions?.length"
      class="status-card__reactions"
    >
      <button
        v-for="r in shown.pleroma.emoji_reactions"
        :key="r.name"
        type="button"
        class="status-card__reaction"
        :class="{ 'status-card__reaction--me': r.me }"
        :disabled="!auth.isLoggedIn"
        @click="onReact(r.name)"
      >
        <img
          v-if="r.url"
          :src="r.url"
          :alt="r.name"
          class="status-card__reaction-img"
        />
        <template v-else>{{ r.name }}</template>
        {{ r.count }}
      </button>
    </div>

    <div v-if="expanded" class="status-card__stats">
      <template v-for="(stat, i) in stats" :key="stat.path">
        <span v-if="i > 0" class="status-card__stat-sep">·</span>
        <router-link
          v-if="stat.count > 0"
          :to="`/statuses/${shown.id}/${stat.path}`"
          class="status-card__stat"
          >{{ t(stat.key, { count: stat.count }) }}</router-link
        >
        <span v-else class="status-card__stat">{{
          t(stat.key, { count: stat.count })
        }}</span>
      </template>
    </div>

    <div class="status-card__footer-wrap">
      <StatusFooter
        :status="shown"
        @reply="(s) => guardDraft(() => replyTo(s))"
        @quote="(s) => guardDraft(() => compose.openQuote(s))"
        @react="pickerOpen = !pickerOpen"
      />
      <ReactionPicker v-if="pickerOpen" @select="onReact" />
    </div>

    <ComposeBox
      v-if="!isDirect && compose.replyTargetId === shown.id"
      class="status-card__reply"
    />

    <Modal
      v-if="pendingAction"
      :title="t('status.confirm.title')"
      @close="pendingAction = null"
    >
      <p class="status-card__confirm-text">{{ pendingAction.text }}</p>
      <div class="status-card__confirm-btns">
        <button
          type="button"
          class="status-card__report-submit"
          @click="pendingAction = null"
        >
          {{ t("ui.cancel") }}
        </button>
        <button
          type="button"
          class="status-card__report-submit status-card__confirm-yes"
          data-testid="confirm-yes"
          @click="runPending"
        >
          {{ t("ui.confirm") }}
        </button>
      </div>
    </Modal>

    <Modal
      v-if="reportOpen"
      :title="t('status.reportTitle')"
      @close="reportOpen = false"
    >
      <form @submit.prevent="submitReport">
        <textarea
          v-model="reportComment"
          class="status-card__report-text"
          :placeholder="t('status.reportPlaceholder')"
          rows="3"
        />
        <button type="submit" class="status-card__report-submit">
          {{ t("status.reportSubmit") }}
        </button>
      </form>
    </Modal>
  </article>
</template>

<style scoped>
.status-card {
  background: var(--bg-raised);
  padding: var(--space-4);
  border-bottom: 1px solid var(--border-dim);
  cursor: pointer;
}
.status-card a,
.status-card button,
.status-card .status-card__reply {
  cursor: auto;
}
.status-card__boost {
  color: var(--text-muted);
  font-size: var(--text-sm);
  margin: 0 0 var(--space-2);
}
.status-card__header {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin-bottom: var(--space-2);
}
.status-card__author {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  min-width: 0;
  flex: 1;
  color: var(--text);
  text-decoration: none;
}
.status-card__author:hover .status-card__display {
  text-decoration: underline;
}
.status-card__author .avatar {
  cursor: pointer;
}
.status-card__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
  cursor: pointer;
}
.status-card__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.status-card__acct {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.status-card__favicon {
  width: 14px;
  height: 14px;
}
.status-card__date {
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.status-card__edited {
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.status-card__edited::before {
  content: "·";
  margin-right: var(--space-1);
}
.status-card__visibility,
.status-card__edited {
  opacity: 0.75;
}
.status-card__visibility {
  color: var(--text-muted);
  font-size: 0.75em;
  display: inline-flex;
}
.status-card__translation {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin-top: var(--space-2);
  font-size: var(--text-sm);
}
.status-card__translation button {
  cursor: pointer;
}
.status-card__translated {
  color: var(--text-muted);
  font-size: 0.85em;
  opacity: 0.6;
}
.status-card__translate {
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
  padding: 0;
  border: 0;
  background: none;
  color: var(--accent-link);
  font: inherit;
  cursor: pointer;
}
.status-card__translate:hover:not(:disabled) {
  text-decoration: underline;
}
.status-card__translate:disabled {
  color: var(--text-muted);
  cursor: default;
}
.status-card__content {
  overflow-wrap: break-word;
}
:deep(.status-card__content) pre,
:deep(.status-card__content) code {
  background: var(--bg);
  border-radius: var(--radius-md);
}
:deep(.status-card__content) pre {
  max-height: 30rem;
  overflow: auto;
  padding: var(--space-3);
}
.status-card__preview {
  display: flex;
  gap: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-3);
  margin: var(--space-2) 0;
  text-decoration: none;
  color: var(--text);
}
.status-card__preview-img {
  width: 4rem;
  height: 4rem;
  object-fit: cover;
  border-radius: var(--radius-sm);
}
.status-card__preview-body {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.status-card__preview-desc {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
}
.status-card__preview-provider {
  color: var(--text-muted);
  font-size: var(--text-xs);
}
.status-card__player {
  position: relative;
  margin: var(--space-2) 0;
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  overflow: hidden;
  background: #000;
  max-height: 26rem;
}
.status-card__player-iframe {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  border: 0;
}
.status-card__player-btn {
  position: absolute;
  inset: 0;
  width: 100%;
  padding: 0;
  border: 0;
  background: transparent;
  color: #fff;
  font: inherit;
  cursor: pointer;
}
.status-card__player-btn:focus-visible {
  outline: 2px solid var(--accent-link);
  outline-offset: -2px;
}
.status-card__player-img {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.status-card__player-play {
  position: absolute;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  width: 3.5rem;
  height: 3.5rem;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.65);
  font-size: 1.25rem;
  transition: background 0.15s ease;
}
.status-card__player-btn:hover .status-card__player-play,
.status-card__player-btn:focus-visible .status-card__player-play {
  background: var(--accent);
  color: var(--accent-fg);
}
.status-card__player-info {
  position: absolute;
  left: 0;
  right: 0;
  bottom: 0;
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 0.1rem;
  padding: var(--space-2) var(--space-3);
  text-align: left;
  font-size: var(--text-sm);
  background: linear-gradient(transparent, rgba(0, 0, 0, 0.8));
}
.status-card__player-info > span {
  color: rgba(255, 255, 255, 0.75);
  font-size: var(--text-xs);
}
.status-card__reactions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-1);
  margin-top: var(--space-2);
}
.status-card__reaction {
  border: 1px solid var(--border);
  background: var(--bg-raised);
  border-radius: var(--radius-full);
  padding: 2px var(--space-2);
  cursor: pointer;
  font: inherit;
  color: var(--text);
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
}
.status-card__reaction--me {
  border-color: var(--accent);
  background: color-mix(in srgb, var(--accent) 12%, transparent);
}
.status-card__reaction-img {
  width: 1rem;
  height: 1rem;
}
.status-card__stats {
  display: flex;
  gap: var(--space-2);
  margin-top: var(--space-3);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border);
  color: var(--text-faint);
  font-size: var(--text-sm);
}
.status-card__stat {
  color: var(--text-muted);
}
a.status-card__stat {
  text-decoration: none;
  cursor: pointer;
}
a.status-card__stat:hover {
  color: var(--text);
  text-decoration: underline;
}
.status-card__stat-sep {
  color: var(--text-faint);
}
.status-card__footer-wrap {
  position: relative;
}
.status-card__reply {
  margin-top: var(--space-3);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border);
}
.status-card__error {
  color: #c0392b;
  margin: var(--space-1) 0;
}
.status-card__report-text {
  width: 100%;
  font: inherit;
  margin-bottom: var(--space-2);
}
.status-card__confirm-text {
  margin: 0 0 var(--space-3);
}
.status-card__confirm-btns {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-2);
}
.status-card__confirm-yes {
  border-color: var(--accent);
  color: var(--accent-link);
  font-weight: 600;
}
.status-card__report-submit {
  border: 1px solid var(--border);
  background: var(--bg-raised);
  border-radius: var(--radius-md);
  padding: var(--space-1) var(--space-3);
  color: var(--text);
  cursor: pointer;
}
</style>
