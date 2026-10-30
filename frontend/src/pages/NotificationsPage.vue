<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import { I18nT, useI18n } from "vue-i18n";
import {
  useNotificationsStore,
  notificationTypeKey,
  notificationIcon,
  FILTERS,
  type NotificationFilter,
} from "@/stores/notifications";
import { useAuthStore } from "@/stores/auth";
import {
  authorizeFollowRequest,
  rejectFollowRequest,
} from "@/api/followRequests";
import type { Account, Notification } from "@/api/entities";
import Avatar from "@/components/ui/Avatar.vue";
import AccountRowSkeleton from "@/components/ui/AccountRowSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Modal from "@/components/ui/Modal.vue";
import Tabs from "@/components/ui/Tabs.vue";
import Tooltip from "@/components/ui/Tooltip.vue";
import StatusCard from "@/components/status/StatusCard.vue";
import { FontAwesomeIcon } from "@/icons";
import { ApiError } from "@/api/errors";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";
import { useStatusEvents } from "@/composables/useStatusEvents";
import { patchStatus, statusMatchesId } from "@/utils/statusStream";

const { t } = useI18n();
const store = useNotificationsStore();
const auth = useAuthStore();
const confirming = ref(false);

// Statuses embedded in notifications stay live: an edit refreshes
// the row; a delete drops the notification that referenced it
// (there is nothing left to act on).
useStatusEvents({
  onStatus: (s) => {
    store.items = store.items.map((n) =>
      n.status ? { ...n, status: patchStatus(n.status, s) } : n,
    );
  },
  onDelete: (id) => {
    store.items = store.items.filter(
      (n) => !n.status || !statusMatchesId(n.status, id),
    );
  },
});

// Computed, not constant — `auth.isStaff` can arrive after mount via
// `auth.restore()`, and the reports tab must appear when it does.
const tabs = computed(() =>
  (
    Object.keys(FILTERS).filter(
      (f) => f !== "reports" || auth.isStaff,
    ) as NotificationFilter[]
  ).map((f) => ({ key: f, label: t(`notifications.filter.${f}`) })),
);
const actionError = ref<string | null>(null);
const acting = ref<Set<string>>(new Set());

function displayName(a: Account | null): string {
  return a?.display_name || a?.acct || "";
}

function reportLink(n: Notification): string | null {
  return n.type === "admin.report" && n.report?.id
    ? `/moderation/reports/${n.report.id}`
    : null;
}

async function act(n: Notification, action: "authorize" | "reject") {
  if (acting.value.has(n.id)) return;
  actionError.value = null;
  acting.value = new Set(acting.value).add(n.id);
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
  } finally {
    const done = new Set(acting.value);
    done.delete(n.id);
    acting.value = done;
  }
  await store.dismiss(n.id);
}

async function clearAll() {
  confirming.value = false;
  await store.clear();
}

const listEl = ref<HTMLElement | null>(null);
let observer: IntersectionObserver | null = null;
const pendingRead = new Set<string>();
let readTimer: ReturnType<typeof setTimeout> | null = null;

function flushRead() {
  readTimer = null;
  if (!pendingRead.size) return;
  const ids = [...pendingRead];
  pendingRead.clear();
  void store.markRead(ids);
}

/** Observe (or bulk-mark, without IO support) the unread rows. */
function observeRows() {
  const list = listEl.value;
  if (!list) return;
  const unread = new Set(store.items.filter((n) => !n.read).map((n) => n.id));
  if (observer) {
    for (const el of list.querySelectorAll<HTMLElement>("[data-notif-id]")) {
      if (unread.has(el.dataset.notifId ?? "")) observer.observe(el);
    }
  } else if (unread.size) {
    // Engines without IntersectionObserver treat the loaded list as
    // fully rendered.
    void store.markRead([...unread]);
  }
}

watch(
  () => store.items.length,
  async () => {
    await nextTick();
    observeRows();
  },
);

onMounted(() => {
  if ("IntersectionObserver" in window) {
    observer = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (!e.isIntersecting) continue;
          const id = (e.target as HTMLElement).dataset.notifId;
          if (!id) continue;
          observer?.unobserve(e.target);
          pendingRead.add(id);
          readTimer ??= setTimeout(flushRead, 300);
        }
      },
      { threshold: 0.5 },
    );
  }
  void store.fetchPage(true);
  // Rows may already be loaded (back-navigation) — the watcher only
  // fires on length changes, so seed the observation pass here.
  observeRows();
});

onUnmounted(() => {
  observer?.disconnect();
  if (readTimer) clearTimeout(readTimer);
});
</script>

<template>
  <main class="page">
    <h1>{{ t("nav.notifications") }}</h1>
    <Tabs
      :model-value="store.activeFilter"
      :items="tabs"
      :label="t('notifications.filters')"
      @update:model-value="store.setFilter"
    >
      <Button variant="secondary" size="sm" @click="confirming = true">
        {{ t("notifications.clear") }}
      </Button>
    </Tabs>
    <ul ref="listEl" class="notif-list" :aria-busy="store.loading || undefined">
      <li v-for="n in store.items" :key="n.id" class="notif" role="article">
        <div class="notif-head" :data-notif-id="n.id">
          <span
            v-if="!n.read"
            class="notif-dot"
            :aria-label="t('messages.unread')"
          />
          <FontAwesomeIcon
            :icon="notificationIcon(n)"
            class="notif-icon"
            fixed-width
            aria-hidden="true"
          />
          <router-link
            v-if="n.account"
            :to="`/@${n.account.acct}`"
            class="notif-avatar"
            ><Avatar
              :src="n.account.avatar"
              :name="displayName(n.account)"
              size="sm"
          /></router-link>
          <div class="notif-body">
            <p class="notif-label">
              <I18nT :keypath="notificationTypeKey(n)" tag="span">
                <template #name>
                  <router-link
                    v-if="n.account"
                    :to="`/@${n.account.acct}`"
                    class="notif-name"
                    >{{ displayName(n.account) }}</router-link
                  >
                </template>
              </I18nT>
              <Tooltip :text="formatAbsoluteDate(n.created_at)">
                <router-link
                  v-if="n.status"
                  :to="`/statuses/${n.status.id}`"
                  class="notif-when"
                  >&nbsp;·
                  <time :datetime="n.created_at">{{
                    formatRelativeDate(n.created_at)
                  }}</time></router-link
                >
                <time v-else class="notif-when" :datetime="n.created_at"
                  >&nbsp;· {{ formatRelativeDate(n.created_at) }}</time
                >
              </Tooltip>
            </p>
            <p v-if="n.account" class="notif-acct">@{{ n.account.acct }}</p>
            <router-link
              v-if="reportLink(n)"
              :to="reportLink(n)!"
              class="notif-report"
              >{{ t("notifications.viewReport") }}</router-link
            >
          </div>
          <button
            class="dismiss"
            :aria-label="t('notifications.dismiss')"
            @click="store.dismiss(n.id)"
          >
            <FontAwesomeIcon icon="xmark" />
          </button>
        </div>
        <StatusCard v-if="n.status" :status="n.status" />
        <div v-if="n.type === 'follow_request' && n.account" class="fr-actions">
          <Button
            size="sm"
            :loading="acting.has(n.id)"
            @click="act(n, 'authorize')"
            >{{ t("notifications.accept") }}</Button
          >
          <Button
            size="sm"
            variant="secondary"
            :disabled="acting.has(n.id)"
            @click="act(n, 'reject')"
          >
            {{ t("notifications.reject") }}
          </Button>
        </div>
      </li>
      <li
        v-for="n in store.loading && !store.items.length ? 4 : 0"
        :key="`skel-${n}`"
        class="notif notif--skel"
      >
        <AccountRowSkeleton />
      </li>
    </ul>
    <p v-if="actionError" role="alert">{{ actionError }}</p>
    <p v-if="!store.loading && !store.items.length">{{ t("lists.empty") }}</p>
    <p v-if="store.error" role="alert">{{ store.error }}</p>
    <Button
      v-if="store.nextCursor"
      :loading="store.loading"
      @click="store.fetchPage()"
    >
      {{ t("common.loadMore") }}
    </Button>
    <Modal
      v-if="confirming"
      :title="t('notifications.clearTitle')"
      @close="confirming = false"
    >
      <p>{{ t("notifications.clearBody") }}</p>
      <Button variant="danger" @click="clearAll">{{
        t("common.confirm")
      }}</Button>
    </Modal>
  </main>
</template>

<style scoped>
.notif-list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.notif {
  background: var(--bg-raised);
  margin-bottom: var(--space-3);
  padding: var(--space-3) var(--space-4);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
}
/* The skeleton brings its own row padding — the card chrome stays. */
.notif--skel {
  padding: 0 var(--space-1);
}
.notif-head {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.notif-icon {
  color: var(--accent-link);
  flex: none;
}
.notif-dot {
  flex: none;
  width: 0.5rem;
  height: 0.5rem;
  border-radius: var(--radius-full);
  background: var(--accent);
}
.notif-avatar {
  flex: none;
}
.notif-body {
  flex: 1;
  min-width: 0;
}
.notif-label {
  margin: 0;
  overflow-wrap: break-word;
}
.notif-name {
  color: var(--text);
  font-weight: 600;
  text-decoration: none;
}
.notif-name:hover {
  color: var(--accent-link);
}
.notif-when {
  color: var(--text-muted);
  font-size: var(--text-sm);
  text-decoration: none;
  white-space: nowrap;
}
a.notif-when:hover {
  text-decoration: underline;
}
.notif-acct {
  margin: 0;
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.notif-report {
  font-size: var(--text-sm);
}
.dismiss {
  flex: none;
  background: none;
  border: 0;
  color: var(--text-muted);
  cursor: pointer;
  padding: var(--space-1);
  border-radius: var(--radius-sm);
}
.dismiss:hover {
  color: var(--text);
  background: var(--bg-hover, var(--bg));
}
.fr-actions {
  display: flex;
  gap: var(--space-2);
  margin-top: var(--space-2);
}
/* The embedded post keeps its own card chrome but drops the doubled
   padding/border the notification row already provides. */
.notif :deep(.status-card) {
  font-size: var(--text-sm);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  margin-top: var(--space-2);
  padding: var(--space-2);
}
</style>
