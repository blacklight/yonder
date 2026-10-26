<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import { followers, following, lookup } from "@/api/accounts";
import { ApiError } from "@/api/errors";
import type { Account } from "@/api/entities";
import Avatar from "@/components/ui/Avatar.vue";
import Button from "@/components/ui/Button.vue";
import Tabs from "@/components/ui/Tabs.vue";

const props = defineProps<{ direction: "followers" | "following" }>();
const { t } = useI18n();
const route = useRoute();
const router = useRouter();

const account = ref<Account | null>(null);
const members = ref<Account[]>([]);
const nextCursor = ref<string | undefined>();
const state = ref<"loading" | "ok" | "not-found" | "sign-in" | "error">(
  "loading",
);

const tabs = computed(
  () =>
    [
      { key: "followers", label: t("route.profile-followers.title") },
      { key: "following", label: t("route.profile-following.title") },
    ] as const,
);
const titleKey = computed(() => `route.profile-${props.direction}.title`);

// Route reuse + the mount-time watcher can overlap `load()` calls —
// the generation counter drops stale completions so an older fetch
// can't overwrite a newer one.
let generation = 0;

async function loadMembers(more = false, gen = generation) {
  if (!account.value) return;
  const max_id =
    more && nextCursor.value
      ? (new URL(nextCursor.value, location.origin).searchParams.get(
          "max_id",
        ) ?? undefined)
      : undefined;
  const fetchPage = props.direction === "followers" ? followers : following;
  const page = await fetchPage(account.value.id, { max_id });
  if (gen !== generation) return;
  members.value = more ? [...members.value, ...page.items] : page.items;
  nextCursor.value = page.next;
}

async function load() {
  const gen = ++generation;
  state.value = "loading";
  members.value = [];
  nextCursor.value = undefined;
  try {
    const a = await lookup(String(route.params.acct));
    if (gen !== generation) return;
    account.value = a;
    state.value = "ok";
    await loadMembers(false, gen);
  } catch (e) {
    if (gen !== generation) return;
    if (e instanceof ApiError && e.status === 401) state.value = "sign-in";
    else if (e instanceof ApiError && (e.status === 404 || e.status === 422))
      state.value = "not-found";
    else state.value = "error";
  }
}

function onTab(key: "followers" | "following") {
  if (key !== props.direction) {
    router.push(`/@${route.params.acct}/${key}`);
  }
}

watch([account, state], () => {
  const app = t("app.name");
  const a = account.value;
  if (state.value === "ok" && a) {
    const name = a.display_name || a.username;
    document.title = `${t(titleKey.value)} — ${name} (@${a.acct}) — ${app}`;
  } else if (state.value !== "loading") {
    document.title = `${t(titleKey.value)} — ${app}`;
  }
});

// Both siblings reuse this component, and profile links can swap the
// `acct` param — reload on either change.
watch([() => route.fullPath, () => props.direction], load, {
  immediate: true,
});
</script>

<template>
  <main class="page network">
    <p v-if="state === 'loading'" aria-busy="true">{{ t("common.loading") }}</p>
    <p v-else-if="state === 'not-found'">{{ t("profile.notFound") }}</p>
    <p v-else-if="state === 'error'" role="alert">
      {{ t("profile.loadError") }}
    </p>
    <div v-else-if="state === 'sign-in'" class="network__signin">
      <p>{{ t("profile.signInToView") }}</p>
      <router-link to="/login">{{ t("nav.signIn") }}</router-link>
    </div>
    <template v-else-if="account">
      <header class="network__header">
        <router-link :to="`/@${account.acct}`" class="network__owner">
          <Avatar
            :src="account.avatar"
            :name="account.display_name || account.acct"
            size="sm"
          />
          <span class="network__names">
            <span class="network__display">
              {{ account.display_name || account.username }}
            </span>
            <span class="network__acct">@{{ account.acct }}</span>
          </span>
        </router-link>
      </header>
      <Tabs
        :model-value="direction"
        :items="tabs"
        :label="t('profile.networkTabs')"
        @update:model-value="onTab"
      />
      <ul class="network__list">
        <li v-for="a in members" :key="a.id">
          <router-link :to="`/@${a.acct}`" class="network__row">
            <Avatar
              :src="a.avatar"
              :name="a.display_name || a.acct"
              size="sm"
            />
            <span class="network__names">
              <span class="network__display">
                {{ a.display_name || a.username }}
              </span>
              <span class="network__acct">@{{ a.acct }}</span>
            </span>
          </router-link>
        </li>
      </ul>
      <p v-if="!members.length" class="network__empty">
        {{ t("lists.empty") }}
      </p>
      <div v-if="nextCursor" class="network__more">
        <Button variant="secondary" @click="loadMembers(true)">
          {{ t("common.loadMore") }}
        </Button>
      </div>
    </template>
  </main>
</template>

<style scoped>
.network__header {
  margin-bottom: var(--space-2);
}
.network__owner {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  color: var(--text);
  text-decoration: none;
}
.network__list {
  list-style: none;
  margin: 0;
  padding: 0;
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  overflow: hidden;
}
.network__list li + li {
  border-top: 1px solid var(--border);
}
.network__row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3);
  color: var(--text);
  text-decoration: none;
}
.network__row:hover {
  background: var(--bg-hover, var(--bg-sunken));
}
.network__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.network__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.network__acct {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.network__empty {
  margin: 0;
  padding: var(--space-5) 0;
  text-align: center;
  color: var(--text-muted);
}
.network__more {
  margin-top: var(--space-4);
  text-align: center;
}
.network__signin {
  padding: var(--space-6) 0;
  text-align: center;
}
</style>
