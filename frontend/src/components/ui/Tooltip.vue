<script setup lang="ts">
import { computed, ref, useId } from "vue";

const props = withDefaults(defineProps<{ text: string; delay?: number }>(), {
  delay: 300,
});
const id = useId();
const shown = ref(false);
let timer: ReturnType<typeof setTimeout> | undefined;

const describedBy = computed(() => (shown.value ? id : undefined));

function show() {
  timer = setTimeout(() => (shown.value = true), props.delay);
}
function hide() {
  clearTimeout(timer);
  shown.value = false;
}
</script>

<template>
  <span
    class="tooltip"
    :aria-describedby="describedBy"
    @mouseenter="show"
    @mouseleave="hide"
    @focusin="show"
    @focusout="hide"
  >
    <slot />
    <span v-if="shown" :id="id" role="tooltip" class="tooltip__bubble">{{
      text
    }}</span>
  </span>
</template>

<style scoped>
.tooltip {
  position: relative;
  display: inline-flex;
}
.tooltip__bubble {
  position: absolute;
  bottom: calc(100% + var(--space-1));
  left: 50%;
  transform: translateX(-50%);
  /* pre-line: explicit \n breaks (admin setting help) without
     wrapping otherwise. */
  white-space: pre-line;
  background: var(--text);
  color: var(--bg);
  font-size: var(--text-xs);
  padding: var(--space-1) var(--space-2);
  border-radius: var(--radius-sm);
  z-index: 60;
}
</style>
