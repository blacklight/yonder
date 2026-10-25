<script setup lang="ts">
import { nextTick, onUnmounted, ref, useId, watch } from "vue";
import type { IconProp } from "@fortawesome/fontawesome-svg-core";
import { FontAwesomeIcon } from "@/icons";

export interface MenuItem {
  id: string;
  label: string;
  icon?: IconProp;
  disabled?: boolean;
}

const props = defineProps<{ items: MenuItem[]; label: string }>();
const emit = defineEmits<{ select: [item: MenuItem] }>();

const open = ref(false);
const activeIndex = ref(-1);
const root = ref<HTMLElement | null>(null);
const trigger = ref<HTMLButtonElement | null>(null);
const list = ref<HTMLUListElement | null>(null);
const uid = useId();

const itemId = (i: number) => `menu-${uid}-item-${i}`;
const enabledIndexes = () =>
  props.items
    .map((item, idx) => ({ item, idx }))
    .filter((x) => !x.item.disabled);

async function openMenu(fromKey?: "down" | "up") {
  open.value = true;
  const enabled = enabledIndexes();
  activeIndex.value = fromKey
    ? (enabled[fromKey === "up" ? enabled.length - 1 : 0]?.idx ?? -1)
    : -1;
  await nextTick();
  list.value?.focus();
}

function closeMenu(refocus: boolean) {
  open.value = false;
  activeIndex.value = -1;
  if (refocus) trigger.value?.focus();
}

function toggle() {
  if (open.value) closeMenu(false);
  else openMenu();
}

function choose(item: MenuItem, index: number) {
  if (item.disabled) return;
  emit("select", item);
  activeIndex.value = index;
  closeMenu(true);
}

function onKeydown(event: KeyboardEvent) {
  const enabled = enabledIndexes();
  if (!enabled.length) return;
  const current = enabled.findIndex((x) => x.idx === activeIndex.value);
  if (event.key === "ArrowDown") {
    event.preventDefault();
    activeIndex.value = enabled[(current + 1) % enabled.length].idx;
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    activeIndex.value =
      enabled[(current - 1 + enabled.length) % enabled.length].idx;
  } else if (event.key === "Enter" || event.key === " ") {
    if (activeIndex.value >= 0) {
      event.preventDefault();
      choose(props.items[activeIndex.value], activeIndex.value);
    }
  } else if (event.key === "Escape") {
    event.preventDefault();
    closeMenu(true);
  } else if (event.key === "Tab") {
    closeMenu(false);
  }
}

function onDocumentPointerdown(event: Event) {
  if (!root.value?.contains(event.target as Node)) closeMenu(false);
}

watch(open, (v) => {
  const method = v ? "addEventListener" : "removeEventListener";
  document[method]("pointerdown", onDocumentPointerdown);
});
onUnmounted(() =>
  document.removeEventListener("pointerdown", onDocumentPointerdown),
);
</script>

<template>
  <div ref="root" class="menu">
    <button
      ref="trigger"
      class="menu__trigger"
      :aria-label="label"
      aria-haspopup="menu"
      :aria-expanded="open"
      @click="toggle"
      @keydown.down.prevent="openMenu('down')"
      @keydown.up.prevent="openMenu('up')"
    >
      <slot name="trigger"><FontAwesomeIcon icon="ellipsis" /></slot>
    </button>
    <ul
      v-if="open"
      ref="list"
      class="menu__list"
      role="menu"
      :aria-label="label"
      tabindex="-1"
      :aria-activedescendant="
        activeIndex >= 0 ? itemId(activeIndex) : undefined
      "
      @keydown="onKeydown"
    >
      <li
        v-for="(item, i) in items"
        :id="itemId(i)"
        :key="item.id"
        role="menuitem"
        class="menu__item"
        :class="{
          'menu__item--active': i === activeIndex,
          'menu__item--disabled': item.disabled,
        }"
        :aria-disabled="item.disabled || undefined"
        @click="choose(item, i)"
        @mouseenter="activeIndex = i"
      >
        <FontAwesomeIcon
          v-if="item.icon"
          :icon="item.icon"
          class="menu__icon"
          fixed-width
        />
        {{ item.label }}
      </li>
    </ul>
  </div>
</template>

<style scoped>
.menu {
  position: relative;
  display: inline-block;
}
.menu__list {
  position: absolute;
  top: 100%;
  left: 0;
  margin: var(--space-1) 0 0;
  padding: var(--space-1);
  list-style: none;
  min-width: 10rem;
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.18);
  z-index: 50;
}
.menu__list:focus-visible {
  outline: var(--focus-ring);
}
.menu__item {
  padding: var(--space-2) var(--space-3);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.menu__icon {
  margin-right: var(--space-2);
  color: var(--text-muted);
}
.menu__item--active {
  background: var(--accent-soft);
}
.menu__item--disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
