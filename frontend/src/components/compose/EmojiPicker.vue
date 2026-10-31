<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { customEmojis } from "@/api/emojis";
import { UNICODE_EMOJIS, type EmojiSelection } from "@/utils/unicodeEmoji";
import type { CustomEmoji } from "@/api/entities";

const emit = defineEmits<{ select: [emoji: EmojiSelection] }>();
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

/** Static `t()` calls so the i18n usage check sees every key. */
function catLabel(id: string) {
  switch (id) {
    case "smileys":
      return t("compose.emojiCatSmileys");
    case "people":
      return t("compose.emojiCatPeople");
    case "animals":
      return t("compose.emojiCatAnimals");
    case "food":
      return t("compose.emojiCatFood");
    case "travel":
      return t("compose.emojiCatTravel");
    case "activities":
      return t("compose.emojiCatActivities");
    case "objects":
      return t("compose.emojiCatObjects");
    case "symbols":
      return t("compose.emojiCatSymbols");
    case "flags":
      return t("compose.emojiCatFlags");
    default:
      return id;
  }
}

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
    <section v-for="[cat, list] in grouped" :key="`custom-${cat}`">
      <p v-if="cat" class="emoji-picker__cat">{{ cat }}</p>
      <button
        v-for="e in list"
        :key="e.shortcode"
        type="button"
        class="emoji-picker__item"
        :title="`:${e.shortcode}:`"
        :aria-label="`:${e.shortcode}:`"
        @click="emit('select', { kind: 'custom', value: e.shortcode })"
      >
        <img :src="e.url" :alt="e.shortcode" class="emoji-picker__img" />
      </button>
    </section>
    <section v-for="cat in UNICODE_EMOJIS" :key="cat.id">
      <p class="emoji-picker__cat">{{ catLabel(cat.id) }}</p>
      <button
        v-for="e in cat.emojis"
        :key="e"
        type="button"
        class="emoji-picker__item"
        :title="e"
        :aria-label="e"
        @click="emit('select', { kind: 'unicode', value: e })"
      >
        <span class="emoji-picker__glyph">{{ e }}</span>
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
  width: 19rem;
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
/* Fixed square tiles: a non-square uploaded emoji fits inside its
 * box (object-fit: contain) instead of being stretched to fill it. */
.emoji-picker__item {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 2rem;
  height: 2rem;
  border: 0;
  background: none;
  padding: 0;
  cursor: pointer;
  border-radius: var(--radius-sm);
  font-size: 1.3rem;
  line-height: 1;
}
.emoji-picker__item:hover {
  background: var(--bg-sunken);
}
.emoji-picker__img {
  width: 1.4rem;
  height: 1.4rem;
  object-fit: contain;
}
.emoji-picker__glyph {
  line-height: 1;
}
</style>
