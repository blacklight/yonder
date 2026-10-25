<script setup lang="ts">
import { computed } from "vue";
import type { AdminSetting } from "@/api/adminSettings";
import SettingField from "./SettingField.vue";

const props = defineProps<{
  /** `admin.sections.<id>` i18n key suffix. */
  sectionId: string;
  title: string;
  entries: AdminSetting[];
}>();

const sorted = computed(() =>
  [...props.entries].sort((a, b) => a.toml_path.localeCompare(b.toml_path)),
);
</script>

<template>
  <section class="settings-group" :aria-label="title">
    <h3 class="settings-group__title">{{ title }}</h3>
    <SettingField
      v-for="entry in sorted"
      :key="entry.toml_path"
      :entry="entry"
      class="settings-group__row"
    />
  </section>
</template>

<style scoped>
.settings-group__title {
  margin: var(--space-6) 0 var(--space-2);
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-xs);
  font-weight: 700;
  letter-spacing: 0.05em;
  text-transform: uppercase;
  color: var(--text-muted);
}
.settings-group:first-child .settings-group__title {
  margin-top: 0;
}
.settings-group__row + .settings-group__row {
  border-top: 1px solid var(--border);
}
</style>
