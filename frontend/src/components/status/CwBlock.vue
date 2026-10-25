<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { FontAwesomeIcon } from "@/icons";
import { useSettingsStore } from "@/stores/settings";

const props = defineProps<{
  spoilerText: string;
}>();
const { t } = useI18n();
const settings = useSettingsStore();

// `cw_always_expand` (yonder setting) expands CWs by default; kept
// reactive so settings loaded after mount still apply.
const defaultExpanded = computed(
  () =>
    !props.spoilerText ||
    Boolean(
      (settings.settings as Record<string, unknown> | null)?.cw_always_expand,
    ),
);
// `null` = no user choice yet — follow the default.
const toggled = ref<boolean | null>(null);
const expanded = computed(() => toggled.value ?? defaultExpanded.value);
const label = computed(() =>
  expanded.value ? t("status.cwHide") : t("status.cwShow"),
);
</script>

<template>
  <div class="cw">
    <div v-if="spoilerText" class="cw__header">
      <span class="cw__text">{{ spoilerText }}</span>
      <button
        type="button"
        class="cw__toggle"
        :aria-expanded="expanded"
        @click="toggled = !expanded"
      >
        <FontAwesomeIcon :icon="expanded ? 'eye-slash' : 'eye'" />
        {{ label }}
      </button>
    </div>
    <div v-show="expanded" class="cw__body">
      <slot />
    </div>
  </div>
</template>

<style scoped>
.cw__header {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  margin-bottom: var(--space-2);
  background: var(--bg-sunken);
  border-radius: var(--radius-md);
}
.cw__text {
  flex: 1;
  font-weight: 600;
}
.cw__toggle {
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
  border: 1px solid var(--border);
  background: var(--bg-raised);
  border-radius: var(--radius-sm);
  padding: var(--space-1) var(--space-2);
  color: var(--text);
  cursor: pointer;
  font: inherit;
  font-size: var(--text-sm);
}
</style>
