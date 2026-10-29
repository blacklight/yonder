<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useConversationsStore } from "@/stores/conversations";
import { useComposeStore } from "@/stores/compose";
import { ApiError } from "@/api/errors";
import type { Status } from "@/api/entities";
import StatusCard from "@/components/status/StatusCard.vue";
import ComposeBox from "@/components/compose/ComposeBox.vue";
import Button from "@/components/ui/Button.vue";
import Modal from "@/components/ui/Modal.vue";

const props = defineProps<{ id: string }>();
const { t } = useI18n();
const store = useConversationsStore();
const compose = useComposeStore();
const notFound = ref(false);
const loadError = ref(false);
// A user-typed draft from another context — held behind a confirm
// before the new conversation's seed replaces it.
const pendingReseed = ref<Status | null>(null);

const validKey = computed(() => /^[0-9a-f]{64}$/.test(props.id));
const newest = computed(() => store.activeThread?.statuses[0]);

/** Seed the composer for this conversation. A reply draft already
 * targeting this thread resumes untouched; an empty draft or an
 * untouched seed from a previous context is replaced; typed text
 * from another context is kept behind a discard confirm. */
function seedComposer(latest: Status) {
  const sameThread =
    compose.draft.inReplyToId !== null &&
    store.activeThread?.statuses.some(
      (s) => s.id === compose.draft.inReplyToId,
    );
  if (sameThread) return;
  if (!compose.isDirty || compose.isUntouchedSeed) {
    compose.openDirectReply(latest);
  } else {
    pendingReseed.value = latest;
  }
}

function confirmReseed() {
  const s = pendingReseed.value;
  pendingReseed.value = null;
  if (s) compose.openDirectReply(s);
}

async function openConversation() {
  notFound.value = false;
  loadError.value = false;
  if (!validKey.value) {
    notFound.value = true;
    return;
  }
  try {
    await store.openThread(props.id);
    if (newest.value) seedComposer(newest.value);
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) notFound.value = true;
    else loadError.value = true;
  }
}

onMounted(openConversation);
// The route is unkeyed — `/messages/A` → `/messages/B` reuses this
// component, so the param change re-opens the thread and re-seeds.
watch(() => props.id, openConversation);
</script>

<template>
  <main class="page">
    <p v-if="notFound">{{ t("messages.notFound") }}</p>
    <p v-else-if="loadError" role="alert">{{ t("common.error") }}</p>
    <template v-else-if="store.activeThread">
      <ComposeBox direct />
      <StatusCard
        v-for="s in store.activeThread.statuses"
        :key="s.id"
        :status="s"
        direct-reply
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

    <Modal
      v-if="pendingReseed"
      :title="t('status.confirm.title')"
      @close="pendingReseed = null"
    >
      <p>{{ t("compose.discardText") }}</p>
      <div class="confirm-btns">
        <Button variant="secondary" @click="pendingReseed = null">
          {{ t("ui.cancel") }}
        </Button>
        <Button
          variant="danger"
          data-testid="reseed-confirm"
          @click="confirmReseed"
        >
          {{ t("ui.confirm") }}
        </Button>
      </div>
    </Modal>
  </main>
</template>

<style scoped>
.confirm-btns {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-2);
  margin-top: var(--space-3);
}
</style>
