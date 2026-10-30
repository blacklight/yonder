<script setup lang="ts">
// Shimmering placeholder block — the building block for skeleton
// screens. Purely decorative: always `aria-hidden`.
withDefaults(
  defineProps<{
    width?: string;
    height?: string;
    /** Round block for avatar stand-ins (`width` doubles as height). */
    circle?: boolean;
  }>(),
  { width: "100%", height: "0.875em", circle: false },
);
</script>

<template>
  <span
    class="skeleton"
    :class="{ 'skeleton--circle': circle }"
    :style="{ width, height: circle ? width : height }"
    aria-hidden="true"
  />
</template>

<style scoped>
.skeleton {
  display: block;
  border-radius: var(--radius-sm);
  background: var(--border);
  animation: skeleton-pulse 1.4s ease-in-out infinite;
}
.skeleton--circle {
  border-radius: var(--radius-full);
}
@keyframes skeleton-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.45;
  }
}
</style>
