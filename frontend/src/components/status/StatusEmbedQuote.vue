<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import Avatar from "@/components/ui/Avatar.vue";
import EmojiText from "@/components/ui/EmojiText.vue";
import Spinner from "@/components/ui/Spinner.vue";
import CwBlock from "./CwBlock.vue";
import StatusMedia from "./StatusMedia.vue";
import StatusEmbedCard from "./StatusEmbedCard.vue";
import { getStatus } from "@/api/statuses";
import { sanitizeHtml } from "@/utils/sanitize";
import { highlightCodeBlocks } from "@/utils/highlight";
import { formatAbsoluteDate } from "@/utils/relativeDate";
import type { Status } from "@/api/entities";

// Read-only quote block for the standalone embed page. Unlike
// StatusQuote (one level, interactive footer) this nests recursively:
// the backend materializes `quote.quoted_status` only at entity depth
// 0, so deeper levels are re-fetched by `quote_id` — each fetch yields
// that status with its own materialized quote, rebuilding the chain.
const props = withDefaults(defineProps<{ status: Status; depth?: number }>(), {
  depth: 0,
});
const { t } = useI18n();

/** Total nested quote cards rendered inside an embed. */
const MAX_DEPTH = 4;

const quote = computed(() => props.status.quote ?? null);
const target = ref<Status | null>(
  quote.value?.state === "accepted" ? quote.value.quoted_status : null,
);
const loading = ref(false);

// An `accepted` quote with no materialized status still carries the
// local id — it was depth-trimmed by the entity serializer, not denied.
const resolvable = computed(
  () => !target.value && props.depth < MAX_DEPTH && !!props.status.quote_id,
);
onMounted(async () => {
  if (!resolvable.value) return;
  loading.value = true;
  try {
    target.value = await getStatus(String(props.status.quote_id));
  } catch {
    // Deleted/unresolvable — falls through to the unavailable card.
  } finally {
    loading.value = false;
  }
});

const hasQuote = computed(
  () =>
    Boolean(quote.value) ||
    Boolean(props.status.yonder?.quote?.uri) ||
    Boolean(props.status.quote_id),
);
// Pending remote quotes carry only the AP uri — offer it as a link.
const remoteUri = computed(() => props.status.yonder?.quote?.uri ?? null);
const permalink = computed(() => target.value?.url ?? target.value?.uri ?? "");

// The federated `RE:` paragraph mirrors the quote card — strip it when
// the next level will render (materialized now or fetchable).
const RE_PARAGRAPH = /<p>\s*RE:\s*<a\b[^>]*>[\s\S]*?<\/a>\s*<\/p>\s*$/;
function nestedQuoteRenders(s: Status): boolean {
  if (s.quote?.quoted_status) return true;
  return (
    s.quote?.state === "accepted" && !!s.quote_id && props.depth + 1 < MAX_DEPTH
  );
}
const contentHtml = computed(() => {
  const t_ = target.value;
  if (!t_) return "";
  const raw = nestedQuoteRenders(t_)
    ? t_.content.replace(RE_PARAGRAPH, "")
    : t_.content;
  return highlightCodeBlocks(
    sanitizeHtml(raw, {
      mentions: t_.mentions,
      tags: t_.tags,
      emojis: t_.emojis,
    }),
  );
});

// Whole card is a link to the quoted post (new top-level tab — the
// page-level handler already covers real anchors; this covers the rest).
function open(event: MouseEvent) {
  const el = event.target as Element | null;
  if (el?.closest("a, button, input, textarea, select, label, audio, video"))
    return;
  if (permalink.value) window.open(permalink.value, "_blank", "noopener");
}
</script>

<template>
  <div
    v-if="target"
    class="embed-quote"
    data-testid="embed-quote"
    @click.stop="open"
  >
    <header class="embed-quote__author">
      <a :href="target.account.url" class="embed-quote__identity">
        <Avatar
          :src="target.account.avatar"
          :name="target.account.display_name || target.account.username"
          size="sm"
        />
        <span class="embed-quote__names">
          <span class="embed-quote__display"
            ><EmojiText
              :text="target.account.display_name || target.account.username"
              :emojis="target.account.emojis"
          /></span>
          <span class="embed-quote__acct">@{{ target.account.acct }}</span>
        </span>
      </a>
      <a
        class="embed-quote__date"
        :href="permalink"
        :datetime="target.created_at"
        >{{ formatAbsoluteDate(target.created_at) }}</a
      >
    </header>
    <CwBlock :spoiler-text="target.spoiler_text" :emojis="target.emojis">
      <!-- eslint-disable-next-line vue/no-v-html -->
      <div class="embed-quote__content" v-html="contentHtml" />
      <StatusMedia
        :media="target.media_attachments"
        :sensitive="target.sensitive && !target.spoiler_text"
      />
      <StatusEmbedCard :status="target" />
      <StatusEmbedQuote
        v-if="target.quote || target.quote_id"
        :status="target"
        :depth="depth + 1"
      />
    </CwBlock>
  </div>
  <div
    v-else-if="loading"
    class="embed-quote embed-quote--loading"
    role="status"
  >
    <Spinner size="sm" />
  </div>
  <a
    v-else-if="remoteUri"
    :href="remoteUri"
    class="embed-quote embed-quote--unavailable"
    >{{ t("status.quoteUnavailable") }}</a
  >
  <div v-else-if="hasQuote" class="embed-quote embed-quote--unavailable">
    {{ t("status.quoteUnavailable") }}
  </div>
</template>

<style scoped>
.embed-quote {
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-3);
  margin: var(--space-2) 0;
  cursor: pointer;
}
.embed-quote--unavailable {
  display: block;
  color: var(--text-muted);
  font-style: italic;
  cursor: default;
  text-decoration: none;
}
.embed-quote--loading {
  display: flex;
  justify-content: center;
  color: var(--text-muted);
  cursor: default;
}
.embed-quote__author {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  margin-bottom: var(--space-2);
}
.embed-quote__identity {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  min-width: 0;
  color: inherit;
  text-decoration: none;
}
.embed-quote__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
  line-height: 1.2;
}
.embed-quote__display {
  font-weight: 600;
  font-size: var(--text-sm);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.embed-quote__acct {
  font-size: var(--text-xs);
  color: var(--text-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.embed-quote__date {
  margin-inline-start: auto;
  flex-shrink: 0;
  font-size: var(--text-xs);
  color: var(--text-muted);
  text-decoration: none;
}
.embed-quote__content {
  font-size: var(--text-sm);
  overflow-wrap: break-word;
}
</style>
