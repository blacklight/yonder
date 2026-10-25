<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import { useConversationsStore } from "@/stores/conversations";
import type { Conversation } from "@/api/entities";
import Avatar from "@/components/ui/Avatar.vue";
import Button from "@/components/ui/Button.vue";
import Modal from "@/components/ui/Modal.vue";
import { FontAwesomeIcon } from "@/icons";
import { sanitizeHtml } from "@/utils/sanitize";

const { t } = useI18n();
const router = useRouter();
const store = useConversationsStore();
const deleting = ref<Conversation | null>(null);

function names(c: Conversation): string {
  return c.accounts.map((a) => a.display_name || a.acct).join(", ");
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
    <h1>{{ t("messages.title") }}</h1>
    <ul class="conv-list">
      <li
        v-for="c in store.items"
        :key="c.id"
        class="conv"
        :class="{ unread: c.unread }"
        role="button"
        tabindex="0"
        @click="router.push(`/messages/${c.id}`)"
        @keydown.enter="router.push(`/messages/${c.id}`)"
      >
        <div class="conv-head">
          <span class="avatars">
            <Avatar
              v-for="a in c.accounts"
              :key="a.id"
              :src="a.avatar"
              :name="a.display_name || a.acct"
              size="sm"
            />
          </span>
          <strong>{{ names(c) }}</strong>
          <span
            v-if="c.unread"
            class="dot"
            :aria-label="t('messages.unread')"
          />
          <button
            class="del"
            :aria-label="t('messages.delete')"
            @click.stop="deleting = c"
          >
            <FontAwesomeIcon icon="trash-can" />
          </button>
        </div>
        <p
          v-if="c.last_status"
          class="preview"
          v-html="sanitizeHtml(c.last_status.content)"
        />
      </li>
    </ul>
    <p v-if="!store.loading && !store.items.length">
      {{ t("messages.empty") }}
    </p>
    <p v-if="store.error" role="alert">{{ store.error }}</p>
    <Button
      v-if="store.nextCursor"
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
