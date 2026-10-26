import { mount } from "@vue/test-utils";
import {
  createMemoryHistory,
  createRouter,
  type RouteRecordRaw,
} from "vue-router";
import { i18n } from "@/i18n";
import type { Component } from "vue";

const stub = { template: "<div />" };

export const stubRoutes: RouteRecordRaw[] = [
  { path: "/", name: "home", component: stub },
  { path: "/login", name: "sign-in", component: stub },
  { path: "/register", name: "register", component: stub },
  { path: "/settings", name: "settings", component: stub },
  { path: "/compose", name: "compose", component: stub },
  { path: "/local", name: "local", component: stub },
  { path: "/explore", name: "explore", component: stub },
  { path: "/notifications", name: "notifications", component: stub },
  { path: "/favourites", name: "favourites", component: stub },
  { path: "/bookmarks", name: "bookmarks", component: stub },
  { path: "/lists", name: "lists", component: stub },
  { path: "/hashtags", name: "hashtags", component: stub },
  { path: "/moderation", name: "moderation", component: stub },
  {
    path: "/moderation/reports/:id",
    name: "moderation-report",
    component: stub,
  },
  { path: "/admin", name: "admin", component: stub },
  { path: "/profile", name: "profile", component: stub },
  { path: "/@:acct", name: "profile-acct", component: stub },
  { path: "/accounts/:id", name: "profile-id", component: stub },
  { path: "/search", name: "search", component: stub },
  { path: "/about", name: "about", component: stub },
  { path: "/tags/:name", name: "tag", component: stub },
  { path: "/lists/:id", name: "list", component: stub },
];

/** Mount a component with the real i18n and a memory router. */
export function mountWith(
  component: Component,
  options: {
    props?: Record<string, unknown>;
    slots?: Record<string, string>;
    global?: Record<string, unknown>;
  } = {},
) {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: stubRoutes,
  });
  const wrapper = mount(
    component as never,
    {
      ...options,
      global: {
        plugins: [i18n, router],
        ...(options.global ?? {}),
      },
    } as never,
  );
  return { wrapper, router };
}
