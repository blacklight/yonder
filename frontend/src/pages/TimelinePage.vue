<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, watch } from "vue";
import { useI18n } from "vue-i18n";
import StatusCard from "@/components/status/StatusCard.vue";
import StatusCardSkeleton from "@/components/status/StatusCardSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Spinner from "@/components/ui/Spinner.vue";
import { FontAwesomeIcon } from "@/icons";
import { useTimelineStore, type TimelineKey } from "@/stores/timeline";
import { useStreamingStore } from "@/stores/streaming";
import { useInfiniteScroll } from "@/composables/useInfiniteScroll";
import type { Status } from "@/api/entities";

/**
 * One component for every timeline — `key` is the folded stream name
 * (`public`, `public:local`, `hashtag:<tag>`, `list:<id>`); `stream`
 * is the subscription name to send (`user` for home).
 */
const props = defineProps<{
  timeline: TimelineKey;
  stream?: string;
  /** i18n key suffix for the empty state (`timeline.empty_<emptyKey>`). */
  emptyKey?: string;
}>();
const { t } = useI18n();
const timelines = useTimelineStore();
const streaming = useStreamingStore();

const tl = computed(() => timelines.get(props.timeline));
/**
 * Hashtag timelines back a public feed (`/tags/{t}.rss`); list feeds
 * need an owner token, so only hashtags get the header/icon.
 */
const feedTag = computed(() =>
  props.timeline.startsWith("hashtag:")
    ? props.timeline.slice("hashtag:".length)
    : null,
);
const feedHref = computed(() =>
  feedTag.value ? `/tags/${encodeURIComponent(feedTag.value)}.rss` : null,
);
// Infinite scroll — the bottom sentinel pages the next batch in as it
// enters the viewport; `hasMore` also guards in-flight fetches.
const { vInfinite, ioSupported } = useInfiniteScroll(
  () => timelines.hasMore(props.timeline) && !tl.value.loading,
  () => timelines.loadMore(props.timeline),
);
let offStream: (() => void) | null = null;

function streamKey(): string {
  return props.stream ?? props.timeline;
}

function onStreamEvent(ev: { event: string; payload: unknown }) {
  if (ev.event === "update")
    timelines.buffer(props.timeline, ev.payload as Status);
  else if (ev.event === "status.update")
    // An edit only ever replaces — never lands as a "new" pending item.
    timelines.replace(ev.payload as Status);
  else if (ev.event === "delete") timelines.remove(String(ev.payload));
}

onMounted(() => {
  void timelines.load(props.timeline);
  offStream = streaming.subscribe(streamKey(), onStreamEvent);
});

watch(
  () => props.timeline,
  (key, old) => {
    offStream?.();
    offStream = streaming.subscribe(streamKey(), onStreamEvent);
    void timelines.load(key);
    void old;
  },
);

onBeforeUnmount(() => {
  offStream?.();
});
</script>

<template>
  <div class="timeline">
    <header v-if="feedTag" class="timeline__header">
      <h1 class="timeline__title">#{{ feedTag }}</h1>
      <a
        v-if="feedHref"
        class="timeline__feed"
        :href="feedHref"
        :title="t('common.feed')"
        :aria-label="t('common.feed')"
      >
        <FontAwesomeIcon icon="rss" />
      </a>
    </header>
    <button
      v-if="timelines.pendingCount(timeline) > 0"
      type="button"
      class="timeline__pending"
      data-testid="pending"
      @click="timelines.flushPending(timeline)"
    >
      <FontAwesomeIcon icon="arrow-up" />
      {{ t("timeline.newPosts", { count: timelines.pendingCount(timeline) }) }}
    </button>

    <div
      v-if="tl.loading && !tl.loaded"
      role="status"
      :aria-label="t('common.loading')"
    >
      <StatusCardSkeleton v-for="n in 3" :key="n" />
    </div>
    <div v-else-if="tl.error" class="timeline__state timeline__error">
      <p>{{ tl.error }}</p>
      <!-- A failed `loadMore` must resume from the cursor — `load`
           would reset the whole list to page one. -->
      <Button
        size="sm"
        @click="
          tl.loaded ? timelines.loadMore(timeline) : timelines.load(timeline)
        "
        >{{ t("timeline.retry") }}</Button
      >
    </div>
    <p v-else-if="tl.loaded && tl.items.length === 0" class="timeline__state">
      {{ t(`timeline.empty_${emptyKey ?? "empty"}`) }}
    </p>

    <StatusCard
      v-for="s in tl.items"
      :key="s.id"
      :status="s"
      @removed="(id) => timelines.remove(id)"
    />

    <div v-infinite class="timeline__sentinel" aria-hidden="true" />
    <div v-if="tl.loading && tl.loaded" class="timeline__state">
      <Spinner />
    </div>
    <p
      v-else-if="tl.loaded && !timelines.hasMore(timeline) && tl.items.length"
      class="timeline__state"
    >
      ·
    </p>
    <!-- Manual fallback for engines without IntersectionObserver. -->
    <Button
      v-if="!ioSupported && timelines.hasMore(timeline)"
      :loading="tl.loading"
      @click="timelines.loadMore(timeline)"
      >{{ t("common.loadMore") }}</Button
    >
  </div>
</template>

<style scoped>
.timeline {
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  overflow: hidden;
}
.timeline__header {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
  border-bottom: 1px solid var(--border);
}
.timeline__title {
  flex: 1;
  margin: 0;
  font-size: var(--text-lg);
  font-weight: 700;
  overflow-wrap: anywhere;
}
.timeline__feed {
  flex-shrink: 0;
  padding: var(--space-1);
  border-radius: var(--radius-sm);
  color: var(--text-muted);
  font-size: var(--text-sm);
  text-decoration: none;
}
.timeline__feed:hover,
.timeline__feed:focus-visible {
  background: var(--bg-sunken);
  color: var(--text);
}
.timeline__pending {
  width: 100%;
  margin-bottom: var(--space-2);
  padding: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-raised);
  color: var(--accent-link);
  font-weight: 600;
  cursor: pointer;
}
.timeline__state {
  text-align: center;
  color: var(--text-muted);
  padding: var(--space-5) 0;
}
.timeline__sentinel {
  height: 1px;
}
</style>
