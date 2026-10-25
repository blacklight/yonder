<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import StatusCard from "@/components/status/StatusCard.vue";
import Button from "@/components/ui/Button.vue";
import { useTimelineStore, type TimelineKey } from "@/stores/timeline";
import { useStreamingStore } from "@/stores/streaming";
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
const sentinel = ref<HTMLElement | null>(null);
let observer: IntersectionObserver | null = null;
let offStream: (() => void) | null = null;

function streamKey(): string {
  return props.stream ?? props.timeline;
}

function onStreamEvent(ev: { event: string; payload: unknown }) {
  if (ev.event === "update")
    timelines.buffer(props.timeline, ev.payload as Status);
  else if (ev.event === "delete")
    timelines.remove(props.timeline, String(ev.payload));
}

onMounted(() => {
  void timelines.load(props.timeline);
  offStream = streaming.subscribe(streamKey(), onStreamEvent);
  observer = new IntersectionObserver((entries) => {
    if (entries[0]?.isIntersecting) void timelines.loadMore(props.timeline);
  });
  if (sentinel.value) observer.observe(sentinel.value);
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
  observer?.disconnect();
});
</script>

<template>
  <div class="timeline">
    <button
      v-if="timelines.pendingCount(timeline) > 0"
      type="button"
      class="timeline__pending"
      data-testid="pending"
      @click="timelines.flushPending(timeline)"
    >
      {{ t("timeline.newPosts", { count: timelines.pendingCount(timeline) }) }}
    </button>

    <p v-if="tl.loading && !tl.loaded" class="timeline__state">
      {{ t("ui.loading") }}
    </p>
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
      @removed="(id) => timelines.remove(timeline, id)"
    />

    <div ref="sentinel" class="timeline__sentinel" aria-hidden="true" />
    <p v-if="tl.loading && tl.loaded" class="timeline__state">
      {{ t("ui.loading") }}
    </p>
    <p
      v-else-if="tl.loaded && !timelines.hasMore(timeline) && tl.items.length"
      class="timeline__state"
    >
      ·
    </p>
  </div>
</template>

<style scoped>
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
