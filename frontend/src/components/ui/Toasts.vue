<script setup lang="ts">
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import type { IconProp } from "@fortawesome/fontawesome-svg-core";
import { FontAwesomeIcon } from "@/icons";
import { useToastsStore, type Toast } from "@/stores/toasts";
import { accentForeground } from "@/stores/theme";

const { t } = useI18n();
const router = useRouter();
const store = useToastsStore();

const KIND_ICONS: Record<string, IconProp> = {
  success: "circle-check",
  error: "triangle-exclamation",
};

function iconFor(toast: Toast): IconProp | undefined {
  return toast.icon ?? KIND_ICONS[toast.kind ?? ""];
}

/** Custom backgrounds get a luminance-computed foreground. */
function styleFor(toast: Toast) {
  if (!toast.background) return undefined;
  return {
    background: toast.background,
    color: accentForeground(toast.background),
    borderColor: "transparent",
  };
}

function open(toast: Toast) {
  store.remove(toast.id);
  if (toast.to) void router.push(toast.to);
}
</script>

<template>
  <div class="toasts" aria-live="polite">
    <div
      v-for="toast in store.items"
      :key="toast.id"
      class="toast"
      :class="`toast--${toast.kind ?? 'info'}`"
      :style="styleFor(toast)"
      role="status"
    >
      <img v-if="toast.image" :src="toast.image" alt="" class="toast__img" />
      <FontAwesomeIcon
        v-else-if="iconFor(toast)"
        :icon="iconFor(toast)!"
        class="toast__icon"
        aria-hidden="true"
      />
      <button
        type="button"
        class="toast__body"
        :class="{ 'toast__body--link': toast.to }"
        @click="open(toast)"
      >
        <span class="toast__text">{{ toast.text }}</span>
        <span v-if="toast.body" class="toast__sub">{{ toast.body }}</span>
      </button>
      <button
        type="button"
        class="toast__close"
        :aria-label="t('ui.close')"
        @click="store.remove(toast.id)"
      >
        <FontAwesomeIcon icon="xmark" />
      </button>
    </div>
  </div>
</template>

<style scoped>
.toasts {
  position: fixed;
  bottom: var(--space-4);
  inset-inline-end: var(--space-4);
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  z-index: 90;
  max-width: min(24rem, calc(100vw - var(--space-8)));
}
.toast {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-3) var(--space-3) var(--space-4);
  background: var(--bg-raised);
  color: var(--text);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  box-shadow: 0 8px 24px rgb(0 0 0 / 25%);
  overflow: hidden;
  animation: toast-in var(--duration-normal) ease-out;
}
.toast--success {
  background: var(--success);
  color: #fff;
  border-color: transparent;
}
.toast--error {
  background: var(--danger);
  color: #fff;
  border-color: transparent;
}
.toast__img {
  flex: none;
  width: 2.25rem;
  height: 2.25rem;
  border-radius: var(--radius-full);
  object-fit: cover;
}
.toast__icon {
  flex: none;
  font-size: 1.1rem;
  color: var(--accent-link);
}
/* Colored variants keep the icon on the card's foreground. */
.toast--success .toast__icon,
.toast--error .toast__icon {
  color: currentColor;
}
.toast__body {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 1px;
  padding: var(--space-1) 0;
  background: none;
  border: 0;
  font: inherit;
  color: inherit;
  text-align: start;
  cursor: pointer;
}
.toast__text {
  font-weight: 600;
  overflow-wrap: break-word;
}
.toast__body--link:hover .toast__text {
  text-decoration: underline;
}
.toast__sub {
  font-size: var(--text-sm);
  opacity: 0.8;
  overflow: hidden;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
}
.toast__close {
  flex: none;
  align-self: flex-start;
  padding: var(--space-1);
  background: none;
  border: 0;
  border-radius: var(--radius-full);
  color: inherit;
  opacity: 0.7;
  cursor: pointer;
}
.toast__close:hover {
  opacity: 1;
  background: color-mix(in srgb, currentColor 15%, transparent);
}
@keyframes toast-in {
  from {
    opacity: 0;
    transform: translateY(var(--space-2));
  }
  to {
    opacity: 1;
    transform: none;
  }
}
</style>
