<script setup lang="ts">
import Avatar from "@/components/ui/Avatar.vue";
import { FontAwesomeIcon } from "@/icons";
import type { NavItem } from "./NavMenu.vue";

export interface BottomNavItem extends Omit<NavItem, "badge"> {
  /** Bottom-bar entries always navigate. */
  to: string;
  /** Unread count bubble; `true` renders a bare dot (e.g. new DMs). */
  badge?: number | boolean;
  /** Render the viewer's avatar instead of an icon (profile shortcut). */
  avatar?: boolean;
  avatarSrc?: string;
  /**
   * Force the active styling when route matching can't see it —
   * e.g. `/profile` redirects to `/@acct`, and `/` renders the same
   * home timeline as `/home`.
   */
  active?: boolean;
}

defineProps<{ items: BottomNavItem[] }>();
</script>

<template>
  <nav class="bottomnav" aria-label="Primary">
    <ul class="bottomnav__list">
      <li v-for="item in items" :key="item.id" class="bottomnav__item">
        <router-link
          :to="item.to"
          class="bottomnav__link"
          :class="{ 'bottomnav__link--active': item.active }"
          :aria-label="item.label"
          :title="item.label"
        >
          <span class="bottomnav__iconwrap">
            <Avatar
              v-if="item.avatar"
              :src="item.avatarSrc"
              :name="item.label"
              size="sm"
            />
            <FontAwesomeIcon
              v-else-if="item.icon"
              :icon="item.icon"
              class="bottomnav__icon"
            />
            <span
              v-if="item.badge"
              class="bottomnav__badge"
              :class="{ 'bottomnav__badge--dot': item.badge === true }"
              aria-hidden="true"
              >{{ typeof item.badge === "number" ? item.badge : "" }}</span
            >
          </span>
        </router-link>
      </li>
    </ul>
  </nav>
</template>

<style scoped>
.bottomnav {
  position: fixed;
  bottom: 0;
  left: 0;
  right: 0;
  z-index: 30;
  background: var(--bg-raised);
  border-top: 1px solid var(--border);
  padding-bottom: env(safe-area-inset-bottom);
}
.bottomnav__list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
}
.bottomnav__item {
  flex: 1;
}
.bottomnav__link {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 3.25rem;
  color: var(--text-muted);
  text-decoration: none;
}
.bottomnav__link:hover {
  background: var(--bg-hover, var(--bg));
  color: var(--text);
}
.bottomnav__link.router-link-active,
.bottomnav__link--active {
  color: var(--accent-link);
}
.bottomnav__link.router-link-active :deep(.avatar),
.bottomnav__link--active :deep(.avatar) {
  box-shadow: 0 0 0 2px var(--accent);
}
.bottomnav__iconwrap {
  position: relative;
  display: inline-flex;
}
.bottomnav__icon {
  font-size: 1.3rem;
}
.bottomnav__badge {
  position: absolute;
  top: -0.4rem;
  inset-inline-end: -0.6rem;
  min-width: 1rem;
  padding: 0 var(--space-1);
  border-radius: var(--radius-full);
  background: var(--accent);
  color: var(--accent-fg);
  font-size: 0.65rem;
  font-weight: 700;
  line-height: 1rem;
  text-align: center;
}
.bottomnav__badge--dot {
  min-width: 0;
  width: 0.6rem;
  height: 0.6rem;
  padding: 0;
  top: -0.15rem;
  inset-inline-end: -0.3rem;
}
@media (min-width: 768px) {
  .bottomnav {
    display: none;
  }
}
</style>
