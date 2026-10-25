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
    { path: "/search", name: "search", ...stub("nav.search") },
    {
      path: "/notifications",
      name: "notifications",
      ...stub("nav.notifications"),
    },
    { path: "/favourites", name: "favourites", ...stub("nav.favourites") },
    { path: "/bookmarks", name: "bookmarks", ...stub("nav.bookmarks") },
    { path: "/lists", name: "lists", ...stub("nav.lists") },
    { path: "/hashtags", name: "hashtags", ...stub("nav.hashtags") },
    { path: "/moderation", name: "moderation", ...stub("nav.moderation") },
    { path: "/admin", name: "admin", ...stub("nav.admin") },
    { path: "/profile", name: "profile", ...stub("nav.profile") },
    {
      path: "/settings",
      name: "settings",
      component: () => import("@/pages/SettingsPage.vue"),
    },
    {
      // The backend's /api/v1/invites flows link to this route; the
      // invite acceptance UI lands in epic 25.
      path: "/invite/:code",
      name: "invite",
      component: () => import("@/pages/InvitePage.vue"),
      props: true,
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
