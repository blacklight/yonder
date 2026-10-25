<script setup lang="ts">
import { useI18n } from "vue-i18n";
import { useTheme } from "@/composables/useTheme";
import Avatar from "@/components/ui/Avatar.vue";
import Button from "@/components/ui/Button.vue";

const { t, locale } = useI18n();
const { toggle } = useTheme();
const locales = [
  { code: "en", label: "English" },
  { code: "es", label: "Español" },
  { code: "it", label: "Italiano" },
  { code: "de", label: "Deutsch" },
  { code: "ar", label: "العربية" },
];

async function setLocale(code: string) {
  const { applyLocale } = await import("@/i18n");
  await applyLocale(code);
}
</script>

<template>
  <div class="layout">
    <header class="layout__header">
      <router-link to="/" class="layout__brand">
        <img src="/logo.svg" alt="Yonder" class="layout__logo" />
        <span>{{ t("app.name") }}</span>
      </router-link>
      <nav class="layout__nav">
        <router-link to="/">{{ t("nav.home") }}</router-link>
        <router-link to="/settings">{{ t("nav.settings") }}</router-link>
      </nav>
      <div class="layout__actions">
        <select
          class="layout__locale"
          :value="locale"
          :aria-label="t('settings.language')"
          @change="setLocale(($event.target as HTMLSelectElement).value)"
        >
          <option v-for="l in locales" :key="l.code" :value="l.code">
            {{ l.label }}
          </option>
        </select>
        <Button variant="secondary" size="sm" @click="toggle">{{
          t("settings.theme")
        }}</Button>
        <Avatar name="?" size="sm" />
      </div>
    </header>
    <main class="layout__main">
      <slot />
    </main>
  </div>
</template>

<style scoped>
.layout {
  min-height: 100vh;
  display: flex;
  flex-direction: column;
}
.layout__header {
  display: flex;
  align-items: center;
  gap: var(--space-4);
  padding: var(--space-3) var(--space-4);
  background: var(--bg-raised);
  border-bottom: 1px solid var(--border);
}
.layout__brand {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  font-weight: 700;
  color: var(--text);
  text-decoration: none;
}
.layout__logo {
  width: 28px;
  height: 28px;
}
.layout__nav {
  display: flex;
  gap: var(--space-4);
}
.layout__nav a {
  color: var(--text-muted);
  text-decoration: none;
}
.layout__nav a.router-link-active {
  color: var(--accent);
  font-weight: 600;
}
.layout__actions {
  margin-left: auto;
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.layout__locale {
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-1) var(--space-2);
}
.layout__main {
  flex: 1;
  padding: var(--space-4);
  max-width: 60rem;
  width: 100%;
  margin: 0 auto;
}
</style>
