<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import StatusCard from "@/components/status/StatusCard.vue";
import StatusCardSkeleton from "@/components/status/StatusCardSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Spinner from "@/components/ui/Spinner.vue";
import {
  listEvents,
  cursorOf,
  FEP_CATEGORIES,
  type EventsScope,
} from "@/api/events";
import type { Status } from "@/api/entities";
import { useAuthStore } from "@/stores/auth";
import { useInfiniteScroll } from "@/composables/useInfiniteScroll";

const { t } = useI18n();
const auth = useAuthStore();

type Tab = "home" | "local" | "federated";
// The home tab needs an authenticated viewer; anonymous visitors get
// local/federated only.
const tabs = computed<{ id: Tab; label: string }[]>(() => {
  const all: { id: Tab; label: string }[] = [
    { id: "home", label: t("eventsDiscover.tabHome") },
    { id: "local", label: t("eventsDiscover.tabLocal") },
    { id: "federated", label: t("eventsDiscover.tabFederated") },
  ];
  return auth.isFull ? all : all.slice(1);
});
// Signed-in viewers land on Upcoming (home); anonymous visitors have
// no home tab and start on Local.
const tab = ref<Tab>(auth.isFull ? "home" : "local");

const items = ref<Status[]>([]);
const loading = ref(false);
const loaded = ref(false);
const error = ref<string | null>(null);
const nextCursor = ref<string | undefined>();

// Filters — category (FEP list, mirrors the backend), hashtag,
// online-only, and opt-in "near me" via browser geolocation.
const category = ref("");
const tag = ref("");
const online = ref(false);
const near = ref<string | null>(null);
const nearBusy = ref(false);
const nearError = ref(false);

const CATEGORIES = FEP_CATEGORIES;

// The tag input refilters on Enter and on blur — but only when the
// value actually changed since the last applied filter.
const appliedTag = ref("");

// Each request gets a sequence number; a response only lands if it
// is still the latest one, so a tab switch or filter change during
// an in-flight page can't render the old scope's items.
let requestSeq = 0;

async function load(reset = false) {
  // Paging waits for the page in flight; a reset always supersedes it.
  if (!reset && loading.value) return;
  const seq = ++requestSeq;
  loading.value = true;
  error.value = null;
  try {
    const page = await listEvents({
      scope: tab.value as EventsScope,
      limit: 20,
      cursor: reset ? undefined : nextCursor.value,
      category: category.value || undefined,
      tag: tag.value.trim() || undefined,
      online: online.value || undefined,
      near: near.value ?? undefined,
    });
    if (seq !== requestSeq) return;
    items.value = reset ? page.items : [...items.value, ...page.items];
    nextCursor.value = cursorOf(page.next);
    appliedTag.value = tag.value.trim();
    loaded.value = true;
  } catch {
    if (seq !== requestSeq) return;
    // Drop the cursor so the infinite-scroll sentinel can't keep
    // re-triggering into the same failure. Retry restarts page one
    // (`load(true)`) and replaces the list, so a failure mid-paging
    // can't duplicate the first page.
    nextCursor.value = undefined;
    error.value = t("common.error");
  } finally {
    if (seq === requestSeq) loading.value = false;
  }
}

function switchTab(id: Tab) {
  if (tab.value === id) return;
  tab.value = id;
  items.value = [];
  nextCursor.value = undefined;
  loaded.value = false;
  void load(true);
}

function refilter() {
  items.value = [];
  nextCursor.value = undefined;
  loaded.value = false;
  void load(true);
}

/** Opt-in browser geolocation → `near=lat,lon`. */
function locate() {
  if (!navigator.geolocation) {
    nearError.value = true;
    return;
  }
  nearBusy.value = true;
  nearError.value = false;
  navigator.geolocation.getCurrentPosition(
    (pos) => {
      near.value = `${pos.coords.latitude.toFixed(5)},${pos.coords.longitude.toFixed(5)}`;
      nearBusy.value = false;
      refilter();
    },
    () => {
      nearBusy.value = false;
      nearError.value = true;
    },
    { timeout: 10_000 },
  );
}

function clearNear() {
  near.value = null;
  refilter();
}

const hasMore = () => !!nextCursor.value;
const { vInfinite, ioSupported } = useInfiniteScroll(hasMore, () => load());

onMounted(() => {
  void load(true);
});
</script>

<template>
  <div class="discover">
    <header class="discover__header">
      <h1>{{ t("eventsDiscover.title") }}</h1>
      <router-link v-if="auth.isFull" to="/events" class="discover__link">
        {{ t("eventsDiscover.myEvents") }}
      </router-link>
    </header>

    <nav class="discover__tabs" role="tablist">
      <button
        v-for="tb in tabs"
        :key="tb.id"
        role="tab"
        :aria-selected="tab === tb.id"
        class="discover__tab"
        :class="{ 'discover__tab--active': tab === tb.id }"
        @click="switchTab(tb.id)"
      >
        {{ tb.label }}
      </button>
    </nav>

    <section class="discover__filters">
      <select v-model="category" @change="refilter">
        <option value="">{{ t("eventsDiscover.allCategories") }}</option>
        <option v-for="c in CATEGORIES" :key="c" :value="c">{{ c }}</option>
      </select>
      <input
        v-model="tag"
        type="search"
        :placeholder="t('eventsDiscover.tagPlaceholder')"
        @keydown.enter="refilter"
        @blur="tag.trim() !== appliedTag && refilter()"
      />
      <label class="discover__toggle">
        <input v-model="online" type="checkbox" @change="refilter" />
        {{ t("eventsDiscover.online") }}
      </label>
      <template v-if="near">
        <span class="discover__near">{{ t("eventsDiscover.nearActive") }}</span>
        <Button size="sm" variant="secondary" @click="clearNear">
          {{ t("eventsDiscover.nearClear") }}
        </Button>
      </template>
      <Button v-else size="sm" :loading="nearBusy" @click="locate">
        {{ t("eventsDiscover.nearMe") }}
      </Button>
      <p v-if="nearError" class="discover__state">
        {{ t("eventsDiscover.nearFailed") }}
      </p>
    </section>

    <div
      v-if="loading && !loaded"
      role="status"
      :aria-label="t('common.loading')"
    >
      <StatusCardSkeleton v-for="n in 3" :key="n" />
    </div>
    <div v-else-if="error" class="discover__state">
      <p>{{ error }}</p>
      <Button size="sm" @click="load(true)">{{ t("timeline.retry") }}</Button>
    </div>
    <p v-else-if="loaded && items.length === 0" class="discover__state">
      {{ t("eventsDiscover.empty") }}
    </p>

    <StatusCard
      v-for="s in items"
      :key="s.id"
      :status="s"
      @removed="(id) => (items = items.filter((i) => i.id !== id))"
    />

    <div v-infinite class="discover__sentinel" aria-hidden="true" />
    <div v-if="loading && loaded" class="discover__state">
      <Spinner />
    </div>
    <Button v-if="!ioSupported && hasMore()" :loading="loading" @click="load()">
      {{ t("common.loadMore") }}
    </Button>
  </div>
</template>

<style scoped>
.discover {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
  padding: 1rem;
}
.discover__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.discover__header h1 {
  font-size: 1.25rem;
  margin: 0;
}
.discover__link {
  font-size: 0.85rem;
}
.discover__tabs {
  display: flex;
  gap: 0.25rem;
  flex-wrap: wrap;
}
.discover__tab {
  padding: 0.35rem 0.75rem;
  border: 1px solid var(--color-border, #ccc);
  border-radius: 999px;
  background: transparent;
  cursor: pointer;
}
.discover__tab--active {
  background: var(--color-accent, #356);
  color: #fff;
  border-color: transparent;
}
.discover__filters {
  display: flex;
  gap: 0.5rem;
  flex-wrap: wrap;
  align-items: center;
}
.discover__filters select,
.discover__filters input[type="search"] {
  padding: 0.35rem 0.5rem;
  border: 1px solid var(--color-border, #ccc);
  border-radius: 0.35rem;
}
.discover__toggle {
  display: flex;
  gap: 0.4rem;
  align-items: center;
  font-size: 0.85rem;
}
.discover__near {
  font-size: 0.85rem;
  color: var(--color-text-muted, #888);
}
.discover__state {
  padding: 1rem;
  text-align: center;
  color: var(--color-text-muted, #888);
}
.discover__sentinel {
  height: 1px;
}
</style>
