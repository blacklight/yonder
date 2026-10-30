<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import { useSearchStore } from "@/stores/search";
import { useAuthStore } from "@/stores/auth";
import { ApiError } from "@/api/errors";
import Avatar from "@/components/ui/Avatar.vue";
import AccountRowSkeleton from "@/components/ui/AccountRowSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import Tabs from "@/components/ui/Tabs.vue";
import StatusCard from "@/components/status/StatusCard.vue";
import { useStatusEvents } from "@/composables/useStatusEvents";
import { dropFromStatusList, patchStatusList } from "@/utils/statusStream";

const { t } = useI18n();
const store = useSearchStore();
const auth = useAuthStore();
const route = useRoute();
const router = useRouter();
const q = ref(String(route.query.q ?? ""));
const tab = ref<"accounts" | "statuses" | "hashtags">("accounts");
const unauthorized = ref(false);

const tabs = computed(
  () =>
    [
      { key: "accounts", label: t("search.users") },
      { key: "statuses", label: t("search.posts") },
      { key: "hashtags", label: t("search.hashtags") },
    ] as const,
);
const isFqnOrUrl = computed(
  () =>
    /^https?:\/\//.test(q.value.trim()) ||
    /^@?[\w.]+@[\w.-]+\.[a-zA-Z]{2,}$/.test(q.value.trim()),
);
const suggestionAccounts = computed(() => store.suggestions?.accounts ?? []);
const suggestionHashtags = computed(() => store.suggestions?.hashtags ?? []);
const hasSuggestions = computed(
  () => suggestionAccounts.value.length + suggestionHashtags.value.length > 0,
);
const activeEmpty = computed(() => {
  const r = store.results;
  if (!r || store.loading) return false;
  if (tab.value === "accounts") return r.accounts.length === 0;
  if (tab.value === "statuses") return r.statuses.length === 0;
  return r.hashtags.length === 0;
});

function onInput() {
  unauthorized.value = false;
  store.suggest(q.value);
}

async function submit() {
  if (!q.value.trim()) return;
  unauthorized.value = false;
  store.clearSuggestions();
  // One-shot remote resolve: only signed-in, only on explicit submit.
  const resolve = auth.isLoggedIn && isFqnOrUrl.value;
  try {
    await store.run(q.value.trim(), resolve);
    // Land on the first tab that actually has hits.
    const r = store.results;
    if (r?.accounts.length) tab.value = "accounts";
    else if (r?.statuses.length) tab.value = "statuses";
    else if (r?.hashtags.length) tab.value = "hashtags";
    else tab.value = "accounts";
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) unauthorized.value = true;
  }
}

function goAccount(acct: string) {
  router.push(`/@${acct}`);
}
function goTag(name: string) {
  router.push(`/tags/${encodeURIComponent(name)}`);
}
function goStatus(id: string) {
  router.push(`/statuses/${id}`);
}

// Status results stay live: edits replace the row, deletes drop it.
useStatusEvents({
  onStatus: (s) => {
    const r = store.results;
    if (r) r.statuses = patchStatusList(r.statuses, s);
  },
  onDelete: (id) => {
    const r = store.results;
    if (r) r.statuses = dropFromStatusList(r.statuses, id);
  },
});

onMounted(() => {
  if (q.value.trim()) void submit();
});
</script>

<template>
  <main class="page">
    <div class="search__box">
      <form class="search__form" role="search" @submit.prevent="submit">
        <Input
          v-model="q"
          class="search__input"
          :label="t('nav.search')"
          type="search"
          autocomplete="off"
          clearable
          :placeholder="t('nav.searchPlaceholder')"
          @update:model-value="onInput"
        />
        <Button type="submit" :loading="store.loading">
          {{ t("nav.search") }}
        </Button>
      </form>
      <ul v-if="hasSuggestions" class="search__suggestions" role="listbox">
        <li
          v-for="a in suggestionAccounts"
          :key="a.id"
          class="search__suggestion"
          role="option"
          aria-selected="false"
          tabindex="0"
          @click="goAccount(a.acct)"
          @keydown.enter="goAccount(a.acct)"
        >
          <Avatar :src="a.avatar" :name="a.display_name || a.acct" size="sm" />
          <span class="search__names">
            <span class="search__display">
              {{ a.display_name || a.username }}
            </span>
            <span class="search__acct">@{{ a.acct }}</span>
          </span>
        </li>
        <li
          v-for="h in suggestionHashtags"
          :key="h.name"
          class="search__suggestion"
          role="option"
          aria-selected="false"
          tabindex="0"
          @click="goTag(h.name)"
          @keydown.enter="goTag(h.name)"
        >
          <span class="search__tag">#{{ h.name }}</span>
        </li>
      </ul>
    </div>
    <p v-if="unauthorized" class="search__alert" role="alert">
      {{ t("search.signInRequired") }}
    </p>
    <p v-else-if="store.error" class="search__alert" role="alert">
      {{ store.error }}
    </p>
    <ul
      v-if="store.loading && !store.results"
      class="search__results"
      role="status"
      :aria-label="t('common.loading')"
    >
      <li v-for="n in 4" :key="n" class="search__result">
        <AccountRowSkeleton class="search__skel" />
      </li>
    </ul>
    <template v-if="store.results">
      <Tabs v-model="tab" :items="tabs" :label="t('search.resultTypes')" />
      <ul v-if="tab === 'accounts'" class="search__results">
        <li
          v-for="a in store.results.accounts"
          :key="a.id"
          class="search__result"
          role="button"
          tabindex="0"
          @click="goAccount(a.acct)"
          @keydown.enter="goAccount(a.acct)"
        >
          <Avatar :src="a.avatar" :name="a.display_name || a.acct" size="sm" />
          <span class="search__names">
            <span class="search__display">
              {{ a.display_name || a.username }}
            </span>
            <span class="search__acct">@{{ a.acct }}</span>
          </span>
        </li>
      </ul>
      <template v-if="tab === 'statuses'">
        <StatusCard
          v-for="s in store.results.statuses"
          :key="s.id"
          :status="s"
          @click="goStatus(s.id)"
        />
      </template>
      <ul v-if="tab === 'hashtags'" class="search__results">
        <li
          v-for="h in store.results.hashtags"
          :key="h.name"
          class="search__result"
          role="button"
          tabindex="0"
          @click="goTag(h.name)"
          @keydown.enter="goTag(h.name)"
        >
          <span class="search__tag">#{{ h.name }}</span>
        </li>
      </ul>
      <p v-if="activeEmpty" class="search__state">{{ t("lists.empty") }}</p>
      <p v-if="isFqnOrUrl && !auth.isLoggedIn" class="search__hint">
        {{ t("search.remoteHint") }}
      </p>
    </template>
  </main>
</template>

<style scoped>
.search__box {
  position: relative;
}
.search__form {
  display: flex;
  align-items: flex-end;
  gap: var(--space-2);
}
.search__input {
  flex: 1;
  min-width: 0;
}
.search__suggestions {
  position: absolute;
  top: 100%;
  left: 0;
  right: 0;
  z-index: 20;
  margin: var(--space-1) 0 0;
  padding: var(--space-2) 0;
  list-style: none;
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  box-shadow: 0 4px 12px rgb(0 0 0 / 0.12);
  max-height: 20rem;
  overflow-y: auto;
}
.search__suggestion {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  cursor: pointer;
}
.search__suggestion:hover {
  background: var(--bg-hover, var(--bg-sunken));
}
.search__results {
  list-style: none;
  margin: 0;
  padding: 0;
}
.search__result {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3);
  border-bottom: 1px solid var(--border);
  cursor: pointer;
}
.search__result:hover {
  background: var(--bg-hover, var(--bg-sunken));
}
/* Skeleton fills the row — the result li's own padding would double it. */
.search__skel {
  flex: 1;
  min-width: 0;
  margin: calc(-1 * var(--space-3));
}
.search__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.search__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.search__acct {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.search__tag {
  font-weight: 600;
}
.search__state {
  margin: 0;
  padding: var(--space-5) 0;
  text-align: center;
  color: var(--text-muted);
}
.search__alert {
  margin: var(--space-3) 0 0;
  color: var(--danger);
}
.search__hint {
  margin: var(--space-3) 0 0;
  color: var(--text-muted);
  font-size: var(--text-sm);
}
</style>
