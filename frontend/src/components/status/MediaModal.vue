<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { FontAwesomeIcon } from "@/icons";
import type { MediaAttachment } from "@/api/entities";

const props = defineProps<{
  media: MediaAttachment[];
  initialIndex?: number;
}>();
const emit = defineEmits<{ close: [] }>();
const { t } = useI18n();

const index = ref(props.initialIndex ?? 0);
const current = computed(() => props.media[index.value]);

function step(delta: number) {
  const n = props.media.length;
  if (n < 2) return;
  index.value = (index.value + delta + n) % n;
}

const panel = ref<HTMLElement>();
let restoreFocus: HTMLElement | null = null;
let prevBodyOverflow = "";

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function onKeydown(event: KeyboardEvent) {
  if (event.key === "Escape") {
    emit("close");
    return;
  }
  if (event.key === "ArrowLeft") {
    step(-1);
    return;
  }
  if (event.key === "ArrowRight") {
    step(1);
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
  prevBodyOverflow = document.body.style.overflow;
  document.body.style.overflow = "hidden";
  document.addEventListener("keydown", onKeydown);
});
onBeforeUnmount(() => {
  document.removeEventListener("keydown", onKeydown);
  document.body.style.overflow = prevBodyOverflow;
  restoreFocus?.focus();
});
</script>

<template>
  <Teleport to="body">
    <div class="media-modal__backdrop" @click.self="emit('close')">
      <div
        ref="panel"
        class="media-modal"
        role="dialog"
        aria-modal="true"
        :aria-label="t('status.media.view')"
        tabindex="-1"
        data-testid="media-modal"
      >
        <button
          type="button"
          class="media-modal__close"
          :aria-label="t('ui.close')"
          @click="emit('close')"
        >
          <FontAwesomeIcon icon="xmark" />
        </button>
        <button
          v-if="media.length > 1"
          type="button"
          class="media-modal__nav media-modal__nav--prev"
          :aria-label="t('status.media.previous')"
          data-testid="media-prev"
          @click="step(-1)"
        >
          <FontAwesomeIcon icon="chevron-left" />
        </button>
        <figure class="media-modal__stage" @click.self="emit('close')">
          <img
            v-if="current?.type === 'image'"
            :src="current.url"
            :alt="current.description ?? ''"
            class="media-modal__img"
          />
          <video
            v-else-if="current?.type === 'gifv'"
            :src="current.url"
            class="media-modal__img"
            autoplay
            loop
            muted
            playsinline
          />
          <video
            v-else-if="current?.type === 'video'"
            :src="current.url"
            class="media-modal__img"
            controls
            autoplay
          />
          <audio
            v-else-if="current?.type === 'audio'"
            :src="current.url"
            controls
          />
          <img
            v-else-if="current"
            :src="current.preview_url ?? current.url"
            :alt="current.description ?? ''"
            class="media-modal__img"
          />
          <figcaption v-if="current?.description" class="media-modal__caption">
            {{ current.description }}
          </figcaption>
        </figure>
        <button
          v-if="media.length > 1"
          type="button"
          class="media-modal__nav media-modal__nav--next"
          :aria-label="t('status.media.next')"
          data-testid="media-next"
          @click="step(1)"
        >
          <FontAwesomeIcon icon="chevron-right" />
        </button>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.media-modal__backdrop {
  position: fixed;
  inset: 0;
  background: rgb(0 0 0 / 90%);
  z-index: 100;
}
.media-modal {
  position: relative;
  width: 100%;
  height: 100%;
  display: flex;
  align-items: center;
  justify-content: center;
}
.media-modal__stage {
  margin: 0;
  width: 100%;
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  min-width: 0;
}
.media-modal__img {
  max-width: 90vw;
  max-height: 85vh;
  object-fit: contain;
}
.media-modal__caption {
  color: #fff;
  font-size: var(--text-sm);
  padding: var(--space-3) var(--space-4);
  max-width: 90vw;
  text-align: center;
}
.media-modal__close {
  position: absolute;
  top: var(--space-4);
  right: var(--space-4);
  z-index: 1;
  border: 0;
  border-radius: 50%;
  width: 2.5rem;
  height: 2.5rem;
  background: rgb(0 0 0 / 60%);
  color: #fff;
  font-size: var(--text-xl);
  cursor: pointer;
}
.media-modal__nav {
  position: absolute;
  top: 50%;
  transform: translateY(-50%);
  z-index: 1;
  border: 0;
  border-radius: 50%;
  width: 2.75rem;
  height: 2.75rem;
  background: rgb(0 0 0 / 60%);
  color: #fff;
  font-size: var(--text-xl);
  cursor: pointer;
}
.media-modal__nav--prev {
  left: var(--space-4);
}
.media-modal__nav--next {
  right: var(--space-4);
}
</style>
