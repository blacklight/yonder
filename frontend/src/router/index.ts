import { createRouter, createWebHistory } from "vue-router";
import { i18n } from "@/i18n";
import { useAuthStore } from "@/stores/auth";
import HomePage from "@/pages/HomePage.vue";

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
      path: "/sign-in",
      name: "sign-in",
      component: () => import("@/pages/SignInPage.vue"),
    },
    {
      path: "/register",
      name: "register",
      component: () => import("@/pages/RegisterPage.vue"),
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
    { path: "/moderation", name: "moderation", ...stub("nav.moderation") },
    { path: "/admin", name: "admin", ...stub("nav.admin") },
    {
      // `/@user` / `/@user@domain` — the canonical profile URL.
      path: "/@:acct",
      name: "profile-acct",
      component: () => import("@/pages/ProfilePage.vue"),
    },
    {
      // Numeric ids resolve on this alias only — `/@123` is a valid handle.
      path: "/accounts/:id",
      name: "profile-id",
      component: () => import("@/pages/ProfilePage.vue"),
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
// the sign-in page with a `redirect` back-reference.
router.beforeEach((to) => {
  if (to.meta.requiresAuth && !useAuthStore().isLoggedIn) {
    return {
      name: "sign-in",
      query: { redirect: to.fullPath },
    };
  }
  return true;
});

router.afterEach((to) => {
  const key = `route.${String(to.name ?? "home")}.title`;
  document.title = `${i18n.global.t(key)} — ${i18n.global.t("app.name")}`;
});
