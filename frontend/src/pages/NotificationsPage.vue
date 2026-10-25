<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useNotificationsStore, FILTERS, type NotificationFilter } from "@/stores/notifications";
import { useAuthStore } from "@/stores/auth";
import { authorizeFollowRequest, rejectFollowRequest } from "@/api/followRequests";
import type { Notification } from "@/api/entities";
import Button from "@/components/ui/Button.vue";
import Modal from "@/components/ui/Modal.vue";
import StatusCard from "@/components/status/StatusCard.vue";
import { ApiError } from "@/api/errors";

const { t } = useI18n();
const store = useNotificationsStore();
const auth = useAuthStore();
const confirming = ref(false);

// Computed, not constant — `auth.isStaff` can arrive after mount via
// `auth.restore()`, and the reports tab must appear when it does.
const tabs = computed(
  () =>
    Object.keys(FILTERS).filter(
      (f) => f !== "reports" || auth.isStaff,
    ) as NotificationFilter[],
);
const actionError = ref<string | null>(null);

function label(n: Notification): string {
  const key = `notifications.type.${n.type.replace(".", "_")}`;
  return t(key, { acct: n.account?.acct ?? "" }) === key
    ? t("notifications.type.unknown", { acct: n.account?.acct ?? "" })
    : t(key, { acct: n.account?.acct ?? "" });
}

async function act(n: Notification, action: "authorize" | "reject") {
  actionError.value = null;
  try {
    if (!n.account) return;
    if (action === "authorize") await authorizeFollowRequest(n.account.id);
    else await rejectFollowRequest(n.account.id);
  } catch (e) {
    // 404 = already handled elsewhere — still dismiss the row.
    if (!(e instanceof ApiError && e.status === 404)) {
      actionError.value = t("common.error");
      return;
    }
  }
  await store.dismiss(n.id);
}

async function clearAll() {
  confirming.value = false;
  await store.clear();
}

onMounted(() => store.fetchPage(true));
</script>

<template>
  <main class="page">
    <h1>{{ t("nav.notifications") }}</h1>
    <nav class="tabs" aria-label="notification filters">
      <button
        v-for="f in tabs"
        :key="f"
        :aria-pressed="store.activeFilter === f"
        @click="store.setFilter(f)"
      >
        {{ t(`notifications.filter.${f}`) }}
      </button>
      <Button variant="secondary" size="sm" @click="confirming = true">
        {{ t("notifications.clear") }}
      </Button>
    </nav>
    <ul class="notif-list">
      <li v-for="n in store.items" :key="n.id" class="notif" role="article">
        <p class="notif-label">{{ label(n) }}</p>
        <StatusCard v-if="n.status" :status="n.status" />
        <div v-if="n.type === 'follow_request' && n.account" class="fr-actions">
          <Button size="sm" @click="act(n, 'authorize')">{{ t("notifications.accept") }}</Button>
          <Button size="sm" variant="secondary" @click="act(n, 'reject')">
            {{ t("notifications.reject") }}
          </Button>
        </div>
        <button class="dismiss" :aria-label="t('notifications.dismiss')" @click="store.dismiss(n.id)">
          ×
        </button>
      </li>
    </ul>
    <p v-if="actionError" role="alert">{{ actionError }}</p>
    <p v-if="!store.loading && !store.items.length">{{ t("lists.empty") }}</p>
    <p v-if="store.error" role="alert">{{ store.error }}</p>
    <Button v-if="store.nextCursor" :loading="store.loading" @click="store.fetchPage()">
      {{ t("common.loadMore") }}
    </Button>
    <Modal v-if="confirming" :title="t('notifications.clearTitle')" @close="confirming = false">
      <p>{{ t("notifications.clearBody") }}</p>
      <Button variant="danger" @click="clearAll">{{ t("common.confirm") }}</Button>
    </Modal>
  </main>
</template>
