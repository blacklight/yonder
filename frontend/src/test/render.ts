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
  { path: "/verify-email/:token", name: "verify-email", component: stub },
  { path: "/settings", name: "settings", component: stub },
  {
    path: "/settings/profile",
    name: "settings-profile",
    component: stub,
  },
  {
    path: "/settings/security",
    name: "settings-security",
    component: stub,
  },
  {
    path: "/settings/tokens",
    name: "settings-tokens",
    component: stub,
  },
  {
    path: "/settings/invites",
    name: "settings-invites",
    component: stub,
  },
  { path: "/compose", name: "compose", component: stub },
  { path: "/local", name: "local", component: stub },
  { path: "/explore", name: "explore", component: stub },
  { path: "/notifications", name: "notifications", component: stub },
  { path: "/messages", name: "messages", component: stub },
  { path: "/messages/:id", name: "conversation", component: stub },
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
  { path: "/admin/users", name: "admin-users", component: stub },
  { path: "/admin/users/:id", name: "admin-user", component: stub },
  { path: "/profile", name: "profile", component: stub },
  { path: "/@:acct", name: "profile-acct", component: stub },
  {
    path: "/@:acct/followers",
    name: "profile-followers",
    component: stub,
  },
  {
    path: "/@:acct/following",
    name: "profile-following",
    component: stub,
  },
  { path: "/accounts/:id", name: "profile-id", component: stub },
  { path: "/statuses/:id", name: "status", component: stub },
  {
    path: "/statuses/:id/boosts",
    name: "status-boosts",
    component: stub,
  },
  {
    path: "/statuses/:id/quotes",
    name: "status-quotes",
    component: stub,
  },
  {
    path: "/statuses/:id/favourites",
    name: "status-favourites",
    component: stub,
  },
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
