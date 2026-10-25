<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { request } from "@/api/client";
import { useAuthStore } from "@/stores/auth";
import { useSettingsStore } from "@/stores/settings";
import { useTheme } from "@/composables/useTheme";
import { accentForeground, type ThemeMode } from "@/stores/theme";
import { FontAwesomeIcon } from "@/icons";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";

const { t } = useI18n();
const theme = useTheme();
const settings = useSettingsStore();
const auth = useAuthStore();

const frontends = ref<{ name: string; version?: string }[]>([]);
const saved = ref(false);
const accentInput = ref(theme.accent.value ?? "");
const accentInvalid = ref(false);

// One-click swatches — all survive both themes: --accent-fg/-soft/-link
// are derived from whatever is picked.
const accentPresets = [
  "#14b8a6", // teal — brand default
  "#0ea5e9", // sky
  "#3b82f6", // blue
  "#8b5cf6", // violet
  "#ec4899", // pink
  "#ef4444", // red
  "#f97316", // orange
  "#22c55e", // green
];

const HEX6 = /^#?[0-9a-f]{6}$/i;

// <input type="color"> only speaks #rrggbb — fall back to the brand
// teal when the textbox holds anything else (empty or partial input).
const pickerColor = computed(() =>
  HEX6.test(accentInput.value.trim()) && accentInput.value.trim()
    ? accentInput.value.trim()
    : "#14b8a6",
);

function isActiveAccent(preset: string): boolean {
  return theme.accent.value?.toLowerCase() === preset;
}

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

// Swatch clicks apply immediately — the live recolor is the feedback.
async function pickAccent(preset: string) {
  accentInput.value = preset;
  accentInvalid.value = false;
  await saveAccent();
}

async function saveAccent() {
  const raw = accentInput.value.trim();
  if (raw && !HEX6.test(raw)) {
    accentInvalid.value = true;
    return;
  }
  accentInvalid.value = false;
  const accent = raw ? (raw.startsWith("#") ? raw : `#${raw}`) : null;
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
      <div class="settings__row settings__row--top">
        <span>{{ t("settings.accent") }}</span>
        <div class="accent-picker">
          <div
            class="accent-picker__swatches"
            role="radiogroup"
            :aria-label="t('settings.accent')"
          >
            <button
              v-for="preset in accentPresets"
              :key="preset"
              type="button"
              role="radio"
              class="accent-picker__swatch"
              :class="{
                'accent-picker__swatch--active': isActiveAccent(preset),
              }"
              :style="{ background: preset, color: accentForeground(preset) }"
              :aria-checked="isActiveAccent(preset)"
              :aria-label="preset"
              @click="pickAccent(preset)"
            >
              <FontAwesomeIcon
                v-if="isActiveAccent(preset)"
                icon="check"
                aria-hidden="true"
              />
            </button>
            <input
              type="color"
              class="accent-picker__color"
              :value="pickerColor"
              :aria-label="t('settings.accentCustom')"
              @input="
                accentInput = ($event.target as HTMLInputElement).value;
                accentInvalid = false;
              "
            />
          </div>
          <div class="accent-picker__custom">
            <Input
              v-model="accentInput"
              :label="t('settings.accentCustom')"
              :error="accentInvalid ? t('settings.accentInvalid') : undefined"
              placeholder="#14b8a6"
            />
            <Button variant="secondary" size="sm" @click="saveAccent">{{
              t("settings.save")
            }}</Button>
          </div>
        </div>
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
.settings__row--top {
  align-items: flex-start;
}
.settings__row--top > span {
  padding-top: var(--space-2);
}
.accent-picker {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.accent-picker__swatches {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-2);
}
.accent-picker__swatch {
  width: 1.75rem;
  height: 1.75rem;
  padding: 0;
  border: 2px solid var(--border);
  border-radius: 50%;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 0.7rem;
}
.accent-picker__swatch--active {
  border-color: var(--text);
}
.accent-picker__color {
  width: 1.75rem;
  height: 1.75rem;
  padding: 0;
  border: 2px solid var(--border);
  border-radius: 50%;
  background: none;
  cursor: pointer;
}
.accent-picker__color::-webkit-color-swatch-wrapper {
  padding: 0;
}
.accent-picker__color::-webkit-color-swatch {
  border: 0;
  border-radius: 50%;
}
.accent-picker__color::-moz-color-swatch {
  border: 0;
  border-radius: 50%;
}
.accent-picker__custom {
  display: flex;
  align-items: flex-end;
  gap: var(--space-2);
}
.settings__saved {
  color: var(--success);
  margin: 0;
}
</style>
