<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";
import NavMenu, { type NavItem } from "@/components/layout/NavMenu.vue";

const { t } = useI18n();

// Section order matches the PROMPT's admin-page list; settings pages
// first, then the management surfaces.
const items = computed<NavItem[]>(() => [
  { id: "general", label: t("admin.nav.general"), to: "/admin/general" },
  { id: "instance", label: t("admin.nav.instance"), to: "/admin/instance" },
  { id: "content", label: t("admin.nav.content"), to: "/admin/content" },
  {
    id: "federation",
    label: t("admin.nav.federation"),
    to: "/admin/federation",
  },
  { id: "media", label: t("admin.nav.media"), to: "/admin/media" },
  { id: "email", label: t("admin.nav.email"), to: "/admin/email" },
  { id: "polls", label: t("admin.nav.polls"), to: "/admin/polls" },
  { id: "http", label: t("admin.nav.http"), to: "/admin/http" },
  {
    id: "rateLimits",
    label: t("admin.nav.rateLimits"),
    to: "/admin/rate-limits",
  },
  { id: "search", label: t("admin.nav.search"), to: "/admin/search" },
  { id: "server", label: t("admin.nav.server"), to: "/admin/server" },
  { id: "users", label: t("admin.nav.users"), to: "/admin/users" },
  { id: "relays", label: t("admin.nav.relays"), to: "/admin/relays" },
  {
    id: "domainBlocks",
    label: t("admin.nav.domainBlocks"),
    to: "/admin/domain-blocks",
  },
  { id: "emojis", label: t("admin.nav.emojis"), to: "/admin/emojis" },
  {
    id: "frontends",
    label: t("admin.nav.frontends"),
    to: "/admin/frontends",
  },
  { id: "audit", label: t("admin.nav.audit"), to: "/admin/audit" },
]);
</script>

<template>
  <main class="page admin">
    <h1>{{ t("nav.admin") }}</h1>
    <div class="admin__body">
      <NavMenu :items="items" class="admin__nav" />
      <div class="admin__content">
        <router-view />
      </div>
    </div>
  </main>
</template>

<style scoped>
.admin__body {
  display: grid;
  grid-template-columns: minmax(10rem, 13rem) minmax(0, 1fr);
  gap: var(--space-6);
  align-items: start;
}
.admin__nav {
  position: sticky;
  top: var(--space-4);
}
.admin__content {
  min-width: 0;
}
@media (max-width: 768px) {
  .admin__body {
    grid-template-columns: 1fr;
  }
  .admin__nav {
    position: static;
  }
}
</style>
