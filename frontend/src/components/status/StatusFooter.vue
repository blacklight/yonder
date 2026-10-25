<script setup lang="ts">
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import {
  bookmark,
  favourite,
  reblog,
  unbookmark,
  unfavourite,
  unreblog,
} from "@/api/statuses";
import { ApiError } from "@/api/errors";
import { useAuthStore } from "@/stores/auth";
import type { Status } from "@/api/entities";

const props = defineProps<{ status: Status }>();
const emit = defineEmits<{
  reply: [status: Status];
  quote: [status: Status];
  react: [status: Status];
}>();
const { t } = useI18n();
const auth = useAuthStore();
const busy = ref(false);

// Optimistic toggle: flip the flag/counter locally, roll back on
// ApiError. `st` aliases the timeline store's reactive entity, so
// writes through it update every card sharing the row.
async function toggle(
  flag: "favourited" | "reblogged" | "bookmarked",
  // Mastodon has no bookmarks_count — `null` flips the flag only.
  count: "favourites_count" | "reblogs_count" | null,
  on: (id: string) => Promise<unknown>,
  off: (id: string) => Promise<unknown>,
) {
  if (busy.value) return;
  busy.value = true;
  const st = props.status;
  const was = st[flag];
  st[flag] = !was;
  if (count !== null) {
    st[count] += was ? -1 : 1;
  }
  try {
    await (was ? off : on)(st.id);
  } catch (e) {
    if (e instanceof ApiError) {
      st[flag] = was;
      if (count !== null) {
        st[count] += was ? 1 : -1;
      }
    } else {
      throw e;
    }
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <footer class="status-footer" :aria-label="t('status.actions')">
    <button
      type="button"
      class="status-footer__btn"
      :aria-label="t('status.reply')"
      @click="emit('reply', status)"
    >
      ↩ <span class="status-footer__count">{{ status.replies_count }}</span>
    </button>
    <button
      type="button"
      class="status-footer__btn"
      :class="{ 'status-footer__btn--active': status.favourited }"
      :aria-pressed="status.favourited"
      :aria-label="t('status.like')"
      :disabled="!auth.isLoggedIn"
      data-testid="like"
      @click="toggle('favourited', 'favourites_count', favourite, unfavourite)"
    >
      ♥ <span class="status-footer__count">{{ status.favourites_count }}</span>
    </button>
    <button
      type="button"
      class="status-footer__btn"
      :class="{ 'status-footer__btn--active': status.reblogged }"
      :aria-pressed="status.reblogged"
      :aria-label="t('status.boost')"
      :disabled="!auth.isLoggedIn"
      data-testid="boost"
      @click="toggle('reblogged', 'reblogs_count', reblog, unreblog)"
    >
      ↻ <span class="status-footer__count">{{ status.reblogs_count }}</span>
    </button>
    <button
      type="button"
      class="status-footer__btn"
      :aria-label="t('status.quote')"
      :disabled="!auth.isLoggedIn"
      @click="emit('quote', status)"
    >
      ❝ <span class="status-footer__count">{{ status.quotes_count }}</span>
    </button>
    <button
      type="button"
      class="status-footer__btn"
      :aria-label="t('status.react')"
      :disabled="!auth.isLoggedIn"
      data-testid="react"
      @click="emit('react', status)"
    >
      ☺
    </button>
    <button
      type="button"
      class="status-footer__btn"
      :class="{ 'status-footer__btn--active': status.bookmarked }"
      :aria-pressed="status.bookmarked"
      :aria-label="t('status.bookmark')"
      :disabled="!auth.isLoggedIn"
      data-testid="bookmark"
      @click="toggle('bookmarked', null, bookmark, unbookmark)"
    >
      🔖
    </button>
  </footer>
</template>

<style scoped>
.status-footer {
  display: flex;
  gap: var(--space-3);
  padding-top: var(--space-2);
  margin-top: var(--space-2);
  border-top: 1px solid var(--border);
}
.status-footer__btn {
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
  border: 0;
  background: none;
  color: var(--text-muted);
  cursor: pointer;
  font: inherit;
  padding: var(--space-1);
  border-radius: var(--radius-sm);
}
.status-footer__btn:hover {
  color: var(--text);
  background: var(--bg-sunken);
}
.status-footer__btn--active {
  color: var(--accent);
}
.status-footer__btn:disabled {
  opacity: 0.45;
  cursor: default;
}
.status-footer__count {
  font-size: var(--text-sm);
}
</style>
