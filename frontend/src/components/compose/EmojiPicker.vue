<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { customEmojis } from "@/api/emojis";
import type { CustomEmoji } from "@/api/entities";

const emit = defineEmits<{ select: [shortcode: string] }>();
const { t } = useI18n();
const emojis = ref<CustomEmoji[]>([]);

const grouped = computed(() => {
  const groups = new Map<string, CustomEmoji[]>();
  for (const e of emojis.value) {
    const cat = e.category ?? "";
    if (!groups.has(cat)) groups.set(cat, []);
    groups.get(cat)!.push(e);
  }
  return groups;
});

onMounted(async () => {
  try {
    emojis.value = (await customEmojis()).filter((e) => e.visible_in_picker);
  } catch {
    emojis.value = [];
  }
});
</script>

<template>
  <div
    class="emoji-picker"
    data-testid="emoji-picker"
    role="dialog"
    :aria-label="t('compose.emoji')"
  >
    <section v-for="[cat, list] in grouped" :key="cat">
      <p v-if="cat" class="emoji-picker__cat">{{ cat }}</p>
      <button
        v-for="e in list"
        :key="e.shortcode"
        type="button"
        class="emoji-picker__item"
        :title="`:${e.shortcode}:`"
        @click="emit('select', e.shortcode)"
      >
        <img :src="e.url" :alt="e.shortcode" class="emoji-picker__img" />
      </button>
    </section>
  </div>
</template>

<style scoped>
.emoji-picker {
  position: absolute;
  bottom: 100%;
  right: 0;
  z-index: 20;
  max-height: 16rem;
  overflow-y: auto;
  padding: var(--space-2);
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  box-shadow: 0 4px 16px rgb(0 0 0 / 15%);
}
.emoji-picker__cat {
  font-size: var(--text-xs);
  color: var(--text-muted);
  margin: var(--space-1) 0;
}
.emoji-picker__item {
  border: 0;
  background: none;
  padding: 2px;
  cursor: pointer;
  border-radius: var(--radius-sm);
}
.emoji-picker__item:hover {
  background: var(--bg-sunken);
}
.emoji-picker__img {
  width: 1.4rem;
  height: 1.4rem;
}
</style>
