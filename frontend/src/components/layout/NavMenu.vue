<script setup lang="ts">
export interface NavItem {
  id: string;
  label: string;
  /** Route target; absent → `action` semantics (emits `select`). */
  to?: string;
  badge?: number;
}

defineProps<{ items: NavItem[] }>();
const emit = defineEmits<{ select: [item: NavItem] }>();
</script>

<template>
  <nav class="navmenu" aria-label="Primary">
    <ul class="navmenu__list">
      <li v-for="item in items" :key="item.id" class="navmenu__item">
        <router-link
          v-if="item.to"
          :to="item.to"
          class="navmenu__link"
          @click="emit('select', item)"
        >
          <span class="navmenu__label">{{ item.label }}</span>
          <span
            v-if="item.badge"
            class="navmenu__badge"
            :data-badge-for="item.id"
            >{{ item.badge }}</span
          >
        </router-link>
        <button
          v-else
          type="button"
          class="navmenu__link navmenu__button"
          @click="emit('select', item)"
        >
          <span class="navmenu__label">{{ item.label }}</span>
          <span v-if="item.badge" class="navmenu__badge">{{ item.badge }}</span>
        </button>
      </li>
    </ul>
  </nav>
</template>

<style scoped>
.navmenu__list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.navmenu__link {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-2);
  width: 100%;
  padding: var(--space-2) var(--space-3);
  border-radius: var(--radius-md);
  color: var(--text-muted);
  text-decoration: none;
  background: none;
  border: 0;
  font: inherit;
  text-align: left;
  cursor: pointer;
}
.navmenu__link:hover {
  background: var(--bg-hover, var(--bg));
  color: var(--text);
}
.navmenu__link.router-link-active {
  color: var(--accent-link);
  font-weight: 600;
  background: var(--bg-active, var(--bg));
}
.navmenu__badge {
  min-width: 1.5em;
  padding: 0 var(--space-1);
  border-radius: var(--radius-sm);
  background: var(--accent);
  color: var(--accent-fg);
  font-size: var(--font-sm, 0.8125rem);
  font-weight: 700;
  text-align: center;
}
</style>
