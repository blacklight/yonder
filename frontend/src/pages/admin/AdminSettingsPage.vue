<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import { useAdminSettingsStore } from "@/stores/adminSettings";
import SettingsSection from "@/components/admin/SettingsSection.vue";

// Generic admin settings page — one per route; `sections` lists the
// top-level config sections this page renders and `extraKeys` pulls
// individual keys out of other sections (e.g. General also shows
// `content.max_post_length` per the admin-page spec).
const props = defineProps<{
  titleKey: string;
  sections: string[];
  extraKeys?: string[];
}>();
const { t } = useI18n();
const store = useAdminSettingsStore();

const loading = ref(true);
const error = ref("");

const extraEntries = computed(() =>
  (props.extraKeys ?? [])
    .map((k) => store.byPath(k))
    .filter((e): e is NonNullable<typeof e> => !!e),
);

onMounted(async () => {
  loading.value = true;
  error.value = "";
  try {
    if (store.entries.length === 0) await store.fetch();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
});
</script>

<template>
  <div class="admin-settings">
    <h2 class="admin-settings__title">{{ t(props.titleKey) }}</h2>
    <p v-if="loading" class="admin-settings__status">
      {{ t("common.loading") }}
    </p>
    <p v-else-if="error" class="admin-settings__error" role="alert">
      {{ error }}
    </p>
    <template v-else>
      <SettingsSection
        v-for="section in props.sections"
        :key="section"
        :section-id="section"
        :title="t(`admin.sections.${section}`)"
        :entries="store.bySection(section)"
      />
      <SettingsSection
        v-if="extraEntries.length"
        section-id="other"
        :title="t('admin.sections.other')"
        :entries="extraEntries"
      />
      <slot />
    </template>
  </div>
</template>

<style scoped>
.admin-settings__title {
  margin: 0 0 var(--space-2);
  font-size: var(--font-lg, 1.25rem);
}
.admin-settings__status {
  color: var(--text-muted);
}
.admin-settings__error {
  color: var(--danger);
}
</style>
