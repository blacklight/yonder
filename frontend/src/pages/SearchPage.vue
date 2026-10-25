<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import { useSearchStore } from "@/stores/search";
import { useAuthStore } from "@/stores/auth";
import { ApiError } from "@/api/errors";
import Avatar from "@/components/ui/Avatar.vue";
import Input from "@/components/ui/Input.vue";
import StatusCard from "@/components/status/StatusCard.vue";

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
      ["accounts", t("search.users")],
      ["statuses", t("search.posts")],
      ["hashtags", t("search.hashtags")],
    ] as const,
);
const isFqnOrUrl = computed(() =>
  /^https?:\/\//.test(q.value.trim()) || /^@?[\w.]+@[\w.-]+\.[a-zA-Z]{2,}$/.test(q.value.trim()),
);

function onInput() {
  unauthorized.value = false;
  store.suggest(q.value);
}

async function submit() {
  unauthorized.value = false;
  // One-shot remote resolve: only signed-in, only on explicit submit.
  const resolve = auth.isLoggedIn && isFqnOrUrl.value;
  try {
    await store.run(q.value.trim(), resolve);
    tab.value = "accounts";
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

onMounted(() => {
  if (q.value.trim()) void submit();
});
</script>

<template>
  <main class="page">
    <form role="search" @submit.prevent="submit">
      <Input
        v-model="q"
        :label="t('nav.search')"
        :placeholder="t('nav.searchPlaceholder')"
        @update:model-value="onInput"
      />
    </form>
    <ul v-if="store.suggestions" class="suggestions" role="listbox">
      <li
        v-for="a in store.suggestions.accounts"
        :key="a.id"
        role="option"
        tabindex="0"
        @click="goAccount(a.acct)"
        @keydown.enter="goAccount(a.acct)"
      >
        <Avatar :src="a.avatar" :name="a.display_name || a.acct" size="sm" /> @{{ a.acct }}
      </li>
      <li
        v-for="h in store.suggestions.hashtags"
        :key="h.name"
        role="option"
        tabindex="0"
        @click="goTag(h.name)"
        @keydown.enter="goTag(h.name)"
      >
        #{{ h.name }}
      </li>
    </ul>
    <p v-if="unauthorized" role="alert">{{ t("search.signInRequired") }}</p>
    <p v-else-if="store.error" role="alert">{{ store.error }}</p>
    <template v-if="store.results">
      <nav class="tabs" aria-label="search result types">
        <button
          v-for="[key, label] in tabs"
          :key="key"
          :aria-pressed="tab === key"
          @click="tab = key"
        >
          {{ label }}
        </button>
      </nav>
      <ul v-if="tab === 'accounts'">
        <li
          v-for="a in store.results.accounts"
          :key="a.id"
          role="button"
          tabindex="0"
          @click="goAccount(a.acct)"
          @keydown.enter="goAccount(a.acct)"
        >
          <Avatar :src="a.avatar" :name="a.display_name || a.acct" size="sm" />
          {{ a.display_name || a.username }} @{{ a.acct }}
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
      <ul v-if="tab === 'hashtags'">
        <li
          v-for="h in store.results.hashtags"
          :key="h.name"
          role="button"
          tabindex="0"
          @click="goTag(h.name)"
          @keydown.enter="goTag(h.name)"
        >
          #{{ h.name }}
        </li>
      </ul>
      <p v-if="isFqnOrUrl && !auth.isLoggedIn" class="hint">{{ t("search.remoteHint") }}</p>
    </template>
  </main>
</template>
