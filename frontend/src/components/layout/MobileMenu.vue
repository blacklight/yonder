<script setup lang="ts">
import { nextTick, ref, watch, onBeforeUnmount } from "vue";
import { useI18n } from "vue-i18n";

const props = defineProps<{ open: boolean }>();
const emit = defineEmits<{ close: [] }>();
const { t } = useI18n();

const panel = ref<HTMLElement | null>(null);
const closeBtn = ref<HTMLButtonElement | null>(null);
let previousFocus: Element | null = null;

const FOCUSABLE =
  "a[href], button:not([disabled]), input, select, textarea, [tabindex]:not([tabindex='-1'])";

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") {
    emit("close");
    return;
  }
  // `aria-modal` promises the dialog owns keyboard focus — wrap Tab
  // at both ends of the panel's focusable set.
  if (e.key !== "Tab" || !panel.value) return;
  // The panel is only rendered while open, so everything matching is
  // visible — no need to filter by layout boxes (also keeps jsdom
  // honest, where offsetParent is always null).
  const items = Array.from(
    panel.value.querySelectorAll<HTMLElement>(FOCUSABLE),
  );
  if (items.length === 0) return;
  const first = items[0];
  const last = items[items.length - 1];
  if (e.shiftKey && document.activeElement === first) {
    e.preventDefault();
    last.focus();
  } else if (!e.shiftKey && document.activeElement === last) {
    e.preventDefault();
    first.focus();
  }
}

// Escape handling lives on the document while the panel is open;
// focus moves into the dialog on open and back out on close.
watch(
  () => props.open,
  async (open) => {
    if (open) {
      previousFocus = document.activeElement;
      document.addEventListener("keydown", onKeydown);
      await nextTick();
      closeBtn.value?.focus();
    } else {
      document.removeEventListener("keydown", onKeydown);
      if (previousFocus instanceof HTMLElement) previousFocus.focus();
      previousFocus = null;
    }
  },
  // Mounted already-open (tests, future embeds) needs the same
  // focus/listener setup as a false→true flip.
  { immediate: true },
);
onBeforeUnmount(() => document.removeEventListener("keydown", onKeydown));
</script>

<template>
  <Teleport to="body">
    <div v-if="open" class="mobile-menu" data-testid="mobile-menu">
      <button
        type="button"
        class="mobile-menu__overlay"
        :aria-label="t('ui.close')"
        @click="emit('close')"
      />
      <div
        ref="panel"
        class="mobile-menu__panel"
        role="dialog"
        aria-modal="true"
        :aria-label="t('menu.title')"
      >
        <button
          ref="closeBtn"
          type="button"
          class="mobile-menu__close"
          :aria-label="t('ui.close')"
          @click="emit('close')"
        >
          ✕
        </button>
        <slot />
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.mobile-menu {
  position: fixed;
  inset: 0;
  z-index: 100;
}
.mobile-menu__overlay {
  position: absolute;
  inset: 0;
  width: 100%;
  border: 0;
  background: rgb(0 0 0 / 45%);
  cursor: default;
}
.mobile-menu__panel {
  position: absolute;
  top: 0;
  bottom: 0;
  left: 0;
  width: min(20rem, 85vw);
  padding: var(--space-4);
  background: var(--bg-raised);
  overflow-y: auto;
  box-shadow: var(--shadow-lg, 0 8px 24px rgb(0 0 0 / 20%));
}
.mobile-menu__close {
  background: none;
  border: 0;
  font-size: 1.25rem;
  color: var(--text-muted);
  cursor: pointer;
  padding: var(--space-1) var(--space-2);
  margin-bottom: var(--space-3);
}
</style>
