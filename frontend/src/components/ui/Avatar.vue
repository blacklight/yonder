<script setup lang="ts">
import { computed } from "vue";

const props = withDefaults(
  defineProps<{
    src?: string;
    alt?: string;
    name?: string;
    size?: "sm" | "md" | "lg";
  }>(),
  { alt: "", size: "md" },
);

const initial = computed(
  () => (props.name ?? props.alt ?? "?").trim().charAt(0).toUpperCase() || "?",
);
</script>

<template>
  <span
    class="avatar"
    :class="`avatar--${size}`"
    role="img"
    :aria-label="alt || name"
  >
    <img v-if="src" :src="src" :alt="alt" class="avatar__img" />
    <span v-else class="avatar__initial" aria-hidden="true">{{ initial }}</span>
  </span>
</template>

<style scoped>
.avatar {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
  border-radius: var(--radius-full);
  background: var(--accent-soft);
  color: var(--accent-link);
  font-weight: 700;
}
.avatar--sm {
  width: 32px;
  height: 32px;
  font-size: var(--text-sm);
}
.avatar--md {
  width: 48px;
  height: 48px;
  font-size: var(--text-lg);
}
.avatar--lg {
  width: 96px;
  height: 96px;
  font-size: var(--text-2xl);
}
.avatar__img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
</style>
