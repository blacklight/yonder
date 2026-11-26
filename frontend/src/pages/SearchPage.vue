<script setup lang="ts">
import { computed, ref, useTemplateRef, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import { useSearchStore } from "@/stores/search";
import { useAuthStore } from "@/stores/auth";
import { ApiError } from "@/api/errors";
import Avatar from "@/components/ui/Avatar.vue";
import EmojiText from "@/components/ui/EmojiText.vue";
import AccountRowSkeleton from "@/components/ui/AccountRowSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import Tabs from "@/components/ui/Tabs.vue";
import StatusCard from "@/components/status/StatusCard.vue";
import SearchSuggestions from "@/components/search/SearchSuggestions.vue";
import { useStatusEvents } from "@/composables/useStatusEvents";
import { dropFromStatusList, patchStatusList } from "@/utils/statusStream";

const { t } = useI18n();
const store = useSearchStore();
const auth = useAuthStore();
const route = useRoute();
const router = useRouter();
function routeQuery(): string {
  const value = route.query.q;
  if (Array.isArray(value)) return String(value[0] ?? "");
  return String(value ?? "");
}

const q = ref(routeQuery());
const tab = ref<"accounts" | "statuses" | "hashtags">("accounts");
const unauthorized = ref(false);
const suggestions =
  useTemplateRef<InstanceType<typeof SearchSuggestions>>("suggestions");

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
const activeEmpty = computed(() => {
  const r = store.results;
  if (!r || store.loading) return false;
  if (tab.value === "accounts") return r.accounts.length === 0;
  if (tab.value === "statuses") return r.statuses.length === 0;
  return r.hashtags.length === 0;
});

function onInput() {
  unauthorized.value = false;
  store.suggest(q.value, 300, "page");
}

function onSearchKeydown(event: KeyboardEvent) {
  const target = event.target;
  if (!(target instanceof HTMLInputElement) || target.type !== "search") return;
  if (event.key === "Enter") {
    if (event.isComposing) return;
    event.preventDefault();
    submit();
    return;
  }
  if (!["ArrowDown", "ArrowUp", "Tab"].includes(event.key)) return;
  const edge =
    event.key === "ArrowUp" || (event.key === "Tab" && event.shiftKey)
      ? "last"
      : "first";
  if (suggestions.value?.focusEdge(edge)) event.preventDefault();
}

async function performSearch() {
  const term = q.value.trim();
  if (!term) return;
  unauthorized.value = false;
  store.clearSuggestions();
  // One-shot remote resolve: only signed-in, only on explicit submit.
  const resolve = auth.isLoggedIn && isFqnOrUrl.value;
  try {
    await store.run(term, resolve);
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

function submit() {
  const term = q.value.trim();
  if (!term) return;
  q.value = term;
  if (term === routeQuery().trim()) void performSearch();
  else void router.push({ name: "search", query: { q: term } });
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

watch(
  () => route.query.q,
  () => {
    q.value = routeQuery();
    if (q.value.trim()) void performSearch();
    else {
      unauthorized.value = false;
      store.clearSuggestions();
      store.clearResults();
    }
  },
  { immediate: true },
);
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
          autofocus
          clearable
          :placeholder="t('nav.searchPlaceholder')"
          @update:model-value="onInput"
          @keydown="onSearchKeydown"
        />
        <Button type="submit" :loading="store.loading">
          {{ t("nav.search") }}
        </Button>
      </form>
      <SearchSuggestions ref="suggestions" owner="page" />
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
            <span class="search__display"
              ><EmojiText
                :text="a.display_name || a.username"
                :emojis="a.emojis"
            /></span>
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
