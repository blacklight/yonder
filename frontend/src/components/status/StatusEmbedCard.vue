<script setup lang="ts">
import { computed } from "vue";
import type { Status } from "@/api/entities";

// Link-preview card for the standalone embed page. Always renders as a
// plain link card — the click-to-play iframe variant from StatusCard
// would nest a third-party player inside the embed frame, so video
// cards just link out like everything else.
const props = defineProps<{ status: Status }>();

// Same scheme check as StatusCard: preview rows are scheme-validated
// upstream, re-checked at the sink.
const card = computed(() => {
  const c = props.status.card;
  return c && /^https?:\/\//i.test(c.url) ? c : null;
});
</script>

<template>
  <a
    v-if="card"
    :href="card.url"
    class="embed-card__preview"
    target="_blank"
    rel="noopener"
  >
    <img
      v-if="card.image"
      :src="card.image"
      alt=""
      class="embed-card__preview-img"
      loading="lazy"
    />
    <span class="embed-card__preview-body">
      <strong>{{ card.title }}</strong>
      <span class="embed-card__preview-desc">{{ card.description }}</span>
      <span class="embed-card__preview-provider">{{ card.provider_name }}</span>
    </span>
  </a>
</template>

<style scoped>
.embed-card__preview {
  display: flex;
  gap: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-3);
  margin: var(--space-2) 0;
  text-decoration: none;
  color: var(--text);
}
.embed-card__preview-img {
  width: 4rem;
  height: 4rem;
  object-fit: cover;
  border-radius: var(--radius-sm);
}
.embed-card__preview-body {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.embed-card__preview-desc {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
}
.embed-card__preview-provider {
  color: var(--text-muted);
  font-size: var(--text-xs);
}
</style>
