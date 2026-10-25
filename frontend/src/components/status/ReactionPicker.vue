<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { customEmojis } from "@/api/emojis";
import type { CustomEmoji } from "@/api/entities";

const emit = defineEmits<{ select: [emoji: string] }>();
const { t } = useI18n();

const UNICODE_DEFAULTS = ["👍", "❤️", "😂", "🎉", "😮", "😢"];
const custom = ref<CustomEmoji[]>([]);

onMounted(async () => {
  try {
    custom.value = (await customEmojis()).filter((e) => e.visible_in_picker);
  } catch {
    custom.value = [];
  }
});
</script>

<template>
  <div
    class="reaction-picker"
    role="listbox"
    :aria-label="t('status.react')"
    data-testid="reaction-picker"
  >
    <button
      v-for="e in UNICODE_DEFAULTS"
      :key="e"
      type="button"
      class="reaction-picker__item"
      @click="emit('select', e)"
    >
      {{ e }}
    </button>
    <button
      v-for="e in custom"
      :key="e.shortcode"
      type="button"
      class="reaction-picker__item"
      :title="`:${e.shortcode}:`"
      @click="emit('select', e.shortcode)"
    >
      <img :src="e.url" :alt="e.shortcode" class="reaction-picker__img" />
    </button>
  </div>
</template>

<style scoped>
.reaction-picker {
  position: absolute;
  bottom: 100%;
  left: 0;
  z-index: 20;
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-1);
  max-width: 16rem;
  padding: var(--space-2);
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  box-shadow: 0 4px 16px rgb(0 0 0 / 15%);
}
.reaction-picker__item {
  border: 0;
  background: none;
  font-size: 1.25rem;
  cursor: pointer;
  padding: var(--space-1);
  border-radius: var(--radius-sm);
}
.reaction-picker__item:hover {
  background: var(--bg-sunken);
}
.reaction-picker__img {
  width: 1.25rem;
  height: 1.25rem;
  vertical-align: middle;
}
</style>
