<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import StatusCard from "@/components/status/StatusCard.vue";
import StatusCardSkeleton from "@/components/status/StatusCardSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Spinner from "@/components/ui/Spinner.vue";
import {
  calendarTokenCreate,
  calendarTokenRevoke,
  listEvents,
  type EventsScope,
} from "@/api/events";
import type { Status } from "@/api/entities";
import { useComposeStore } from "@/stores/compose";
import { useInfiniteScroll } from "@/composables/useInfiniteScroll";

const { t } = useI18n();
const router = useRouter();
const compose = useComposeStore();

type Tab = "organizing" | "going" | "pending" | "invited" | "all";
const tab = ref<Tab>("organizing");
const items = ref<Status[]>([]);
const loading = ref(false);
const loaded = ref(false);
const error = ref<string | null>(null);
const nextCursor = ref<string | undefined>();
const includePast = ref(false);

const scopeFor = computed<EventsScope>(() =>
  tab.value === "organizing" ? "mine" : "participating",
);
const stateFor = computed<string | undefined>(() => {
  switch (tab.value) {
    case "going":
      return "going";
    case "pending":
      return "pending";
    case "invited":
      return "invited";
    default:
      return undefined;
  }
});

function cursorOf(url?: string) {
  if (!url) return undefined;
  return new URL(url, location.origin).searchParams.get("cursor") ?? undefined;
}

async function load(reset = false) {
  if (loading.value) return;
  loading.value = true;
  error.value = null;
  try {
    const page = await listEvents({
      scope: scopeFor.value,
      state: stateFor.value,
      limit: 20,
      cursor: reset ? undefined : nextCursor.value,
      include_cancelled: true,
      // The API's window defaults to now → +90d; widening `from`
      // pulls in past events the viewer still participates in.
      from: includePast.value ? "2000-01-01T00:00:00Z" : undefined,
    });
    items.value = reset ? page.items : [...items.value, ...page.items];
    nextCursor.value = cursorOf(page.next);
    loaded.value = true;
  } catch {
    error.value = t("common.error");
  } finally {
    loading.value = false;
  }
}

function switchTab(t: Tab) {
  if (tab.value === t) return;
  tab.value = t;
  items.value = [];
  nextCursor.value = undefined;
  loaded.value = false;
  void load(true);
}

const hasMore = () => !!nextCursor.value;
const { vInfinite, ioSupported } = useInfiniteScroll(hasMore, () => load());

onMounted(() => load(true));

const tabs: { id: Tab; label: string }[] = [
  { id: "organizing", label: "events.tabOrganizing" },
  { id: "going", label: "events.tabGoing" },
  { id: "pending", label: "events.tabPending" },
  { id: "invited", label: "events.tabInvited" },
  { id: "all", label: "events.tabAll" },
];

function newEvent() {
  compose.openEvent();
  void router.push("/compose");
}

// Personalized calendar subscription (Section 18) — the token URL is
// only returned at mint/rotate time; afterwards the UI offers
// Copy/Regenerate/Revoke.
const calendarUrl = ref<string | null>(null);
const calendarBusy = ref(false);
const calendarError = ref(false);
const copied = ref(false);

async function mintCalendar() {
  calendarBusy.value = true;
  calendarError.value = false;
  try {
    const res = await calendarTokenCreate();
    calendarUrl.value = res.calendar_url;
    copied.value = false;
  } catch {
    calendarError.value = true;
  } finally {
    calendarBusy.value = false;
  }
}

async function revokeCalendar() {
  calendarBusy.value = true;
  calendarError.value = false;
  try {
    await calendarTokenRevoke();
    calendarUrl.value = null;
  } catch {
    calendarError.value = true;
  } finally {
    calendarBusy.value = false;
  }
}

async function copyCalendar() {
  if (!calendarUrl.value) return;
  try {
    await navigator.clipboard.writeText(calendarUrl.value);
    copied.value = true;
  } catch {
    // Clipboard API unavailable (non-secure context) — select fallback.
    copied.value = false;
  }
}
</script>

<template>
  <div class="events-page">
    <header class="events-page__header">
      <h1>{{ t("events.title") }}</h1>
      <Button size="sm" @click="newEvent">
        {{ t("events.newEvent") }}
      </Button>
    </header>

    <nav class="events-page__tabs" role="tablist">
      <button
        v-for="tb in tabs"
        :key="tb.id"
        role="tab"
        :aria-selected="tab === tb.id"
        class="events-page__tab"
        :class="{ 'events-page__tab--active': tab === tb.id }"
        @click="switchTab(tb.id)"
      >
        {{ t(tb.label) }}
      </button>
    </nav>

    <label class="events-page__toggle">
      <input v-model="includePast" type="checkbox" @change="load(true)" />
      {{ t("events.includePast") }}
    </label>

    <section class="events-page__calendar">
      <h2>{{ t("events.calendarTitle") }}</h2>
      <p class="events-page__hint">{{ t("events.calendarHint") }}</p>
      <template v-if="calendarUrl">
        <code class="events-page__url">{{ calendarUrl }}</code>
        <div class="events-page__calendar-actions">
          <Button size="sm" @click="copyCalendar">
            {{ copied ? t("events.calendarCopied") : t("events.calendarCopy") }}
          </Button>
          <Button size="sm" variant="secondary" :loading="calendarBusy" @click="mintCalendar">
            {{ t("events.calendarRegenerate") }}
          </Button>
          <Button size="sm" variant="secondary" :loading="calendarBusy" @click="revokeCalendar">
            {{ t("events.calendarRevoke") }}
          </Button>
        </div>
      </template>
      <Button v-else size="sm" :loading="calendarBusy" @click="mintCalendar">
        {{ t("events.calendarCreate") }}
      </Button>
      <p v-if="calendarError" class="events-page__state">
        {{ t("common.error") }}
      </p>
    </section>

    <div v-if="loading && !loaded" role="status" :aria-label="t('common.loading')">
      <StatusCardSkeleton v-for="n in 3" :key="n" />
    </div>
    <div v-else-if="error" class="events-page__state">
      <p>{{ error }}</p>
      <Button size="sm" @click="load()">{{ t("timeline.retry") }}</Button>
    </div>
    <p v-else-if="loaded && items.length === 0" class="events-page__state">
      {{ t("events.empty") }}
    </p>

    <StatusCard
      v-for="s in items"
      :key="s.id"
      :status="s"
      @removed="(id) => (items = items.filter((i) => i.id !== id))"
    />

    <div v-infinite class="events-page__sentinel" aria-hidden="true" />
    <div v-if="loading && loaded" class="events-page__state">
      <Spinner />
    </div>
    <Button
      v-if="!ioSupported && hasMore()"
      :loading="loading"
      @click="load()"
    >
      {{ t("common.loadMore") }}
    </Button>
  </div>
</template>

<style scoped>
.events-page {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
  padding: 1rem;
}
.events-page__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.events-page__header h1 {
  font-size: 1.25rem;
  margin: 0;
}
.events-page__tabs {
  display: flex;
  gap: 0.25rem;
  flex-wrap: wrap;
}
.events-page__tab {
  padding: 0.35rem 0.75rem;
  border: 1px solid var(--color-border, #ccc);
  border-radius: 999px;
  background: transparent;
  cursor: pointer;
}
.events-page__tab--active {
  background: var(--color-accent, #356);
  color: #fff;
  border-color: transparent;
}
.events-page__toggle {
  display: flex;
  gap: 0.4rem;
  align-items: center;
  font-size: 0.85rem;
}
.events-page__state {
  padding: 1rem;
  text-align: center;
  color: var(--color-text-muted, #888);
}
.events-page__calendar {
  border: 1px solid var(--color-border, #ccc);
  border-radius: 0.5rem;
  padding: 0.75rem 1rem;
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
}
.events-page__calendar h2 {
  font-size: 0.95rem;
  margin: 0;
}
.events-page__hint {
  margin: 0;
  font-size: 0.8rem;
  color: var(--color-text-muted, #888);
}
.events-page__url {
  word-break: break-all;
  font-size: 0.8rem;
}
.events-page__calendar-actions {
  display: flex;
  gap: 0.5rem;
  flex-wrap: wrap;
}
.events-page__sentinel {
  height: 1px;
}
</style>
