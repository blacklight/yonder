<script setup lang="ts">
import { computed, onMounted, ref, useTemplateRef, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import Avatar from "@/components/ui/Avatar.vue";
import Button from "@/components/ui/Button.vue";
import NavMenu, { type NavItem } from "./NavMenu.vue";
import MobileMenu from "./MobileMenu.vue";
import QuoteComposeModal from "@/components/compose/QuoteComposeModal.vue";
import { FontAwesomeIcon } from "@/icons";
import { useTheme } from "@/composables/useTheme";
import { getInstance } from "@/api/instance";
import type { InstanceEntity } from "@/api/entities";
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

// Upstream source repository — shown in the wide-screen footer.
const SOURCE_URL = "https://git.platypush.tech/blacklight/yonder";

const menuOpen = ref(false);
const searchQuery = ref("");
const instance = ref<InstanceEntity | null>(null);
const instanceTitle = computed(() => instance.value?.title ?? null);

onMounted(() => {
  void getInstance()
    .then((i) => (instance.value = i))
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
// `to` carry a `run` action (sign out).
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
    to: "/login",
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

// Inside the DM area the compose affordances become "New message" —
// they seed a direct draft instead of a public one. An in-progress
// draft is never clobbered: a dirty draft just resumes on /compose.
const inMessages = computed(() => route.path.startsWith("/messages"));
const seedsDirect = computed(() => inMessages.value && !compose.isDirty);
const composeLabel = computed(() =>
  seedsDirect.value ? t("messages.newMessage") : t("nav.compose"),
);

function seedDirect() {
  if (seedsDirect.value) compose.openDirect();
}

function composeAction() {
  seedDirect();
  void router.push("/compose");
}

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

// Inside /admin the app menu is replaced by the admin panel menu —
// rendering both (sidebar + in-page nav) was too cluttered, and the
// panel menu is what the admin actually needs while section-hopping.
// The "back" entry returns to the main app.
const inAdmin = computed(() => route.matched.some((r) => r.path === "/admin"));

const adminItems = computed<NavItem[]>(() => [
  { id: "back", label: t("admin.nav.back"), to: "/", icon: "arrow-left" },
  {
    id: "general",
    label: t("admin.nav.general"),
    to: "/admin/general",
    icon: "gear",
  },
  {
    id: "instance",
    label: t("admin.nav.instance"),
    to: "/admin/instance",
    icon: "server",
  },
  {
    id: "content",
    label: t("admin.nav.content"),
    to: "/admin/content",
    icon: "note-sticky",
  },
  {
    id: "federation",
    label: t("admin.nav.federation"),
    to: "/admin/federation",
    icon: "share-nodes",
  },
  {
    id: "media",
    label: t("admin.nav.media"),
    to: "/admin/media",
    icon: "image",
  },
  {
    id: "email",
    label: t("admin.nav.email"),
    to: "/admin/email",
    icon: "envelope",
  },
  {
    id: "polls",
    label: t("admin.nav.polls"),
    to: "/admin/polls",
    icon: "square-poll-vertical",
  },
  { id: "http", label: t("admin.nav.http"), to: "/admin/http", icon: "globe" },
  {
    id: "rateLimits",
    label: t("admin.nav.rateLimits"),
    to: "/admin/rate-limits",
    icon: "gauge-high",
  },
  {
    id: "search",
    label: t("admin.nav.search"),
    to: "/admin/search",
    icon: "magnifying-glass",
  },
  {
    id: "server",
    label: t("admin.nav.server"),
    to: "/admin/server",
    icon: "database",
  },
  {
    id: "users",
    label: t("admin.nav.users"),
    to: "/admin/users",
    icon: "users",
  },
  {
    id: "relays",
    label: t("admin.nav.relays"),
    to: "/admin/relays",
    icon: "satellite-dish",
  },
  {
    id: "domainBlocks",
    label: t("admin.nav.domainBlocks"),
    to: "/admin/domain-blocks",
    icon: "ban",
  },
  {
    id: "emojis",
    label: t("admin.nav.emojis"),
    to: "/admin/emojis",
    icon: "face-smile",
  },
  {
    id: "frontends",
    label: t("admin.nav.frontends"),
    to: "/admin/frontends",
    icon: "display",
  },
  {
    id: "audit",
    label: t("admin.nav.audit"),
    to: "/admin/audit",
    icon: "list-ul",
  },
]);

const navItems = computed<NavItem[]>(() =>
  inAdmin.value ? adminItems.value : menuItems.value,
);

// The mobile drawer gets a compose entry — the big sidebar button is
// desktop-only, so without this there is no way to post on mobile.
const mobileItems = computed<NavItem[]>(() =>
  inAdmin.value
    ? adminItems.value
    : auth.isLoggedIn
      ? [
          {
            id: "compose",
            label: composeLabel.value,
            to: "/compose",
            icon: "pen-to-square",
          },
          ...menuItems.value,
        ]
      : menuItems.value,
);

function onSelect(item: NavItem) {
  menuOpen.value = false;
  // The drawer's compose entry isn't in `menuSource` — its `to` link
  // navigates by itself, but the direct-draft seeding still applies.
  if (item.id === "compose") seedDirect();
  else menuSource.value.find((m) => m.id === item.id)?.run?.();
}

async function setLocale(code: string) {
  const { applyLocale } = await import("@/i18n");
  await applyLocale(code);
}

function search() {
  const q = searchQuery.value.trim();
  if (q) void router.push({ name: "search", query: { q } });
}

const sidebarSearch = useTemplateRef<HTMLInputElement>("sidebarSearch");
const asideSearch = useTemplateRef<HTMLInputElement>("asideSearch");

function clearSearch(which: "sidebar" | "aside") {
  searchQuery.value = "";
  (which === "sidebar" ? sidebarSearch : asideSearch).value?.focus();
}
</script>

<template>
  <div class="layout">
    <aside class="layout__sidebar">
      <router-link to="/" class="layout__brand">
        <img src="/logo.svg" alt="Yonder" class="layout__logo" />
        <span>{{ instanceTitle ?? t("app.name") }}</span>
      </router-link>

      <form
        class="layout__search layout__search--sidebar"
        role="search"
        @submit.prevent="search"
      >
        <FontAwesomeIcon icon="magnifying-glass" class="layout__search-icon" />
        <input
          ref="sidebarSearch"
          v-model="searchQuery"
          type="search"
          class="layout__search-input"
          :placeholder="t('nav.searchPlaceholder')"
          :aria-label="t('nav.search')"
        />
        <button
          v-if="searchQuery"
          type="button"
          class="layout__search-clear"
          :aria-label="t('common.clear')"
          :title="t('common.clear')"
          @click="clearSearch('sidebar')"
        >
          <FontAwesomeIcon icon="xmark" />
        </button>
      </form>

      <Button
        v-if="auth.isLoggedIn && !inAdmin"
        class="layout__compose"
        data-testid="compose"
        @click="composeAction"
      >
        <FontAwesomeIcon icon="pen-to-square" />
        {{ composeLabel }}</Button
      >

      <NavMenu :items="navItems" @select="onSelect" />

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
        <div class="layout__controls layout__controls--sidebar">
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
      <div
        v-if="auth.account?.yonder?.verification_pending"
        class="layout__notice"
        role="status"
      >
        <FontAwesomeIcon icon="envelope" />
        {{ t("auth.verificationPending") }}
      </div>
      <slot />
    </main>

    <aside class="layout__aside">
      <form class="layout__search" role="search" @submit.prevent="search">
        <FontAwesomeIcon icon="magnifying-glass" class="layout__search-icon" />
        <input
          ref="asideSearch"
          v-model="searchQuery"
          type="search"
          class="layout__search-input"
          :placeholder="t('nav.searchPlaceholder')"
          :aria-label="t('nav.search')"
        />
        <button
          v-if="searchQuery"
          type="button"
          class="layout__search-clear"
          :aria-label="t('common.clear')"
          :title="t('common.clear')"
          @click="clearSearch('aside')"
        >
          <FontAwesomeIcon icon="xmark" />
        </button>
      </form>

      <div class="layout__aside-footer">
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
        <div class="layout__meta">
          <span v-if="instance?.version" class="layout__meta-version">{{
            instance.version
          }}</span>
          <span v-if="instance?.domain" class="layout__meta-domain">{{
            instance.domain
          }}</span>
        </div>
        <nav class="layout__meta-links" :aria-label="t('footer.links')">
          <router-link to="/about">{{ t("nav.about") }}</router-link>
          <a :href="SOURCE_URL" target="_blank" rel="noopener">{{
            t("footer.source")
          }}</a>
        </nav>
      </div>
    </aside>

    <QuoteComposeModal
      v-if="compose.quoteTarget"
      :status="compose.quoteTarget"
    />
  </div>
</template>

<style scoped>
.layout {
  min-height: 100vh;
}
.layout__sidebar,
.layout__aside {
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
.layout__notice {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-3) var(--space-4);
  margin-bottom: var(--space-4);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-raised);
}

@media (min-width: 768px) {
  .layout {
    display: flex;
    align-items: flex-start;
    justify-content: center;
  }
  .layout__topbar {
    display: none;
  }
  .layout__sidebar {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    position: sticky;
    top: 0;
    height: 100vh;
    width: 15rem;
    flex: none;
    padding: var(--space-4);
    overflow-y: auto;
  }
  .layout__main {
    flex: 0 1 44rem;
    margin: 0;
    min-height: 100vh;
  }
}

.layout__brand {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: var(--space-2);
  font-weight: 700;
  color: var(--text);
  text-decoration: none;
  font-size: 1.5rem;
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
  padding-inline-end: 2rem;
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg);
  color: var(--text);
  font: inherit;
}
/* The clear button replaces the native WebKit search cancel. */
.layout__search-input::-webkit-search-cancel-button {
  appearance: none;
}
.layout__search-clear {
  position: absolute;
  inset-inline-end: var(--space-1);
  top: 50%;
  transform: translateY(-50%);
  display: grid;
  place-items: center;
  width: 1.75rem;
  height: 1.75rem;
  padding: 0;
  border: none;
  border-radius: 50%;
  background: transparent;
  color: var(--text-muted);
  cursor: pointer;
}
.layout__search-clear:hover {
  background: var(--bg-sunken);
  color: var(--text);
}
.layout__compose {
  width: 100%;
  margin: 1rem 0;
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
.layout__aside-footer {
  margin-top: auto;
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  padding-top: var(--space-3);
}
.layout__meta {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
  color: var(--text-muted);
  font-size: 0.85rem;
}
.layout__meta > * + *::before {
  content: "·";
  margin-right: var(--space-2);
}
.layout__meta-links {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-3);
  font-size: 0.85rem;
}
.layout__meta-links a {
  color: var(--text-muted);
  text-decoration: none;
}
.layout__meta-links a:hover {
  color: var(--accent-link);
  text-decoration: underline;
}

/* Wide screens: the right pane takes over search + theme/language, so
   the sidebar copies hide. This block must come last — equal-specificity
   base rules (`.layout__controls`, `.layout__search`) would otherwise
   win on source order even when the media query matches. */
@media (min-width: 1280px) {
  .layout__aside {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    position: sticky;
    top: 0;
    height: 100vh;
    width: 19rem;
    flex: none;
    padding: var(--space-4);
    overflow-y: auto;
  }
  .layout__search--sidebar,
  .layout__controls--sidebar {
    display: none;
  }
}
</style>

<style>
/* Always counter-compensate horizontal padding for timelines on mobile
   (app-like experience) */
@media (max-width: 767px) {
  .timeline {
    margin: 0 calc(var(--space-4) * -1);
  }
}
</style>
