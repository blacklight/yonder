<script setup lang="ts">
// Who interacted with a status — boosters and likers as account
// rows, quotes as full cards. The three tabs share one route family
// (`/statuses/:id/{boosts,quotes,favourites}`), mirroring the
// followers/following page.
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import {
  favouritedBy,
  getStatus,
  rebloggedBy,
  statusQuotes,
} from "@/api/statuses";
import { ApiError } from "@/api/errors";
import type { Account, Status } from "@/api/entities";
import Avatar from "@/components/ui/Avatar.vue";
import AccountRowSkeleton from "@/components/ui/AccountRowSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Tabs from "@/components/ui/Tabs.vue";
import StatusCard from "@/components/status/StatusCard.vue";
import StatusCardSkeleton from "@/components/status/StatusCardSkeleton.vue";
import { useStatusEvents } from "@/composables/useStatusEvents";
import {
  dropFromStatusList,
  patchStatus,
  patchStatusList,
  statusMatchesId,
} from "@/utils/statusStream";

type Kind = "boosts" | "quotes" | "likes";
const props = defineProps<{ id: string; kind: Kind }>();
const { t } = useI18n();
const router = useRouter();

const status = ref<Status | null>(null);
const accounts = ref<Account[]>([]);
const quotes = ref<Status[]>([]);
const listLoading = ref(false);
const nextCursor = ref<string | undefined>();
const state = ref<"loading" | "ok" | "not-found" | "error">("loading");

const KIND_PATH: Record<Kind, string> = {
  boosts: "boosts",
  quotes: "quotes",
  likes: "favourites",
};
const KINDS = ["boosts", "quotes", "likes"] as const;
const tabs = computed(() =>
  KINDS.map((key) => ({ key, label: t(`status.tabs.${key}`) })),
);

function onTab(key: Kind) {
  if (key !== props.kind) {
    router.push(`/statuses/${props.id}/${KIND_PATH[key]}`);
  }
}

// Route reuse + the mount-time watcher can overlap `load()` calls —
// the generation counter drops stale completions so an older fetch
// can't overwrite a newer one.
let generation = 0;

async function loadList(more = false, gen = generation) {
  const max_id =
    more && nextCursor.value
      ? (new URL(nextCursor.value, location.origin).searchParams.get(
          "max_id",
        ) ?? undefined)
      : undefined;
  listLoading.value = true;
  try {
    if (props.kind === "quotes") {
      const page = await statusQuotes(props.id, { max_id });
      if (gen !== generation) return;
      quotes.value = more ? [...quotes.value, ...page.items] : page.items;
      nextCursor.value = page.next;
    } else {
      const fetcher = props.kind === "boosts" ? rebloggedBy : favouritedBy;
      const page = await fetcher(props.id, { max_id });
      if (gen !== generation) return;
      accounts.value = more ? [...accounts.value, ...page.items] : page.items;
      nextCursor.value = page.next;
    }
  } finally {
    if (gen === generation) listLoading.value = false;
  }
}

async function load() {
  const gen = ++generation;
  state.value = "loading";
  accounts.value = [];
  quotes.value = [];
  nextCursor.value = undefined;
  try {
    const s = await getStatus(props.id);
    if (gen !== generation) return;
    status.value = s;
    state.value = "ok";
    await loadList(false, gen);
  } catch (e) {
    if (gen !== generation) return;
    state.value =
      e instanceof ApiError && e.status === 404 ? "not-found" : "error";
  }
}

function removed(id: string) {
  quotes.value = dropFromStatusList(quotes.value, id);
}

useStatusEvents({
  onStatus: (s) => {
    if (status.value && statusMatchesId(status.value, s.id))
      status.value = patchStatus(status.value, s);
    quotes.value = patchStatusList(quotes.value, s);
  },
  onDelete: (id) => {
    // The interacted-with status itself is gone — same as the 404.
    if (status.value && statusMatchesId(status.value, id))
      state.value = "not-found";
    removed(id);
  },
});

watch(() => [props.id, props.kind], load, { immediate: true });
</script>

<template>
  <main class="page">
    <ul
      v-if="state === 'loading'"
      class="interactions__list"
      role="status"
      :aria-label="t('common.loading')"
    >
      <li v-for="n in 6" :key="n"><AccountRowSkeleton /></li>
    </ul>
    <p v-else-if="state === 'not-found'">{{ t("status.notFound") }}</p>
    <p v-else-if="state === 'error'" role="alert">
      {{ t("status.loadError") }}
    </p>
    <template v-else-if="status">
      <header class="interactions__header">
        <router-link :to="`/statuses/${status.id}`" class="interactions__post">
          <Avatar
            :src="status.account.avatar"
            :name="status.account.display_name || status.account.acct"
            size="sm"
          />
          <span class="interactions__names">
            <span class="interactions__display">
              {{ status.account.display_name || status.account.username }}
            </span>
            <span class="interactions__acct">@{{ status.account.acct }}</span>
          </span>
        </router-link>
      </header>
      <Tabs
        :model-value="kind"
        :items="tabs"
        :label="t('status.interactionsTabs')"
        @update:model-value="onTab"
      />
      <template v-if="kind === 'quotes'">
        <StatusCard
          v-for="s in quotes"
          :key="s.id"
          :status="s"
          @removed="removed"
        />
        <template v-if="listLoading">
          <StatusCardSkeleton v-for="n in 2" :key="`skel-${n}`" />
        </template>
        <p v-if="!quotes.length && !listLoading" class="interactions__empty">
          {{ t("lists.empty") }}
        </p>
      </template>
      <template v-else>
        <ul
          v-if="accounts.length || listLoading"
          class="interactions__list"
          :aria-busy="listLoading || undefined"
        >
          <li v-for="a in accounts" :key="a.id">
            <router-link :to="`/@${a.acct}`" class="interactions__row">
              <Avatar
                :src="a.avatar"
                :name="a.display_name || a.acct"
                size="sm"
              />
              <span class="interactions__names">
                <span class="interactions__display">
                  {{ a.display_name || a.username }}
                </span>
                <span class="interactions__acct">@{{ a.acct }}</span>
              </span>
            </router-link>
          </li>
          <li v-for="n in listLoading ? 3 : 0" :key="`skel-${n}`">
            <AccountRowSkeleton />
          </li>
        </ul>
        <p v-if="!accounts.length && !listLoading" class="interactions__empty">
          {{ t("lists.empty") }}
        </p>
      </template>
      <div v-if="nextCursor" class="interactions__more">
        <Button
          variant="secondary"
          :loading="listLoading"
          @click="loadList(true)"
        >
          {{ t("common.loadMore") }}
        </Button>
      </div>
    </template>
  </main>
</template>

<style scoped>
.interactions__header {
  margin-bottom: var(--space-2);
}
.interactions__post {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  color: var(--text);
  text-decoration: none;
}
.interactions__list {
  list-style: none;
  margin: 0;
  padding: 0;
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  overflow: hidden;
}
.interactions__list li + li {
  border-top: 1px solid var(--border);
}
.interactions__row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3);
  color: var(--text);
  text-decoration: none;
}
.interactions__row:hover {
  background: var(--bg-hover, var(--bg-sunken));
}
.interactions__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.interactions__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.interactions__acct {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.interactions__empty {
  margin: 0;
  padding: var(--space-5) 0;
  text-align: center;
  color: var(--text-muted);
}
.interactions__more {
  margin-top: var(--space-4);
  text-align: center;
}
</style>
