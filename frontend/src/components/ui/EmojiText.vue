<script setup lang="ts">
import { computed } from "vue";
import { emojiParts, type EmojiRef } from "@/utils/emoji";

/** Plain text with `:shortcode:` expanded to inline emoji images —
 * for fields served raw by the API (`display_name`, `spoiler_text`),
 * which clients expand from the entity's `emojis` array. */
const props = defineProps<{
  text: string;
  emojis?: EmojiRef[];
}>();

const parts = computed(() => emojiParts(props.text, props.emojis));
</script>

<template>
  <template v-for="(part, i) in parts" :key="i"
    ><img
      v-if="part.emoji"
      class="emoji"
      :src="part.emoji.url"
      :alt="`:${part.emoji.shortcode}:`"
      :title="`:${part.emoji.shortcode}:`"
      draggable="false"
      loading="lazy"
    /><template v-else>{{ part.text }}</template></template
  >
</template>
