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
// The list is teleported to <body> — ancestors with `overflow: hidden`
// (status cards, layout shells) clip `position: absolute` menus no
// matter the z-index. Fixed coords are computed from the trigger rect.
const menuStyle = ref<Record<string, string>>({});
const uid = useId();

const itemId = (i: number) => `menu-${uid}-item-${i}`;
const enabledIndexes = () =>
  props.items
    .map((item, idx) => ({ item, idx }))
    .filter((x) => !x.item.disabled);

// Viewport-aware placement: drop-down under the trigger by default,
// flip up when there isn't room below, and align/clamp horizontally so
// the menu never overflows the canvas edge on small screens.
function place() {
  const rect = trigger.value?.getBoundingClientRect();
  const el = list.value;
  if (!rect || !el) return;
  const vw = document.documentElement.clientWidth;
  const vh = document.documentElement.clientHeight;
  const w = el.offsetWidth;
  const h = el.offsetHeight;
  let left = rect.left;
  if (left + w > vw - 8) left = rect.right - w;
  left = Math.min(Math.max(8, left), Math.max(8, vw - w - 8));
  let top = rect.bottom + 4;
  if (top + h > vh - 8 && rect.top - 4 - h >= 8) top = rect.top - 4 - h;
  top = Math.min(Math.max(8, top), Math.max(8, vh - h - 8));
  menuStyle.value = { top: `${top}px`, left: `${left}px` };
}

async function openMenu(fromKey?: "down" | "up") {
  open.value = true;
  const enabled = enabledIndexes();
  activeIndex.value = fromKey
    ? (enabled[fromKey === "up" ? enabled.length - 1 : 0]?.idx ?? -1)
    : -1;
  await nextTick();
  place();
  // Under VTU's teleport stub the ref may bind to a stub node —
  // fall back to the live DOM list.
  const focusTarget =
    (list.value instanceof HTMLUListElement && list.value.isConnected
      ? list.value
      : null) ?? document.querySelector<HTMLElement>("[role='menu']");
  focusTarget?.focus();
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
  const target = event.target as Node;
  if (!root.value?.contains(target) && !list.value?.contains(target))
    closeMenu(false);
}

function onScrollClose(event: Event) {
  if (list.value?.contains(event.target as Node)) return;
  closeMenu(false);
}

watch(open, (v) => {
  const method = v ? "addEventListener" : "removeEventListener";
  document[method]("pointerdown", onDocumentPointerdown);
  window[method]("resize", place);
  // Fixed coords go stale when the page or any scroller moves —
  // closing is cheaper than chasing every scroll container. Capture
  // covers inner scrollers, which don't bubble.
  if (v) window.addEventListener("scroll", onScrollClose, true);
  else window.removeEventListener("scroll", onScrollClose, true);
});
onUnmounted(() => {
  document.removeEventListener("pointerdown", onDocumentPointerdown);
  window.removeEventListener("resize", place);
  window.removeEventListener("scroll", onScrollClose, true);
});
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
    <Teleport to="body">
      <ul
        v-if="open"
        :key="`menu-${uid}-list`"
        ref="list"
        class="menu__list"
        :style="menuStyle"
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
    </Teleport>
  </div>
</template>

<style scoped>
.menu {
  position: relative;
  display: inline-block;
}
.menu__trigger {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: var(--space-2) var(--space-3);
  border: none;
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--text);
  cursor: pointer;
}
.menu__trigger:hover {
  background: var(--bg-sunken);
}
.menu__list {
  width: fit-content;
  max-width: min(20rem, calc(100vw - 16px));
  position: fixed;
  font-size: var(--text-sm);
  margin: var(--space-1) 0 0;
  padding: var(--space-1);
  list-style: none;
  min-width: 10rem;
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.18);
  z-index: 300;
}
.menu__list:focus-visible {
  outline: var(--focus-ring);
}
.menu__item {
  display: flex;
  align-items: center;
  gap: var(--space-2);
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
