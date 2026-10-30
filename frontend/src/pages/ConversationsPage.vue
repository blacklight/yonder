<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import { useConversationsStore } from "@/stores/conversations";
import { useComposeStore } from "@/stores/compose";
import type { Conversation } from "@/api/entities";
import Avatar from "@/components/ui/Avatar.vue";
import AccountRowSkeleton from "@/components/ui/AccountRowSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Modal from "@/components/ui/Modal.vue";
import Tooltip from "@/components/ui/Tooltip.vue";
import { FontAwesomeIcon } from "@/icons";
import { sanitizeHtml } from "@/utils/sanitize";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";

const { t } = useI18n();
const router = useRouter();
const store = useConversationsStore();
const compose = useComposeStore();
const deleting = ref<Conversation | null>(null);

function names(c: Conversation): string {
  return c.accounts.map((a) => a.display_name || a.acct).join(", ");
}

function open(c: Conversation) {
  void router.push(`/messages/${c.id}`);
}

/** "New message" is the `/compose` page with the draft seeded as a
 * direct message. An in-progress draft is never clobbered — resume
 * it instead of re-purposing it. */
function newMessage() {
  if (!compose.isDirty) compose.openDirect();
  void router.push("/compose");
}

async function confirmDelete() {
  const c = deleting.value;
  deleting.value = null;
  if (c) await store.remove(c.id);
}

onMounted(() => {
  store.start();
  if (!store.items.length) void store.fetchPage(true);
});
</script>

<template>
  <main class="page">
    <div class="page-head">
      <h1>{{ t("messages.title") }}</h1>
      <Button size="sm" data-testid="new-message" @click="newMessage">
        <FontAwesomeIcon icon="pen-to-square" />
        {{ t("messages.newMessage") }}
      </Button>
    </div>

    <ul
      v-if="store.items.length || store.loading"
      class="conv-list"
      :aria-busy="store.loading || undefined"
    >
      <li
        v-for="c in store.items"
        :key="c.id"
        class="conv"
        :class="{ unread: c.unread }"
        role="button"
        tabindex="0"
        @click="open(c)"
        @keydown.enter="open(c)"
        @keydown.space.prevent="open(c)"
      >
        <div class="conv-head">
          <span class="avatars">
            <Avatar
              v-for="a in c.accounts"
              :key="a.id"
              :src="a.avatar"
              :name="a.display_name || a.acct"
              size="md"
            />
          </span>
          <strong class="conv-names">{{ names(c) }}</strong>
          <span
            v-if="c.unread"
            class="dot"
            :aria-label="t('messages.unread')"
          />
          <Tooltip
            v-if="c.last_status"
            :text="formatAbsoluteDate(c.last_status.created_at)"
          >
            <time class="conv-when" :datetime="c.last_status.created_at">{{
              formatRelativeDate(c.last_status.created_at)
            }}</time>
          </Tooltip>
          <button
            class="del"
            :aria-label="t('messages.delete')"
            @click.stop="deleting = c"
            @keydown.enter.stop
            @keydown.space.stop
          >
            <FontAwesomeIcon icon="trash-can" />
          </button>
        </div>
        <div
          v-if="c.last_status"
          class="preview"
          v-html="
            sanitizeHtml(c.last_status.content, {
              mentions: c.last_status.mentions,
              tags: c.last_status.tags,
            })
          "
        />
      </li>
      <li
        v-for="n in store.loading && !store.items.length ? 3 : 0"
        :key="`skel-${n}`"
        class="conv conv--skel"
      >
        <AccountRowSkeleton size="md" />
      </li>
    </ul>

    <p v-if="!store.loading && !store.items.length" class="conv-state">
      {{ t("messages.empty") }}
    </p>
    <p v-if="store.error" class="conv-state" role="alert">{{ store.error }}</p>
    <Button
      v-if="store.nextCursor"
      class="load-more"
      :loading="store.loading"
      @click="store.fetchPage()"
    >
      {{ t("common.loadMore") }}
    </Button>
    <Modal
      v-if="deleting"
      :title="t('messages.deleteTitle')"
      @close="deleting = null"
    >
      <p>{{ t("messages.deleteBody") }}</p>
      <Button variant="danger" @click="confirmDelete">{{
        t("common.confirm")
      }}</Button>
    </Modal>
  </main>
</template>

<style scoped>
.page-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  margin-bottom: var(--space-4);
}
.page-head h1 {
  margin: 0;
}
/* Conversation rows mirror `.timeline > .status-card`: raised cards
   separated by dim borders inside a rounded container. */
.conv-list {
  list-style: none;
  margin: 0;
  padding: 0;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  overflow: hidden;
}
.conv {
  display: block;
  background: var(--bg-raised);
  padding: var(--space-3) var(--space-4);
  border-bottom: 1px solid var(--border-dim);
  cursor: pointer;
}
/* The skeleton brings its own row padding; it isn't clickable. */
.conv--skel {
  padding: 0 var(--space-1);
  cursor: default;
}
.conv:last-child {
  border-bottom: 0;
}
.conv:hover {
  background: var(--bg-sunken);
}
.conv-head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.avatars {
  display: flex;
  flex: none;
}
/* Group conversations stack avatars — the ring keeps each readable
   against the raised card behind them. */
.avatars :deep(.avatar) {
  box-shadow: 0 0 0 2px var(--bg-raised);
}
.avatars :deep(.avatar + .avatar) {
  margin-left: calc(-1 * var(--space-3));
}
.conv-names {
  flex: 1;
  min-width: 0;
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.dot {
  flex: none;
  width: 0.5rem;
  height: 0.5rem;
  border-radius: var(--radius-full);
  background: var(--accent);
}
.conv-when {
  flex: none;
  color: var(--text-muted);
  font-size: var(--text-sm);
  white-space: nowrap;
}
.del {
  flex: none;
  border: 0;
  background: none;
  color: var(--text-faint);
  padding: var(--space-2);
  border-radius: var(--radius-md);
  cursor: pointer;
}
.del:hover {
  color: var(--danger);
  background: var(--bg-sunken);
}
.preview {
  margin: var(--space-1) 0 0;
  color: var(--text-faint);
  font-size: var(--text-sm);
  overflow: hidden;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow-wrap: break-word;
}
.preview :deep(p) {
  margin: 0;
}
.conv.unread .preview {
  color: var(--text);
}
.conv-state {
  text-align: center;
  color: var(--text-muted);
  padding: var(--space-5) 0;
}
.load-more {
  margin-top: var(--space-3);
}
</style>
