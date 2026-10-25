<script setup lang="ts">
withDefaults(
  defineProps<{
    variant?: "primary" | "secondary" | "danger";
    size?: "sm" | "md";
    disabled?: boolean;
    loading?: boolean;
    type?: "button" | "submit" | "reset";
  }>(),
  {
    variant: "primary",
    size: "md",
    disabled: false,
    loading: false,
    type: "button",
  },
);
</script>

<template>
  <button
    :type="type"
    class="btn"
    :class="[`btn--${variant}`, `btn--${size}`]"
    :disabled="disabled || loading"
    :aria-busy="loading || undefined"
  >
    <span v-if="loading" class="btn__spinner" aria-hidden="true" />
    <slot />
  </button>
</template>

<style scoped>
.btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: var(--space-2);
  border: 1px solid transparent;
  border-radius: var(--radius-md);
  padding: var(--space-2) var(--space-4);
  font-weight: 600;
  cursor: pointer;
  transition:
    background-color var(--duration-fast) ease,
    opacity var(--duration-fast) ease;
}
.btn--sm {
  padding: var(--space-1) var(--space-3);
  font-size: var(--text-sm);
}
.btn--primary {
  background: var(--accent);
  color: var(--accent-fg);
}
.btn--secondary {
  background: transparent;
  color: var(--text);
  border-color: var(--border);
}
.btn--danger {
  background: var(--danger);
  color: #fff;
}
.btn:hover:not(:disabled) {
  opacity: 0.9;
}
.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.btn__spinner {
  width: 0.9em;
  height: 0.9em;
  border: 2px solid currentColor;
  border-top-color: transparent;
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
}
@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
</style>
