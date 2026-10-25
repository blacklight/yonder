import { createRouter, createWebHistory } from "vue-router";
import { i18n } from "@/i18n";
import HomePage from "@/pages/HomePage.vue";

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: "/", name: "home", component: HomePage },
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

router.afterEach((to) => {
  const key = `route.${String(to.name ?? "home")}.title`;
  document.title = `${i18n.global.t(key)} — ${i18n.global.t("app.name")}`;
});
