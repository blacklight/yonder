<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

const props = defineProps<{ title: string }>();
const emit = defineEmits<{ close: [] }>();
const { t } = useI18n();

const panel = ref<HTMLElement>();
let restoreFocus: HTMLElement | null = null;

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function onKeydown(event: KeyboardEvent) {
  if (event.key === "Escape") {
    emit("close");
    return;
  }
  if (event.key !== "Tab" || !panel.value) return;
  // Focus trap: wrap first/last focusable inside the panel.
  const items = Array.from(
    panel.value.querySelectorAll<HTMLElement>(FOCUSABLE),
  );
  if (items.length === 0) return;
  const first = items[0];
  const last = items[items.length - 1];
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}

onMounted(() => {
  restoreFocus = document.activeElement as HTMLElement | null;
  const target =
    panel.value?.querySelector<HTMLElement>(FOCUSABLE) ?? panel.value;
  target?.focus();
  document.addEventListener("keydown", onKeydown);
});
onBeforeUnmount(() => {
  document.removeEventListener("keydown", onKeydown);
  restoreFocus?.focus();
});
</script>

<template>
  <Teleport to="body">
    <div class="modal__backdrop" @click.self="emit('close')">
      <div
        ref="panel"
        class="modal__panel"
        role="dialog"
        aria-modal="true"
        :aria-label="props.title"
        tabindex="-1"
      >
        <header class="modal__header">
          <h2 class="modal__title">{{ title }}</h2>
          <button
            class="modal__close"
            :aria-label="t('ui.close')"
            @click="emit('close')"
          >
            ×
          </button>
        </header>
        <div class="modal__body"><slot /></div>
        <footer v-if="$slots.footer" class="modal__footer">
          <slot name="footer" />
        </footer>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.modal__backdrop {
  position: fixed;
  inset: 0;
  display: grid;
  place-items: center;
  background: rgba(0, 0, 0, 0.45);
  z-index: 100;
}
.modal__panel {
  background: var(--bg-raised);
  border-radius: var(--radius-lg);
  padding: var(--space-4);
  width: min(90vw, 32rem);
  max-height: 85vh;
  overflow: auto;
}
.modal__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.modal__title {
  margin: 0;
  font-size: var(--text-lg);
}
.modal__close {
  border: 0;
  background: none;
  font-size: var(--text-xl);
  cursor: pointer;
  color: var(--text-muted);
}
.modal__body {
  margin-top: var(--space-3);
}
.modal__footer {
  margin-top: var(--space-4);
  display: flex;
  justify-content: flex-end;
  gap: var(--space-2);
}
</style>
