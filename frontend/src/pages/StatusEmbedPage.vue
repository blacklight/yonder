<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import Avatar from "@/components/ui/Avatar.vue";
import EmojiText from "@/components/ui/EmojiText.vue";
import Spinner from "@/components/ui/Spinner.vue";
import StatusMedia from "@/components/status/StatusMedia.vue";
import StatusEmbedQuote from "@/components/status/StatusEmbedQuote.vue";
import StatusEmbedCard from "@/components/status/StatusEmbedCard.vue";
import { getStatus } from "@/api/statuses";
import { sanitizeHtml } from "@/utils/sanitize";
import { highlightCodeBlocks } from "@/utils/highlight";
import { formatAbsoluteDate } from "@/utils/relativeDate";
import { FontAwesomeIcon } from "@/icons";
import type { Status } from "@/api/entities";

// Standalone view served inside <iframe>/<script> embeds on third-party
// pages — no app chrome (App.vue bypasses AppLayout for `meta.embed`),
// anonymous by default, every link opens outside the frame.
const props = defineProps<{ id: string }>();
const { t } = useI18n();

const status = ref<Status | null>(null);
const failed = ref(false);
const root = ref<HTMLElement | null>(null);

// Boosts wrap the inner status — the embed shows what was boosted,
// annotated with a booster line like the card does.
const shown = computed(() => status.value?.reblog ?? status.value ?? null);
const booster = computed(() =>
  status.value?.reblog ? status.value.account : null,
);
const account = computed(() => shown.value?.account ?? null);
// Same rule as StatusCard: the federated `RE:` paragraph duplicates the
// quote card — strip it whenever the quote will actually render.
const RE_PARAGRAPH = /<p>\s*RE:\s*<a\b[^>]*>[\s\S]*?<\/a>\s*<\/p>\s*$/;
const hasQuote = computed(() =>
  Boolean(
    shown.value?.quote ||
    shown.value?.quote_id ||
    shown.value?.yonder?.quote?.uri,
  ),
);
const quoteRenders = computed(() =>
  Boolean(
    shown.value?.quote?.quoted_status ||
    (shown.value?.quote?.state === "accepted" && shown.value?.quote_id),
  ),
);
const content = computed(() => {
  const s = shown.value;
  if (!s) return "";
  const raw = quoteRenders.value
    ? s.content.replace(RE_PARAGRAPH, "")
    : s.content;
  return highlightCodeBlocks(
    sanitizeHtml(raw, {
      mentions: s.mentions,
      tags: s.tags,
      emojis: s.emojis,
    }),
  );
});

const permalink = computed(() => shown.value?.url ?? shown.value?.uri ?? "");

// The iframe auto-resize protocol: embed.js swaps .yonder-embed
// placeholders for iframes and listens for these messages to keep the
// frame height in sync with the rendered post. The <main> element is
// measured, not the document — documentElement.scrollHeight bottoms
// out at the viewport height, so it could only ever grow the frame.
function reportHeight() {
  const el = root.value;
  if (!el) return;
  window.parent.postMessage(
    {
      source: "yonder-embed",
      type: "resize",
      height: Math.ceil(el.getBoundingClientRect().height),
    },
    "*",
  );
}

let resizeObserver: ResizeObserver | null = null;
onMounted(async () => {
  // Transparent canvas: the card floats on the host page's background
  // instead of sitting on a gray slab below it.
  document.documentElement.style.background = "transparent";
  document.body.style.background = "transparent";
  if (typeof ResizeObserver !== "undefined" && root.value) {
    resizeObserver = new ResizeObserver(reportHeight);
    resizeObserver.observe(root.value);
  }
  try {
    status.value = await getStatus(props.id);
  } catch {
    failed.value = true;
  } finally {
    reportHeight();
  }
});
onUnmounted(() => resizeObserver?.disconnect());

// Every link opens in a new top-level tab — navigating the iframe
// itself (or the host page) would strand the embed.
function onClick(event: MouseEvent) {
  const anchor = (event.target as HTMLElement | null)?.closest("a");
  const href = anchor?.getAttribute("href");
  if (!anchor || !href) return;
  event.preventDefault();
  event.stopPropagation();
  window.open(new URL(href, window.location.href).href, "_blank", "noopener");
}
</script>

<template>
  <main ref="root" class="embed" @click="onClick">
    <div v-if="!status && !failed" class="embed__loading" role="status">
      <Spinner size="sm" />
      <span class="visually-hidden">{{ t("ui.loading") }}</span>
    </div>

    <p v-else-if="failed || !shown" class="embed__error" role="alert">
      {{ t("embed.unavailable") }}
    </p>

    <article v-else class="embed-card">
      <p v-if="booster" class="embed-card__boost">
        <FontAwesomeIcon icon="retweet" />
        <i18n-t keypath="status.boosted" tag="span">
          <template #name
            ><EmojiText
              :text="booster.display_name || booster.username"
              :emojis="booster.emojis"
          /></template>
        </i18n-t>
      </p>

      <header class="embed-card__header">
        <a
          v-if="account"
          class="embed-card__author"
          :href="account.url"
          target="_blank"
          rel="noopener"
        >
          <Avatar
            :src="account.avatar"
            :name="account.display_name || account.username"
            size="md"
          />
          <span class="embed-card__names">
            <span class="embed-card__display"
              ><EmojiText
                :text="account.display_name || account.username"
                :emojis="account.emojis"
            /></span>
            <span class="embed-card__acct">@{{ account.acct }}</span>
          </span>
        </a>
        <a
          class="embed-card__date"
          :href="permalink"
          target="_blank"
          rel="noopener"
          ><time :datetime="shown.created_at">{{
            formatAbsoluteDate(shown.created_at)
          }}</time></a
        >
      </header>

      <details v-if="shown.spoiler_text" class="embed-card__cw">
        <summary class="embed-card__cw-summary">
          <EmojiText :text="shown.spoiler_text" :emojis="shown.emojis" />
        </summary>
        <div class="embed-card__body">
          <!-- eslint-disable-next-line vue/no-v-html -->
          <div class="embed-card__content" v-html="content" />
          <StatusMedia
            :media="shown.media_attachments"
            :sensitive="shown.sensitive"
          />
          <StatusEmbedQuote v-if="hasQuote" :status="shown" />
          <ul v-if="shown.poll" class="embed-card__poll">
            <li v-for="(option, i) in shown.poll.options" :key="i">
              {{ option.title }}
              <span
                v-if="option.votes_count !== null"
                class="embed-card__votes"
              >
                — {{ t("status.pollVotes", { count: option.votes_count }) }}
              </span>
            </li>
          </ul>
          <StatusEmbedCard :status="shown" />
        </div>
      </details>
      <div v-else class="embed-card__body">
        <!-- eslint-disable-next-line vue/no-v-html -->
        <div class="embed-card__content" v-html="content" />
        <StatusMedia
          :media="shown.media_attachments"
          :sensitive="shown.sensitive"
        />
        <StatusEmbedQuote v-if="hasQuote" :status="shown" />
        <ul v-if="shown.poll" class="embed-card__poll">
          <li v-for="(option, i) in shown.poll.options" :key="i">
            {{ option.title }}
            <span v-if="option.votes_count !== null" class="embed-card__votes">
              — {{ t("status.pollVotes", { count: option.votes_count }) }}
            </span>
          </li>
        </ul>
        <StatusEmbedCard :status="shown" />
      </div>

      <footer class="embed-card__footer">
        <span class="embed-card__count"
          ><FontAwesomeIcon icon="reply" />{{
            t("status.repliesCount", { count: shown.replies_count })
          }}</span
        >
        <span class="embed-card__count"
          ><FontAwesomeIcon icon="retweet" />{{
            t("status.boostsCount", { count: shown.reblogs_count })
          }}</span
        >
        <span class="embed-card__count"
          ><FontAwesomeIcon icon="star" />{{
            t("status.likesCount", { count: shown.favourites_count })
          }}</span
        >
        <a
          class="embed-card__permalink"
          :href="permalink"
          target="_blank"
          rel="noopener"
          >{{ t("embed.viewPost") }}</a
        >
      </footer>
    </article>
  </main>
</template>

<style scoped>
/* The gutter around the card gives its border/shadow room to render;
   the page canvas itself is transparent (set in onMounted), so the
   host page's background shows through — a floating card, not a slab. */
.embed {
  font-size: var(--text-base);
  padding: var(--space-2);
}
.embed__loading {
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 4rem;
  color: var(--text-muted);
}
.embed__error {
  margin: 0;
  padding: var(--space-6) var(--space-4);
  text-align: center;
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.embed-card,
.embed__error {
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  box-shadow:
    0 1px 3px rgb(0 0 0 / 0.12),
    0 1px 2px rgb(0 0 0 / 0.06);
}
.embed-card {
  padding: var(--space-4);
}
.embed-card__boost {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  margin: 0 0 var(--space-2);
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.embed-card__header {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--space-3);
}
.embed-card__author {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  min-width: 0;
  color: inherit;
  text-decoration: none;
}
.embed-card__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.embed-card__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.embed-card__acct {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.embed-card__date {
  color: var(--text-muted);
  font-size: var(--text-sm);
  text-decoration: none;
  white-space: nowrap;
}
.embed-card__body {
  margin-top: var(--space-3);
}
.embed-card__content {
  overflow-wrap: break-word;
}
.embed-card__cw {
  margin-top: var(--space-3);
}
.embed-card__cw-summary {
  cursor: pointer;
  font-weight: 600;
}
.embed-card__poll {
  margin: var(--space-3) 0 0;
  padding-inline-start: var(--space-5);
  color: var(--text-muted);
}
.embed-card__votes {
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.embed-card__footer {
  display: flex;
  align-items: center;
  gap: var(--space-4);
  margin-top: var(--space-4);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border-dim);
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.embed-card__count {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
}
.embed-card__permalink {
  margin-inline-start: auto;
  color: var(--accent-link);
  text-decoration: none;
}
.embed-card__permalink:hover {
  text-decoration: underline;
}
</style>
