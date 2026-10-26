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
import SettingsNav from "@/components/settings/SettingsNav.vue";

const { t, locale } = useI18n();
const theme = useTheme();
const settings = useSettingsStore();
const auth = useAuthStore();

const frontends = ref<{ name: string; version?: string }[]>([]);
const saved = ref(false);
const accentInput = ref(theme.accent.value ?? "");
const accentInvalid = ref(false);
const filterLanguagesInput = ref("");
const scheduledDaysInput = ref("0");

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

function notifySaved() {
  saved.value = true;
  setTimeout(() => (saved.value = false), 2000);
}

/** Boolean keys PATCH on toggle — the same instant-apply pattern the
 * selects use. */
async function patchBool(key: string, checked: boolean) {
  if (!auth.isLoggedIn) return;
  await settings.patch({ [key]: checked });
  notifySaved();
}

async function setVisibility(value: string) {
  if (!auth.isLoggedIn) return;
  await settings.patch({ default_visibility: value });
  notifySaved();
}

async function setPostingLanguage(code: string) {
  if (!auth.isLoggedIn) return;
  await settings.patch({ default_posting_language: code || null });
  notifySaved();
}

async function setEmailNotif(key: string, checked: boolean) {
  if (!auth.isLoggedIn) return;
  await settings.patch({ email_notifications: { [key]: checked } });
  notifySaved();
}

function emailNotifValue(key: string): boolean {
  return settings.settings?.email_notifications?.[key] ?? false;
}

/** `filter_languages` is a tag list server-side; the UI edits it as a
 * comma-separated string. */
async function saveFilterLanguages() {
  if (!auth.isLoggedIn) return;
  const tags = filterLanguagesInput.value
    .split(/[,\s]+/)
    .map((tag) => tag.trim().toLowerCase())
    .filter(Boolean);
  await settings.patch({ filter_languages: tags });
  filterLanguagesInput.value = tags.join(", ");
  notifySaved();
}

async function saveScheduledDays() {
  if (!auth.isLoggedIn) return;
  const days = Math.max(
    0,
    Math.min(3650, Math.trunc(Number(scheduledDaysInput.value) || 0)),
  );
  scheduledDaysInput.value = String(days);
  await settings.patch({ scheduled_deletion_days: days });
  notifySaved();
}

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
  notifySaved();
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
  notifySaved();
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
    filterLanguagesInput.value = (
      settings.settings?.filter_languages ?? []
    ).join(", ");
    scheduledDaysInput.value = String(
      settings.settings?.scheduled_deletion_days ?? 0,
    );
  } catch {
    // Listing is best-effort while auth lands fully in epic 25.
  }
});
</script>

<template>
  <div>
    <SettingsNav />
    <Card>
      <template #header>{{ t("settings.title") }}</template>
      <form class="settings" @submit.prevent>
        <section class="settings__group">
          <h3 class="settings__group-title">
            <FontAwesomeIcon icon="palette" />
            {{ t("settings.groupAppearance") }}
          </h3>
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
                  :style="{
                    background: preset,
                    color: accentForeground(preset),
                  }"
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
                  :error="
                    accentInvalid ? t('settings.accentInvalid') : undefined
                  "
                  placeholder="#14b8a6"
                />
                <Button variant="secondary" size="sm" @click="saveAccent">{{
                  t("settings.save")
                }}</Button>
              </div>
            </div>
          </div>
        </section>
        <section class="settings__group">
          <h3 class="settings__group-title">
            <FontAwesomeIcon icon="display" />
            {{ t("settings.groupInterface") }}
          </h3>
          <label class="settings__row">
            <span>{{ t("settings.language") }}</span>
            <select
              :value="locale"
              @change="setLocale(($event.target as HTMLSelectElement).value)"
            >
              <option v-for="l in localeOptions" :key="l.code" :value="l.code">
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
        </section>
        <template v-if="auth.isLoggedIn && settings.settings">
          <section class="settings__group">
            <h3 class="settings__group-title">
              <FontAwesomeIcon icon="message" />
              {{ t("settings.groupPosting") }}
            </h3>
            <label class="settings__row">
              <span>{{ t("settings.visibility") }}</span>
              <select
                :value="settings.settings.default_visibility ?? 'public'"
                @change="
                  setVisibility(($event.target as HTMLSelectElement).value)
                "
              >
                <option value="public">{{ t("settings.vis_public") }}</option>
                <option value="unlisted">
                  {{ t("settings.vis_unlisted") }}
                </option>
                <option value="followers_only">
                  {{ t("settings.vis_followers") }}
                </option>
                <option value="private">{{ t("settings.vis_private") }}</option>
              </select>
            </label>
            <label class="settings__row">
              <span>{{ t("settings.postingLanguage") }}</span>
              <select
                :value="settings.settings.default_posting_language ?? ''"
                @change="
                  setPostingLanguage(($event.target as HTMLSelectElement).value)
                "
              >
                <option value="">
                  {{ t("settings.postingLanguage_auto") }}
                </option>
                <option
                  v-for="l in localeOptions"
                  :key="l.code"
                  :value="l.code"
                >
                  {{ l.label }}
                </option>
              </select>
            </label>
            <label class="settings__flag">
              <input
                type="checkbox"
                :checked="settings.settings.default_sensitive ?? false"
                @change="
                  patchBool(
                    'default_sensitive',
                    ($event.target as HTMLInputElement).checked,
                  )
                "
              />
              <span>{{ t("settings.defaultSensitive") }}</span>
            </label>
          </section>
          <section class="settings__group">
            <h3 class="settings__group-title">
              <FontAwesomeIcon icon="eye" />
              {{ t("settings.groupReading") }}
            </h3>
            <label class="settings__flag">
              <input
                type="checkbox"
                :checked="settings.settings.cw_always_expand ?? false"
                @change="
                  patchBool(
                    'cw_always_expand',
                    ($event.target as HTMLInputElement).checked,
                  )
                "
              />
              <span>{{ t("settings.cwAlwaysExpand") }}</span>
            </label>
            <label class="settings__flag">
              <input
                type="checkbox"
                :checked="settings.settings.show_app_display ?? false"
                @change="
                  patchBool(
                    'show_app_display',
                    ($event.target as HTMLInputElement).checked,
                  )
                "
              />
              <span>{{ t("settings.showAppDisplay") }}</span>
            </label>
            <div class="settings__row settings__row--top">
              <span>{{ t("settings.filterLanguages") }}</span>
              <div class="settings__stack">
                <Input
                  v-model="filterLanguagesInput"
                  :label="t('settings.filterLanguages')"
                  class="settings__sr-label"
                  placeholder="en, es"
                  @keydown.enter.prevent="saveFilterLanguages"
                />
                <small class="settings__hint">{{
                  t("settings.filterLanguagesHint")
                }}</small>
                <div>
                  <Button
                    variant="secondary"
                    size="sm"
                    @click="saveFilterLanguages"
                  >
                    {{ t("settings.save") }}
                  </Button>
                </div>
              </div>
            </div>
          </section>
          <section class="settings__group">
            <h3 class="settings__group-title">
              <FontAwesomeIcon icon="bell" />
              {{ t("settings.groupNotifications") }}
            </h3>
            <label class="settings__flag">
              <input
                type="checkbox"
                :checked="
                  settings.settings.block_stranger_notifications ?? false
                "
                @change="
                  patchBool(
                    'block_stranger_notifications',
                    ($event.target as HTMLInputElement).checked,
                  )
                "
              />
              <span>{{ t("settings.blockStranger") }}</span>
            </label>
            <fieldset class="settings__email">
              <legend>{{ t("settings.emailNotifications") }}</legend>
              <label class="settings__flag">
                <input
                  type="checkbox"
                  :checked="emailNotifValue('follow')"
                  @change="
                    setEmailNotif(
                      'follow',
                      ($event.target as HTMLInputElement).checked,
                    )
                  "
                />
                <span>{{ t("settings.email_follow") }}</span>
              </label>
              <label class="settings__flag">
                <input
                  type="checkbox"
                  :checked="emailNotifValue('follow_request')"
                  @change="
                    setEmailNotif(
                      'follow_request',
                      ($event.target as HTMLInputElement).checked,
                    )
                  "
                />
                <span>{{ t("settings.email_follow_request") }}</span>
              </label>
              <label class="settings__flag">
                <input
                  type="checkbox"
                  :checked="emailNotifValue('reblog')"
                  @change="
                    setEmailNotif(
                      'reblog',
                      ($event.target as HTMLInputElement).checked,
                    )
                  "
                />
                <span>{{ t("settings.email_reblog") }}</span>
              </label>
              <label class="settings__flag">
                <input
                  type="checkbox"
                  :checked="emailNotifValue('favourite')"
                  @change="
                    setEmailNotif(
                      'favourite',
                      ($event.target as HTMLInputElement).checked,
                    )
                  "
                />
                <span>{{ t("settings.email_favourite") }}</span>
              </label>
              <label class="settings__flag">
                <input
                  type="checkbox"
                  :checked="emailNotifValue('mention')"
                  @change="
                    setEmailNotif(
                      'mention',
                      ($event.target as HTMLInputElement).checked,
                    )
                  "
                />
                <span>{{ t("settings.email_mention") }}</span>
              </label>
              <label class="settings__flag">
                <input
                  type="checkbox"
                  :checked="emailNotifValue('quote')"
                  @change="
                    setEmailNotif(
                      'quote',
                      ($event.target as HTMLInputElement).checked,
                    )
                  "
                />
                <span>{{ t("settings.email_quote") }}</span>
              </label>
            </fieldset>
          </section>
          <section class="settings__group">
            <h3 class="settings__group-title">
              <FontAwesomeIcon icon="lock" />
              {{ t("settings.groupPrivacy") }}
            </h3>
            <label class="settings__flag">
              <input
                type="checkbox"
                :checked="settings.settings.directory_opt_in ?? false"
                @change="
                  patchBool(
                    'directory_opt_in',
                    ($event.target as HTMLInputElement).checked,
                  )
                "
              />
              <span>{{ t("settings.directoryOptIn") }}</span>
            </label>
            <label class="settings__flag">
              <input
                type="checkbox"
                :checked="
                  settings.settings.include_public_posts_in_search ?? false
                "
                @change="
                  patchBool(
                    'include_public_posts_in_search',
                    ($event.target as HTMLInputElement).checked,
                  )
                "
              />
              <span>{{ t("settings.includePublicInSearch") }}</span>
            </label>
          </section>
          <section class="settings__group">
            <h3 class="settings__group-title">
              <FontAwesomeIcon icon="user" />
              {{ t("settings.groupAccount") }}
            </h3>
            <div class="settings__row settings__row--top">
              <span>{{ t("settings.scheduledDeletion") }}</span>
              <div class="settings__stack">
                <div class="settings__days">
                  <input
                    v-model="scheduledDaysInput"
                    type="number"
                    min="0"
                    max="3650"
                    class="settings__days-input"
                    :aria-label="t('settings.scheduledDeletion')"
                    @keydown.enter.prevent="saveScheduledDays"
                  />
                  <span>{{ t("settings.scheduledDeletionUnit") }}</span>
                  <Button
                    variant="secondary"
                    size="sm"
                    @click="saveScheduledDays"
                  >
                    {{ t("settings.save") }}
                  </Button>
                </div>
                <small class="settings__hint">{{
                  t("settings.scheduledDeletionHint")
                }}</small>
              </div>
            </div>
          </section>
        </template>
        <p v-if="saved" class="settings__saved">{{ t("settings.saved") }}</p>
      </form>
    </Card>
  </div>
</template>

<style scoped>
.settings {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
}
.settings__group-title {
  margin: 0;
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-sm);
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--text-muted);
}
.settings__group-title svg{
  color: var(--text-faint);
  margin-right: var(--space-2);
}
.settings__row {
  display: grid;
  grid-template-columns: minmax(8rem, 11rem) minmax(0, 26rem);
  align-items: center;
  gap: var(--space-2) var(--space-4);
  padding: var(--space-3) 0;
}
.settings__row + .settings__row {
  border-top: 1px solid var(--border);
}
.settings__row > span {
  font-weight: 600;
  color: var(--text-muted);
}
.settings__row select {
  max-width: 16rem;
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-2) var(--space-3);
}
.settings__row--top {
  align-items: start;
}
.settings__row--top > span {
  padding-top: var(--space-1);
}
@media (max-width: 768px) {
  .settings__row {
    grid-template-columns: 1fr;
  }
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
.accent-picker__custom .field {
  flex: 1;
  max-width: 14rem;
}
.settings__saved {
  color: var(--success);
  margin: 0;
}
.settings__flag {
  display: flex;
  align-items: flex-start;
  gap: var(--space-2);
  padding: var(--space-1) 0;
}
.settings__flag span {
  display: flex;
  flex-direction: column;
}
.settings__email {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  margin: var(--space-2) 0 0;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  max-width: 26rem;
}
.settings__email legend {
  font-size: var(--text-sm);
  font-weight: 600;
  color: var(--text-muted);
  padding: 0 var(--space-1);
}
.settings__stack {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  max-width: 26rem;
}
.settings__hint {
  color: var(--text-muted);
}
.settings__sr-label :deep(.field__label) {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
}
.settings__days {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.settings__days-input {
  width: 6rem;
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-2) var(--space-3);
  font: inherit;
}
</style>
