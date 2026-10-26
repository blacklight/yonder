<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import { request } from "@/api/client";
import { getStatusContext } from "@/api/statuses";
import type { Status } from "@/api/entities";
import StatusCard from "@/components/status/StatusCard.vue";
import { ApiError } from "@/api/errors";
import { useAuthStore } from "@/stores/auth";
import { useStreamingStore } from "@/stores/streaming";

const props = defineProps<{ id: string }>();
const { t } = useI18n();
const router = useRouter();
const status = ref<Status | null>(null);
const ancestors = ref<Status[]>([]);
const descendants = ref<Status[]>([]);
const error = ref<number | null>(null);

// A delete from a thread card drops it from the rendered lists.
function dropRemoved(id: string) {
  ancestors.value = ancestors.value.filter((s) => s.id !== id);
  descendants.value = descendants.value.filter((s) => s.id !== id);
}

// Mastodon/Songhive-style single threading: every descendant unfolds
// flat into the thread headed by its first-level ancestor — the reply
// that points directly at this status. Replies whose parent is
// missing (visibility-pruned) become heads of their own thread.
const threads = computed<Status[][]>(() => {
  const byId = new Map(descendants.value.map((s) => [s.id, s]));
  const headOf = (s: Status): Status => {
    let cur = s;
    const seen = new Set([cur.id]);
    while (cur.in_reply_to_id && cur.in_reply_to_id !== props.id) {
      const next = byId.get(cur.in_reply_to_id);
      if (!next || seen.has(next.id)) break;
      seen.add(next.id);
      cur = next;
    }
    return cur;
  };
  const groups = new Map<string, Status[]>();
  for (const s of descendants.value) {
    const head = headOf(s);
    const g = groups.get(head.id) ?? [];
    g.push(s);
    groups.set(head.id, g);
  }
  for (const g of groups.values()) {
    // The head opens the thread; the rest unfold chronologically.
    const head = g[0];
    g.sort((a, b) =>
      a.id === head.id ? -1 : b.id === head.id ? 1 : Number(a.id) - Number(b.id),
    );
  }
  return [...groups.values()];
});

async function load() {
  status.value = null;
  ancestors.value = [];
  descendants.value = [];
  error.value = null;
  try {
    const [s, ctx] = await Promise.all([
      request<Status>({ path: `/statuses/${props.id}` }),
      getStatusContext(props.id),
    ]);
    status.value = s;
    ancestors.value = ctx.ancestors;
    descendants.value = ctx.descendants;
  } catch (e) {
    error.value = e instanceof ApiError ? e.status : 500;
  }
}

// Stream payloads render unpersonalized — keep the viewer's own
// interaction flags when an `update` replaces a displayed row.
function mergeViewerFlags(next: Status, prev: Status): Status {
  return {
    ...next,
    favourited: prev.favourited,
    reblogged: prev.reblogged,
    bookmarked: prev.bookmarked,
    pinned: prev.pinned,
  };
}

const auth = useAuthStore();
const streaming = useStreamingStore();
let unsub: (() => void)[] = [];

function onStreamEvent(ev: { event: string; payload: unknown }) {
  if (ev.event === "delete") {
    dropRemoved(String(ev.payload));
    return;
  }
  if (ev.event !== "update") return;
  const s = ev.payload as Status;
  if (status.value?.id === s.id) {
    status.value = mergeViewerFlags(s, status.value);
    return;
  }
  for (const list of [ancestors.value, descendants.value]) {
    const i = list.findIndex((x) => x.id === s.id);
    if (i !== -1) {
      list[i] = mergeViewerFlags(s, list[i]);
      return;
    }
  }
  // A reply landing anywhere inside the visible thread appends in
  // chronological order — covers own replies, remote replies and
  // remote edits pushed over the stream without a refresh.
  const known = new Set([
    status.value?.id,
    ...ancestors.value.map((x) => x.id),
    ...descendants.value.map((x) => x.id),
  ]);
  if (s.in_reply_to_id && known.has(s.in_reply_to_id)) {
    descendants.value = [...descendants.value, s].sort(
      (a, b) => Number(a.id) - Number(b.id),
    );
  }
}

// Clicking an ancestor/descendant card navigates to another
// `/statuses/{id}` — the component is reused, so the param change,
// not a remount, must trigger the refetch.
watch(() => props.id, load);

onMounted(() => {
  void load();
  unsub = [streaming.subscribe("public", onStreamEvent)];
  // The `user` stream is auth-scoped — only subscribe when signed in
  // (own posts and followers' posts land there).
  if (auth.isLoggedIn) unsub.push(streaming.subscribe("user", onStreamEvent));
});

onBeforeUnmount(() => unsub.forEach((f) => f()));
</script>

<template>
  <main class="page">
    <template v-if="status">
      <div class="timeline">
        <StatusCard
          v-for="s in ancestors"
          :key="s.id"
          :status="s"
          @removed="dropRemoved"
        />
        <!-- Deleting the opened status leaves the page — there is
             nothing left to show. -->
        <StatusCard
          :status="status"
          @removed="() => router.push({ name: 'home' })"
        />
        <div v-for="(thread, i) in threads" :key="i" class="thread">
          <StatusCard
            v-for="s in thread"
            :key="s.id"
            :status="s"
            @removed="dropRemoved"
          />
        </div>
      </div>
    </template>
    <p v-else-if="error === 404">{{ t("status.notFound") }}</p>
    <p v-else-if="error">{{ t("status.loadError") }}</p>
    <p v-else aria-busy="true">{{ t("common.loading") }}</p>
  </main>
</template>

<style scoped>
.timeline {
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  overflow: hidden;
}
/* Mastodon-style thread connector: a vertical line runs from the
   thread head's avatar column down to the last member's; every member
   after the head indents one avatar-width beside the line. Flat —
   no recursive nesting, so deep threads stay readable on mobile. */
.thread {
  position: relative;
}
.thread::before {
  content: "";
  position: absolute;
  /* Far-left rail running under the head card through the group —
     matches Mastodon's connector placement, not the avatar column. */
  top: 0;
  bottom: 0;
  left: 0;
  width: 3px;
  background: var(--border);
}
.thread > :nth-child(n + 2) {
  margin-left: 1px;
}
/* A single-member thread has nothing to connect — no line. */
.thread:has(> :only-child)::before {
  display: none;
}
</style>
