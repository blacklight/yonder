import { createRouter, createWebHistory } from "vue-router";
import { i18n } from "@/i18n";
import { useAuthStore } from "@/stores/auth";
import HomePage from "@/pages/HomePage.vue";

declare module "vue-router" {
  interface RouteMeta {
    requiresAuth?: boolean;
    /** Minimum staff role: `moderator` admits moderators and admins. */
    requiresRole?: "moderator" | "admin";
    /** Page sets `document.title` itself once async data resolves. */
    dynamicTitle?: boolean;
  }
}

function stub(titleKey: string) {
  return {
    component: () => import("@/pages/ComingSoonPage.vue"),
    props: { titleKey },
  };
}

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: "/", name: "home", component: HomePage },
    {
      path: "/login",
      name: "sign-in",
      component: () => import("@/pages/SignInPage.vue"),
    },
    {
      // Old links/bookmarks keep working.
      path: "/sign-in",
      redirect: { name: "sign-in" },
    },
    {
      path: "/register",
      name: "register",
      component: () => import("@/pages/RegisterPage.vue"),
    },
    {
      // OAuth consent for client apps — the backend serves the SPA
      // here for `Accept: text/html`; anonymous visitors bounce to
      // sign-in and come back with the query intact.
      path: "/oauth/authorize",
      name: "oauth-authorize",
      component: () => import("@/pages/OAuthAuthorizePage.vue"),
      meta: { requiresAuth: true },
    },
    {
      path: "/home",
      name: "home-timeline",
      component: () => import("@/pages/TimelinePage.vue"),
      props: { timeline: "home", stream: "user", emptyKey: "home" },
      meta: { requiresAuth: true },
    },
    {
      path: "/local",
      name: "local",
      component: () => import("@/pages/TimelinePage.vue"),
      props: {
        timeline: "public:local",
        stream: "public:local",
        emptyKey: "local",
      },
    },
    {
      path: "/explore",
      name: "explore",
      component: () => import("@/pages/TimelinePage.vue"),
      props: { timeline: "public", stream: "public", emptyKey: "explore" },
    },
    {
      path: "/tags/:name",
      name: "tag",
      component: () => import("@/pages/TimelinePage.vue"),
      props: (r) => ({
        timeline: `hashtag:${r.params.name}`,
        stream: `hashtag:${r.params.name}`,
        emptyKey: "tag",
      }),
    },
    {
      path: "/lists/:id",
      name: "list",
      component: () => import("@/pages/TimelinePage.vue"),
      props: (r) => ({
        timeline: `list:${r.params.id}`,
        stream: `list:${r.params.id}`,
        emptyKey: "list",
      }),
      meta: { requiresAuth: true },
    },
    {
      path: "/compose",
      name: "compose",
      component: () => import("@/pages/ComposePage.vue"),
      meta: { requiresAuth: true },
    },
    {
      path: "/about",
      name: "about",
      ...stub("nav.about"),
    },
    {
      path: "/search",
      name: "search",
      component: () => import("@/pages/SearchPage.vue"),
    },
    {
      path: "/statuses/:id",
      name: "status",
      component: () => import("@/pages/StatusPage.vue"),
      props: true,
    },
    {
      path: "/notifications",
      name: "notifications",
      component: () => import("@/pages/NotificationsPage.vue"),
      meta: { requiresAuth: true },
    },
    {
      path: "/messages",
      name: "messages",
      component: () => import("@/pages/ConversationsPage.vue"),
      meta: { requiresAuth: true },
    },
    {
      path: "/messages/:id",
      name: "conversation",
      component: () => import("@/pages/ConversationThreadPage.vue"),
      props: true,
      meta: { requiresAuth: true },
    },
    {
      path: "/favourites",
      name: "favourites",
      component: () => import("@/pages/StatusListPage.vue"),
      props: { feed: "favourites" },
      meta: { requiresAuth: true },
    },
    {
      path: "/bookmarks",
      name: "bookmarks",
      component: () => import("@/pages/StatusListPage.vue"),
      props: { feed: "bookmarks" },
      meta: { requiresAuth: true },
    },
    {
      path: "/lists",
      name: "lists",
      component: () => import("@/pages/ListsPage.vue"),
      meta: { requiresAuth: true },
    },
    { path: "/hashtags", name: "hashtags", ...stub("nav.hashtags") },
    {
      path: "/moderation",
      name: "moderation",
      component: () => import("@/pages/moderation/ModerationReportsPage.vue"),
      meta: { requiresAuth: true, requiresRole: "moderator" },
    },
    {
      path: "/moderation/reports/:id",
      name: "moderation-report",
      component: () => import("@/pages/moderation/ModerationReportPage.vue"),
      props: true,
      meta: { requiresAuth: true, requiresRole: "moderator" },
    },
    {
      path: "/admin",
      component: () => import("@/pages/admin/AdminLayout.vue"),
      meta: { requiresAuth: true, requiresRole: "admin" },
      children: [
        { path: "", name: "admin", redirect: { name: "admin-general" } },
        {
          path: "general",
          name: "admin-general",
          component: () => import("@/pages/admin/AdminSettingsPage.vue"),
          props: {
            titleKey: "admin.nav.general",
            sections: ["auth", "admin", "timelines"],
            // PROMPT "General" keys that live in other sections —
            // they appear here *and* on their own page.
            extraKeys: [
              "content.max_post_length",
              "instance.directory_enabled",
            ],
          },
        },
        {
          path: "instance",
          name: "admin-instance",
          component: () => import("@/pages/admin/AdminSettingsPage.vue"),
          props: {
            titleKey: "admin.nav.instance",
            sections: ["instance", "i18n"],
          },
        },
        {
          path: "content",
          name: "admin-content",
          component: () => import("@/pages/admin/AdminSettingsPage.vue"),
          props: {
            titleKey: "admin.nav.content",
            sections: ["content"],
          },
        },
        {
          path: "federation",
          name: "admin-federation",
          component: () => import("@/pages/admin/AdminSettingsPage.vue"),
          props: {
            titleKey: "admin.nav.federation",
            sections: ["federation"],
          },
        },
        {
          path: "media",
          name: "admin-media",
          component: () => import("@/pages/admin/AdminSettingsPage.vue"),
          props: { titleKey: "admin.nav.media", sections: ["media"] },
        },
        {
          path: "email",
          name: "admin-email",
          component: () => import("@/pages/admin/AdminEmailPage.vue"),
        },
        {
          path: "polls",
          name: "admin-polls",
          component: () => import("@/pages/admin/AdminSettingsPage.vue"),
          props: { titleKey: "admin.nav.polls", sections: ["polls"] },
        },
        {
          path: "http",
          name: "admin-http",
          component: () => import("@/pages/admin/AdminHttpPage.vue"),
        },
        {
          path: "rate-limits",
          name: "admin-rate-limits",
          component: () => import("@/pages/admin/AdminSettingsPage.vue"),
          props: {
            titleKey: "admin.nav.rateLimits",
            sections: ["rate_limit"],
          },
        },
        {
          path: "search",
          name: "admin-search",
          component: () => import("@/pages/admin/AdminSettingsPage.vue"),
          props: { titleKey: "admin.nav.search", sections: ["search"] },
        },
        {
          // Read-only sections (server/database/redis/jobs reject
          // overlay writes) — listed so admins can see effective
          // values, sources and restart requirements.
          path: "server",
          name: "admin-server",
          component: () => import("@/pages/admin/AdminSettingsPage.vue"),
          props: {
            titleKey: "admin.nav.server",
            sections: ["server", "database", "redis", "jobs"],
          },
        },
        {
          path: "users",
          name: "admin-users",
          component: () => import("@/pages/admin/AdminUsersPage.vue"),
        },
        {
          path: "users/:id",
          name: "admin-user",
          component: () => import("@/pages/admin/AdminUserPage.vue"),
          props: true,
        },
        {
          path: "relays",
          name: "admin-relays",
          component: () => import("@/pages/admin/AdminRelaysPage.vue"),
        },
        {
          path: "domain-blocks",
          name: "admin-domain-blocks",
          component: () => import("@/pages/admin/AdminDomainBlocksPage.vue"),
        },
        {
          path: "emojis",
          name: "admin-emojis",
          component: () => import("@/pages/admin/AdminEmojisPage.vue"),
        },
        {
          path: "frontends",
          name: "admin-frontends",
          component: () => import("@/pages/admin/AdminFrontendsPage.vue"),
        },
        {
          path: "audit",
          name: "admin-audit",
          component: () => import("@/pages/admin/AdminAuditPage.vue"),
        },
      ],
    },
    {
      // `/@user` / `/@user@domain` — the canonical profile URL.
      path: "/@:acct",
      name: "profile-acct",
      component: () => import("@/pages/ProfilePage.vue"),
      // Title is the account's display name, known only after lookup.
      meta: { dynamicTitle: true },
    },
    {
      path: "/@:acct/followers",
      name: "profile-followers",
      component: () => import("@/pages/FollowListPage.vue"),
      props: { direction: "followers" },
      meta: { dynamicTitle: true },
    },
    {
      path: "/@:acct/following",
      name: "profile-following",
      component: () => import("@/pages/FollowListPage.vue"),
      props: { direction: "following" },
      meta: { dynamicTitle: true },
    },
    {
      // `/@user/{id}` — the `statuses.url` web form; the id is
      // authoritative, the acct decorative (Mastodon permalink shape).
      path: "/@:acct/:id(\\d+)",
      name: "status-acct",
      component: () => import("@/pages/StatusPage.vue"),
      props: true,
    },
    {
      // Numeric ids resolve on this alias only — `/@123` is a valid handle.
      path: "/accounts/:id",
      name: "profile-id",
      component: () => import("@/pages/ProfilePage.vue"),
      meta: { dynamicTitle: true },
    },
    {
      path: "/profile",
      name: "profile",
      component: () => import("@/pages/ProfilePage.vue"),
      // Waits for `auth.restore()` before resolving — a persisted
      // token may not have an account yet, so never redirect to
      // `/@undefined`.
      beforeEnter: async () => {
        const auth = useAuthStore();
        if (!auth.isLoggedIn) return { name: "sign-in" };
        await auth.restore();
        return auth.account ? `/@${auth.account.acct}` : true;
      },
    },
    {
      path: "/settings",
      name: "settings",
      component: () => import("@/pages/SettingsPage.vue"),
    },
    {
      path: "/settings/profile",
      name: "settings-profile",
      component: () => import("@/pages/settings/ProfileSettingsPage.vue"),
      meta: { requiresAuth: true },
    },
    {
      path: "/settings/security",
      name: "settings-security",
      component: () => import("@/pages/settings/SecuritySettingsPage.vue"),
      meta: { requiresAuth: true },
    },
    {
      // Invite links land on registration with the code prefilled —
      // `POST /api/v1/accounts` consumes it server-side.
      path: "/invite/:code",
      name: "invite",
      redirect: (to) => ({
        name: "register",
        query: { invite: to.params.code },
      }),
    },
    {
      path: "/:pathMatch(.*)*",
      name: "not-found",
      component: () => import("@/pages/NotFoundPage.vue"),
    },
  ],
});

// Routes carrying `meta.requiresAuth` bounce anonymous visitors to
// the sign-in page with a `redirect` back-reference. `meta.requiresRole`
// additionally waits for `auth.restore()` (the role is not persisted, so
// a reload has no role until verify_credentials returns) and sends
// under-privileged users home. The backend remains the authority.
router.beforeEach(async (to) => {
  const auth = useAuthStore();
  if (to.meta.requiresAuth && !auth.isLoggedIn) {
    return {
      name: "sign-in",
      query: { redirect: to.fullPath },
    };
  }
  if (to.meta.requiresRole) {
    await auth.restore();
    const allowed =
      to.meta.requiresRole === "admin" ? auth.isAdmin : auth.isStaff;
    if (!allowed) return { name: "home" };
  }
  return true;
});

router.afterEach((to) => {
  if (to.meta.dynamicTitle) return;
  const key = `route.${String(to.name ?? "home")}.title`;
  const app = i18n.global.t("app.name");
  const page = i18n.global.te(key) ? i18n.global.t(key) : "";
  document.title = page ? `${page} — ${app}` : app;
});
