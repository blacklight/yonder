<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import Tabs from "@/components/ui/Tabs.vue";

/** The `/settings*` sub-navigation shared by the settings pages. */
const { t } = useI18n();
const route = useRoute();
const router = useRouter();

const items = computed(() => [
  { key: "settings", label: t("settingsNav.preferences") },
  { key: "settings-profile", label: t("settingsNav.profile") },
  { key: "settings-security", label: t("settingsNav.security") },
  { key: "settings-tokens", label: t("settingsNav.tokens") },
]);

const active = computed(() =>
  typeof route.name === "string" ? route.name : "settings",
);

function go(key: string) {
  if (key !== route.name) void router.push({ name: key });
}
</script>

<template>
  <Tabs
    :model-value="active"
    :items="items"
    :label="t('settingsNav.label')"
    @update:model-value="go"
  />
</template>
