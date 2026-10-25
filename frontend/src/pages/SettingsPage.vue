<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { request } from "@/api/client";
import { useAuthStore } from "@/stores/auth";
import { useSettingsStore } from "@/stores/settings";
import { useTheme } from "@/composables/useTheme";
import type { ThemeMode } from "@/stores/theme";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";

const { t } = useI18n();
const theme = useTheme();
const settings = useSettingsStore();
const auth = useAuthStore();

const frontends = ref<{ name: string; version?: string }[]>([]);
const saved = ref(false);
const accentInput = ref("#14b8a6");

const localeOptions = [
  { code: "en", label: "English" },
  { code: "es", label: "Español" },
  { code: "it", label: "Italiano" },
  { code: "de", label: "Deutsch" },
  { code: "ar", label: "العربية" },
];

async function setLocale(code: string) {
  const { applyLocale } = await import("@/i18n");
  await applyLocale(code);
  if (auth.isLoggedIn) await settings.patch({ interface_language: code });
}

async function setFrontend(name: string) {
  if (!auth.isLoggedIn) return;
  await settings.patch({ frontend: name || null });
  saved.value = true;
  setTimeout(() => (saved.value = false), 2000);
}

async function saveAccent() {
  const accent = accentInput.value || null;
  await settings.setTheme(theme.mode.value, accent);
  saved.value = true;
  setTimeout(() => (saved.value = false), 2000);
}

onMounted(async () => {
  if (!auth.isLoggedIn) return;
  try {
    const data = await request<{ frontends: { name: string }[] }>({
      path: "/yonder/frontends",
    });
    frontends.value = data.frontends;
    await settings.fetch();
    accentInput.value = theme.accent.value ?? "";
  } catch {
    // Listing is best-effort while auth lands fully in epic 25.
  }
});
</script>

<template>
  <Card>
    <template #header>{{ t("settings.title") }}</template>
    <form class="settings" @submit.prevent>
      <label class="settings__row">
        <span>{{ t("settings.theme") }}</span>
        <select
          :value="theme.mode.value"
          @change="
            settings.setTheme(
              ($event.target as HTMLSelectElement).value as ThemeMode,
              theme.accent.value,
            )
          "
        >
          <option value="system">{{ t("settings.theme_system") }}</option>
          <option value="light">{{ t("settings.theme_light") }}</option>
          <option value="dark">{{ t("settings.theme_dark") }}</option>
        </select>
      </label>
      <div class="settings__row">
        <Input
          v-model="accentInput"
          :label="t('settings.accent')"
          placeholder="#14b8a6"
        />
        <Button variant="secondary" size="sm" @click="saveAccent">{{
          t("settings.save")
        }}</Button>
      </div>
      <label class="settings__row">
        <span>{{ t("settings.language") }}</span>
        <select @change="setLocale(($event.target as HTMLSelectElement).value)">
          <option
            v-for="l in localeOptions"
            :key="l.code"
            :value="l.code"
            :selected="l.code === settings.settings?.interface_language"
          >
            {{ l.label }}
          </option>
        </select>
      </label>
      <label v-if="frontends.length" class="settings__row">
        <span>{{ t("settings.frontend") }}</span>
        <select
          :value="settings.settings?.frontend ?? ''"
          @change="setFrontend(($event.target as HTMLSelectElement).value)"
        >
          <option value="">{{ t("settings.frontend_default") }}</option>
          <option v-for="f in frontends" :key="f.name" :value="f.name">
            {{ f.name }}
          </option>
        </select>
      </label>
      <p v-if="saved" class="settings__saved">{{ t("settings.saved") }}</p>
    </form>
  </Card>
</template>

<style scoped>
.settings {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.settings__row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.settings__row > span {
  min-width: 10rem;
  font-weight: 600;
  color: var(--text-muted);
}
.settings__row select {
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-2) var(--space-3);
}
.settings__saved {
  color: var(--success);
  margin: 0;
}
</style>
