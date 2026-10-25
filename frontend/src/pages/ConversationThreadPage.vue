<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useConversationsStore } from "@/stores/conversations";
import { useComposeStore } from "@/stores/compose";
import { ApiError } from "@/api/errors";
import StatusCard from "@/components/status/StatusCard.vue";
import ComposeBox from "@/components/compose/ComposeBox.vue";
import Button from "@/components/ui/Button.vue";

const props = defineProps<{ id: string }>();
const { t } = useI18n();
const store = useConversationsStore();
const compose = useComposeStore();
const notFound = ref(false);
const loadError = ref(false);

const validKey = computed(() => /^[0-9a-f]{64}$/.test(props.id));
const newest = computed(() => store.activeThread?.statuses[0]);

onMounted(async () => {
  if (!validKey.value) {
    notFound.value = true;
    return;
  }
  try {
    await store.openThread(props.id);
    // Reply to the *newest* thread status — the server carries the
    // mention set forward, keeping the conversation key stable. Only
    // seed an empty draft: reopening a thread must not discard text
    // the user already typed.
    if (newest.value && !compose.draft.text.trim())
      compose.openDirectReply(newest.value);
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) notFound.value = true;
    else loadError.value = true;
  }
});
</script>

<template>
  <main class="page">
    <p v-if="notFound">{{ t("messages.notFound") }}</p>
    <p v-else-if="loadError" role="alert">{{ t("common.error") }}</p>
    <template v-else-if="store.activeThread">
      <ComposeBox />
      <StatusCard
        v-for="s in store.activeThread.statuses"
        :key="s.id"
        :status="s"
      />
      <Button
        v-if="store.activeThread.next"
        variant="secondary"
        @click="store.fetchOlder()"
      >
        {{ t("common.loadMore") }}
      </Button>
    </template>
    <p v-else aria-busy="true">{{ t("common.loading") }}</p>
  </main>
</template>
