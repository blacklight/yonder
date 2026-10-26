<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";
import Avatar from "@/components/ui/Avatar.vue";
import Modal from "@/components/ui/Modal.vue";
import CwBlock from "@/components/status/CwBlock.vue";
import StatusMedia from "@/components/status/StatusMedia.vue";
import ComposeBox from "./ComposeBox.vue";
import { sanitizeHtml } from "@/utils/sanitize";
import { useComposeStore } from "@/stores/compose";
import type { Status } from "@/api/entities";

// Quoting opens the composer in a modal with the quoted post shown as
// a static card — links and actions would compete with the form.
const props = defineProps<{ status: Status }>();
const { t } = useI18n();
const compose = useComposeStore();
const html = computed(() => sanitizeHtml(props.status.content));
</script>

<template>
  <Modal :title="t('status.quote')" @close="compose.closeEmbed()">
    <div class="quote-compose__card">
      <header class="quote-compose__author">
        <Avatar
          :src="status.account.avatar"
          :name="status.account.display_name || status.account.username"
          size="sm"
        />
        <div class="quote-compose__names">
          <span class="quote-compose__display">{{
            status.account.display_name || status.account.username
          }}</span>
          <span class="quote-compose__acct">@{{ status.account.acct }}</span>
        </div>
      </header>
      <CwBlock :spoiler-text="status.spoiler_text">
        <!-- eslint-disable-next-line vue/no-v-html -->
        <div class="quote-compose__content" v-html="html" />
        <StatusMedia
          :media="status.media_attachments"
          :sensitive="status.sensitive && !status.spoiler_text"
        />
      </CwBlock>
    </div>
    <ComposeBox />
  </Modal>
</template>

<style scoped>
.quote-compose__card {
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-3);
  margin-bottom: var(--space-3);
}
.quote-compose__author {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  margin-bottom: var(--space-2);
}
.quote-compose__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.quote-compose__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.quote-compose__acct {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.quote-compose__content {
  overflow-wrap: break-word;
}
</style>
