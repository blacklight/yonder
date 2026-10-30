<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import type { Status } from "@/api/entities";
import { sanitizeHtml } from "@/utils/sanitize";
import { formatRelativeDate } from "@/utils/relativeDate";
import { react, unreact } from "@/api/statuses";
import { useAuthStore } from "@/stores/auth";
import Avatar from "@/components/ui/Avatar.vue";
import StatusFooter from "./StatusFooter.vue";
import ReactionPicker from "./ReactionPicker.vue";

const props = defineProps<{ status: Status }>();
const emit = defineEmits<{
  reply: [status: Status];
  quote: [status: Status];
}>();
const { t } = useI18n();
const router = useRouter();
const auth = useAuthStore();

// Rendered one level deep — the quote block never nests another card.
// `quote.quoted_status` is null when the target couldn't be resolved
// (deleted, blocked, remote-only, or consent still `pending`).
const quote = props.status.quote;
const quoted = quote?.state === "accepted" ? quote.quoted_status : null;
const unavailable =
  !quoted &&
  Boolean(quote || props.status.yonder?.quote?.uri || props.status.quote_id);

const pickerOpen = ref(false);
const actionError = ref("");

const contentHtml = computed(() =>
  quoted
    ? sanitizeHtml(quoted.content, {
        mentions: quoted.mentions,
        tags: quoted.tags,
      })
    : "",
);

// Click through to the quoted post — interactive elements (links,
// buttons, the footer row) keep their own behaviour.
function open(event: MouseEvent) {
  const el = event.target as Element;
  if (
    el instanceof Element &&
    el.closest("a, button, input, textarea, select, label, .menu, audio, video")
  ) {
    return;
  }
  if (quoted) void router.push(`/statuses/${quoted.id}`);
}

async function onReact(emoji: string) {
  pickerOpen.value = false;
  if (!quoted || !auth.isLoggedIn) return;
  try {
    const reactions = quoted.pleroma?.emoji_reactions ?? [];
    const existing = reactions.find(
      (r) => r.name === emoji || r.name === `:${emoji}:`,
    );
    if (existing?.me) await unreact(quoted.id, emoji);
    else await react(quoted.id, emoji);
  } catch (e) {
    actionError.value = e instanceof Error ? e.message : String(e);
  }
}
</script>

<template>
  <div v-if="quoted" class="quote" data-testid="quote" @click.stop="open">
    <header class="quote__author">
      <router-link :to="`/@${quoted.account.acct}`" class="quote__identity">
        <Avatar
          :src="quoted.account.avatar"
          :name="quoted.account.display_name || quoted.account.username"
          size="sm"
        />
        <div class="quote__names">
          <span class="quote__display">{{
            quoted.account.display_name || quoted.account.username
          }}</span>
          <span class="quote__acct">@{{ quoted.account.acct }}</span>
        </div>
      </router-link>
      <time class="quote__date" :datetime="quoted.created_at">{{
        formatRelativeDate(quoted.created_at)
      }}</time>
    </header>
    <!-- eslint-disable-next-line vue/no-v-html -->
    <div class="quote__content" v-html="contentHtml" />
    <p v-if="actionError" class="quote__error" role="alert">
      {{ actionError }}
    </p>
    <div class="quote__footer">
      <StatusFooter
        :status="quoted"
        @reply="(s) => emit('reply', s)"
        @quote="(s) => emit('quote', s)"
        @react="pickerOpen = !pickerOpen"
      />
      <ReactionPicker v-if="pickerOpen" @select="onReact" />
    </div>
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
  cursor: pointer;
}
.quote--unavailable {
  color: var(--text-muted);
  font-style: italic;
  cursor: default;
}
.quote__author {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  margin-bottom: var(--space-2);
}
.quote__identity {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  min-width: 0;
  color: inherit;
  text-decoration: none;
}
.quote__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
  line-height: 1.2;
}
.quote__display {
  font-weight: 600;
  font-size: var(--text-sm);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.quote__acct {
  font-size: var(--text-xs);
  color: var(--text-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.quote__date {
  margin-inline-start: auto;
  flex-shrink: 0;
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.quote__content {
  font-size: var(--text-sm);
  overflow-wrap: break-word;
}
.quote__footer {
  margin-top: var(--space-1);
  position: relative;
}
.quote__footer :deep(.status-footer) {
  padding-top: 0;
  border-top: none;
}
.quote__error {
  color: var(--danger);
  font-size: var(--text-sm);
  margin: var(--space-1) 0 0;
}
</style>
