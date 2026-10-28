<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

// Bodies taller than this collapse behind the fade — ~16 text lines,
// generous enough that ordinary posts never trip it. Only the status
// text is ever wrapped (StatusCard keeps media/quotes/polls outside),
// so attachments are never sliced mid-frame. Keep in sync with
// `max-height` in the styles below.
const MAX_HEIGHT = 400;

const props = defineProps<{
  /** CW'd bodies are already hidden behind CwBlock, and the focal
   * status on its own page opens expanded — both opt out here. */
  disabled?: boolean;
}>();
const { t } = useI18n();

const body = ref<HTMLElement | null>(null);
const overflows = ref(false);
const expanded = ref(false);
const collapsed = computed(() => overflows.value && !expanded.value);

// `scrollHeight` reports the full content height even while
// `max-height` clips the element, so one measure covers both states.
// Re-measure on resize: viewport rotation reflows text, async media
// grows the body (an image landing after mount can trip the clamp —
// while collapsed the box stays fixed, which is fine, it can only
// keep overflowing). jsdom has no ResizeObserver — guard it.
let observer: ResizeObserver | null = null;
function measure() {
  if (props.disabled) return;
  overflows.value = (body.value?.scrollHeight ?? 0) > MAX_HEIGHT;
}
onMounted(() => {
  measure();
  if (typeof ResizeObserver !== "undefined" && body.value) {
    observer = new ResizeObserver(measure);
    observer.observe(body.value);
  }
});
onBeforeUnmount(() => observer?.disconnect());
</script>

<template>
  <div class="show-more" :class="{ 'show-more--collapsed': collapsed }">
    <div ref="body" class="show-more__body">
      <slot />
    </div>
    <div
      v-if="overflows"
      class="show-more__tail"
      :class="{ 'show-more__tail--fade': collapsed }"
    >
      <button
        type="button"
        class="show-more__toggle"
        :aria-expanded="expanded"
        @click="expanded = !expanded"
      >
        {{ collapsed ? t("ui.showMore") : t("ui.showLess") }}
      </button>
    </div>
  </div>
</template>

<style scoped>
.show-more {
  position: relative;
}
.show-more--collapsed .show-more__body {
  max-height: 25rem; /* MAX_HEIGHT */
  overflow: hidden;
}
/* The fade overlays the clamped bottom edge — it must not swallow
   clicks destined for the card beneath, only the button is live. */
.show-more__tail {
  display: flex;
  justify-content: center;
}
.show-more__tail--fade {
  position: absolute;
  left: 0;
  right: 0;
  bottom: 0;
  padding-top: var(--space-7);
  padding-bottom: var(--space-2);
  background: linear-gradient(to bottom, transparent, var(--bg-raised) 70%);
  pointer-events: none;
}
.show-more__tail:not(.show-more__tail--fade) {
  margin-top: var(--space-2);
}
.show-more__toggle {
  border: none;
  background: var(--bg-raised);
  padding: var(--space-1) var(--space-3);
  color: var(--accent-link);
  cursor: pointer;
  font: inherit;
  font-size: var(--text-sm);
  pointer-events: auto;
}
</style>
