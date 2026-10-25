<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import Avatar from "@/components/ui/Avatar.vue";
import Button from "@/components/ui/Button.vue";
import NavMenu, { type NavItem } from "./NavMenu.vue";
import MobileMenu from "./MobileMenu.vue";
import { FontAwesomeIcon } from "@/icons";
import { useTheme } from "@/composables/useTheme";
import { getInstance } from "@/api/instance";
import { useAuthStore } from "@/stores/auth";
import { useComposeStore } from "@/stores/compose";
import { useNotificationsStore } from "@/stores/notifications";
import { useConversationsStore } from "@/stores/conversations";

const { t, locale } = useI18n();
const route = useRoute();
const router = useRouter();
const auth = useAuthStore();
const compose = useComposeStore();
const notifications = useNotificationsStore();
const conversations = useConversationsStore();
const { resolvedMode, toggle } = useTheme();

const locales = [
  { code: "en", label: "English" },
  { code: "es", label: "Español" },
  { code: "it", label: "Italiano" },
  { code: "de", label: "Deutsch" },
  { code: "ar", label: "العربية" },
];

const menuOpen = ref(false);
const searchQuery = ref("");
const instanceTitle = ref<string | null>(null);

onMounted(() => {
  void getInstance()
    .then((i) => (instanceTitle.value = i.title ?? null))
    .catch(() => {});
  notifications.start();
  conversations.start();
});

// The badge and DM feeds follow the session.
watch(
  () => auth.isLoggedIn,
  (logged) => {
    if (logged) {
      notifications.start();
      conversations.start();
    } else {
      notifications.stop();
      conversations.stop();
    }
  },
);

// Any navigation closes the mobile menu.
watch(
  () => route.fullPath,
  () => (menuOpen.value = false),
);

interface MenuEntry extends NavItem {
  visible: boolean;
  run?: () => void;
}

// The data-driven menu — visibility follows auth/role; items without
// `to` carry a `run` action (direct-message shortcut, sign out).
const menuSource = computed<MenuEntry[]>(() => [
  {
    id: "home",
    label: t("nav.home"),
    to: "/home",
    icon: "house",
    visible: auth.isLoggedIn,
  },
  {
    id: "local",
    label: t("nav.local"),
    to: "/local",
    icon: "users",
    visible: true,
  },
  {
    id: "explore",
    label: t("nav.explore"),
    to: "/explore",
    icon: "compass",
    visible: true,
  },
  {
    id: "notifications",
    label: t("nav.notifications"),
    to: "/notifications",
    icon: "bell",
    badge: notifications.unread || undefined,
    visible: auth.isLoggedIn,
  },
  {
    id: "messages",
    label: t("nav.messages"),
    to: "/messages",
    icon: "comments",
    visible: auth.isLoggedIn,
  },
  {
    id: "direct",
    label: t("nav.direct"),
    icon: "envelope",
    visible: auth.isLoggedIn,
    run: () => {
      compose.openDirect();
      void router.push("/compose");
    },
  },
  {
    id: "favourites",
    label: t("nav.favourites"),
    to: "/favourites",
    icon: "star",
    visible: auth.isLoggedIn,
  },
  {
    id: "bookmarks",
    label: t("nav.bookmarks"),
    to: "/bookmarks",
    icon: "bookmark",
    visible: auth.isLoggedIn,
  },
  {
    id: "lists",
    label: t("nav.lists"),
    to: "/lists",
    icon: "list-ul",
    visible: auth.isLoggedIn,
  },
  {
    id: "hashtags",
    label: t("nav.hashtags"),
    to: "/hashtags",
    icon: "hashtag",
    visible: true,
  },
  {
    id: "moderation",
    label: t("nav.moderation"),
    to: "/moderation",
    icon: "gavel",
    visible: auth.isStaff,
  },
  {
    id: "admin",
    label: t("nav.admin"),
    to: "/admin",
    icon: "toolbox",
    visible: auth.isAdmin,
  },
  {
    id: "about",
    label: t("nav.about"),
    to: "/about",
    icon: "circle-info",
    visible: true,
  },
  {
    id: "settings",
    label: t("nav.settings"),
    to: "/settings",
    icon: "gear",
    visible: true,
  },
  {
    id: "sign-in",
    label: t("nav.signIn"),
    to: "/sign-in",
    icon: "right-to-bracket",
    visible: !auth.isLoggedIn,
  },
  {
    id: "register",
    label: t("nav.signUp"),
    to: "/register",
    icon: "user-plus",
    visible: !auth.isLoggedIn,
  },
  {
    id: "sign-out",
    label: t("auth.signOut"),
    icon: "right-from-bracket",
    visible: auth.isLoggedIn,
    run: () => {
      notifications.stop();
      conversations.stop();
      // Explicit sign-out drops any saved draft — it can hold DM text.
      compose.clearPersisted();
      auth.signOut();
      void router.push("/");
    },
  },
]);

const menuItems = computed<NavItem[]>(() =>
  menuSource.value
    .filter((i) => i.visible)
    .map((i) => ({
      id: i.id,
      label: i.label,
      to: i.to,
      icon: i.icon,
      badge: i.badge,
    })),
);

// The mobile drawer gets a compose entry — the big sidebar button is
// desktop-only, so without this there is no way to post on mobile.
const mobileItems = computed<NavItem[]>(() =>
  auth.isLoggedIn
    ? [
        {
          id: "compose",
          label: t("nav.compose"),
          to: "/compose",
          icon: "pen-to-square",
        },
        ...menuItems.value,
      ]
    : menuItems.value,
);

function onSelect(item: NavItem) {
  menuOpen.value = false;
  menuSource.value.find((m) => m.id === item.id)?.run?.();
}

async function setLocale(code: string) {
  const { applyLocale } = await import("@/i18n");
  await applyLocale(code);
}

function search() {
  const q = searchQuery.value.trim();
  if (q) void router.push({ name: "search", query: { q } });
}
</script>

<template>
  <div class="layout">
    <aside class="layout__sidebar">
      <router-link to="/" class="layout__brand">
        <img src="/logo.svg" alt="Yonder" class="layout__logo" />
        <span>{{ instanceTitle ?? t("app.name") }}</span>
      </router-link>

      <form class="layout__search" role="search" @submit.prevent="search">
        <FontAwesomeIcon icon="magnifying-glass" class="layout__search-icon" />
        <input
          v-model="searchQuery"
          type="search"
          class="layout__search-input"
          :placeholder="t('nav.searchPlaceholder')"
          :aria-label="t('nav.search')"
        />
      </form>

      <Button
        v-if="auth.isLoggedIn"
        class="layout__compose"
        @click="router.push('/compose')"
      >
        <FontAwesomeIcon icon="pen-to-square" />
        {{ t("nav.compose") }}</Button
      >

      <NavMenu :items="menuItems" @select="onSelect" />

      <div class="layout__sidebar-footer">
        <router-link v-if="auth.account" to="/profile" class="layout__user">
          <Avatar
            :name="auth.account.display_name || auth.account.username"
            :src="auth.account.avatar"
            size="sm"
          />
          <span class="layout__user-names">
            <span class="layout__user-display">{{
              auth.account.display_name || auth.account.username
            }}</span>
            <span class="layout__user-acct">@{{ auth.account.acct }}</span>
          </span>
        </router-link>
        <div class="layout__controls">
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
          <Button variant="secondary" size="sm" @click="toggle">
            <FontAwesomeIcon :icon="resolvedMode === 'dark' ? 'sun' : 'moon'" />
            {{ t("settings.theme") }}</Button
          >
        </div>
      </div>
    </aside>

    <header class="layout__topbar">
      <button
        type="button"
        class="layout__burger"
        :aria-label="t('menu.open')"
        :aria-expanded="menuOpen"
        @click="menuOpen = true"
      >
        <FontAwesomeIcon icon="bars" />
      </button>
      <router-link to="/" class="layout__topbar-title">
        {{ instanceTitle ?? t("app.name") }}
      </router-link>
      <router-link
        v-if="auth.account"
        to="/profile"
        class="layout__topbar-user"
      >
        <Avatar
          :name="auth.account.display_name || auth.account.username"
          :src="auth.account.avatar"
          size="sm"
        />
      </router-link>
    </header>

    <MobileMenu :open="menuOpen" @close="menuOpen = false">
      <NavMenu :items="mobileItems" @select="onSelect" />
      <div class="layout__controls layout__controls--mobile">
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
        <Button variant="secondary" size="sm" @click="toggle">
          <FontAwesomeIcon :icon="resolvedMode === 'dark' ? 'sun' : 'moon'" />
          {{ t("settings.theme") }}</Button
        >
      </div>
    </MobileMenu>

    <main class="layout__main">
      <slot />
    </main>
  </div>
</template>

<style scoped>
.layout {
  min-height: 100vh;
}
.layout__sidebar {
  display: none;
}
.layout__topbar {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-2) var(--space-4);
  background: var(--bg-raised);
  border-bottom: 1px solid var(--border);
  position: sticky;
  top: 0;
  z-index: 10;
}
.layout__burger {
  background: none;
  border: 0;
  font-size: 1.4rem;
  color: var(--text);
  cursor: pointer;
  padding: var(--space-1) var(--space-2);
}
.layout__topbar-title {
  font-weight: 700;
  color: var(--text);
  text-decoration: none;
}
.layout__topbar-user {
  margin-left: auto;
}
.layout__main {
  padding: var(--space-4);
  max-width: 44rem;
  width: 100%;
  margin: 0 auto;
}

@media (min-width: 768px) {
  .layout__topbar {
    display: none;
  }
  .layout__sidebar {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    position: fixed;
    top: 0;
    bottom: 0;
    left: 0;
    width: 16rem;
    padding: var(--space-4);
    background: var(--bg-raised);
    border-right: 1px solid var(--border);
    overflow-y: auto;
  }
  .layout__main {
    margin-left: 16rem;
    max-width: 44rem;
  }
}

.layout__brand {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  font-weight: 700;
  color: var(--text);
  text-decoration: none;
  font-size: 1.1rem;
}
.layout__logo {
  width: 28px;
  height: 28px;
}
.layout__search {
  position: relative;
}
.layout__search-icon {
  position: absolute;
  left: var(--space-3);
  top: 50%;
  transform: translateY(-50%);
  color: var(--text-muted);
  pointer-events: none;
}
.layout__search-input {
  width: 100%;
  padding: var(--space-2) var(--space-3);
  padding-left: calc(var(--space-3) + 1.5em);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg);
  color: var(--text);
  font: inherit;
}
.layout__compose {
  width: 100%;
}
.layout__sidebar-footer {
  margin-top: auto;
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border);
}
.layout__user {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  text-decoration: none;
  color: var(--text);
}
.layout__user-names {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.layout__user-display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.layout__user-acct {
  color: var(--text-muted);
  font-size: 0.85rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.layout__controls {
  display: flex;
  gap: var(--space-2);
  align-items: center;
}
.layout__controls--mobile {
  margin-top: var(--space-4);
}
.layout__locale {
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-1) var(--space-2);
  color: var(--text);
  font: inherit;
}
</style>
