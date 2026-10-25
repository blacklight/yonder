<script setup lang="ts">
import { useI18n } from "vue-i18n";
import type { Status } from "@/api/entities";
import { sanitizeHtml } from "@/utils/sanitize";

const props = defineProps<{ status: Status }>();
const { t } = useI18n();

// Rendered one level deep — the quote block never nests another card.
// `yonder.quote.uri` without a materialized `quote` means the target
// couldn't be fetched (deleted, blocked, remote-only).
const quoted = props.status.quote ?? null;
const unavailable =
  !quoted && Boolean(props.status.yonder?.quote?.uri || props.status.quote_id);
</script>

<template>
  <div v-if="quoted" class="quote" data-testid="quote">
    <div class="quote__author">@{{ quoted.account.acct }}</div>
    <!-- eslint-disable-next-line vue/no-v-html -->
    <div class="quote__content" v-html="sanitizeHtml(quoted.content)" />
  </div>
  <div v-else-if="unavailable" class="quote quote--unavailable">
    {{ t("status.quoteUnavailable") }}
  </div>
</template>

<style scoped>
.quote {
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-3);
  margin: var(--space-2) 0;
}
.quote--unavailable {
  color: var(--text-muted);
  font-style: italic;
}
.quote__author {
  font-weight: 600;
  font-size: var(--text-sm);
  color: var(--text-muted);
  margin-bottom: var(--space-1);
}
</style>
