<script setup lang="ts">
// Standalone spinner for section-level waits — the same wheel the
// `Button` loading state uses, with a screen-reader label.
import { useI18n } from "vue-i18n";

withDefaults(defineProps<{ size?: "sm" | "md" }>(), { size: "md" });
const { t } = useI18n();
</script>

<template>
  <span class="spinner" :class="`spinner--${size}`" role="status">
    <span class="spinner__wheel" aria-hidden="true" />
    <span class="visually-hidden">{{ t("common.loading") }}</span>
  </span>
</template>

<style scoped>
.spinner {
  display: inline-flex;
  color: var(--text-faint);
}
.spinner__wheel {
  border: 2px solid currentColor;
  border-top-color: transparent;
  border-radius: 50%;
  animation: spinner-rotate 0.8s linear infinite;
}
.spinner--sm .spinner__wheel {
  width: 0.9em;
  height: 0.9em;
}
.spinner--md .spinner__wheel {
  width: 1.4rem;
  height: 1.4rem;
}
@keyframes spinner-rotate {
  to {
    transform: rotate(360deg);
  }
}
</style>
