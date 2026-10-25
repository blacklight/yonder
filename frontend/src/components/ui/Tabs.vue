<script setup lang="ts" generic="T extends string">
/** Pill-style tab strip — pressed state via `aria-pressed`, optional
 *  trailing slot for extra actions (e.g. a "Clear" button). */
defineProps<{
  items: readonly { key: T; label: string }[];
  modelValue: T;
  /** Accessible label for the nav landmark. */
  label?: string;
}>();
const emit = defineEmits<{ "update:modelValue": [key: T] }>();
</script>

<template>
  <nav class="tabs" :aria-label="label">
    <button
      v-for="item in items"
      :key="item.key"
      type="button"
      :aria-pressed="modelValue === item.key"
      @click="emit('update:modelValue', item.key)"
    >
      {{ item.label }}
    </button>
    <slot />
  </nav>
</template>

<style scoped>
.tabs {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-2);
  margin: var(--space-4) 0;
}
.tabs button {
  padding: var(--space-1) var(--space-4);
  border: 1px solid var(--border);
  border-radius: var(--radius-full);
  background: var(--bg-raised);
  color: var(--text-muted);
  font-weight: 600;
  cursor: pointer;
}
.tabs button[aria-pressed="true"] {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--accent-fg);
}
</style>
