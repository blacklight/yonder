<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";
import { FontAwesomeIcon } from "@/icons";
import Tooltip from "@/components/ui/Tooltip.vue";
import type { AdminSetting } from "@/api/adminSettings";

const props = defineProps<{ entry: AdminSetting }>();
const { t } = useI18n();

function format(value: unknown): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

const text = computed(() =>
  [
    `${t("admin.settings.helpKey")}: ${props.entry.toml_path}`,
    `${t("admin.settings.helpEnv")}: ${props.entry.env}`,
    `${t("admin.settings.helpValue")}: ${
      props.entry.sensitive
        ? props.entry.set
          ? t("admin.settings.secretSet")
          : t("admin.settings.secretUnset")
        : format(props.entry.value)
    }`,
    `${t("admin.settings.helpDefault")}: ${
      props.entry.sensitive
        ? t("admin.settings.secret")
        : format(props.entry.default)
    }`,
    `${t("admin.settings.helpSource")}: ${t(`admin.settings.source.${props.entry.source}`)}`,
  ].join("\n"),
);
</script>

<template>
  <Tooltip :text="text" class="setting-help">
    <button
      type="button"
      class="setting-help__btn"
      :aria-label="t('admin.settings.helpLabel')"
    >
      <FontAwesomeIcon
        icon="circle-question"
        class="setting-help__icon"
        aria-hidden="true"
      />
    </button>
  </Tooltip>
</template>

<style scoped>
.setting-help {
  color: var(--text-muted);
}
.setting-help__btn {
  display: inline-flex;
  padding: 0;
  border: 0;
  background: none;
  color: inherit;
  cursor: help;
}
.setting-help__icon {
  font-size: var(--font-sm, 0.8125rem);
}
</style>
