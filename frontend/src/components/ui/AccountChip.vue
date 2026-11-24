<script setup lang="ts">
import { computed } from "vue";
import { RouterLink } from "vue-router";
import Avatar from "./Avatar.vue";
import EmojiText from "./EmojiText.vue";
import type { Account } from "@/api/entities";

const props = defineProps<{
  /** Account entity to render; `null`/`undefined` falls back to
   *  `fallback` as plain text (e.g. a deleted account). */
  account?: Account | null;
  fallback?: string;
}>();

const name = computed(
  () => props.account?.display_name || props.account?.username || "?",
);
const to = computed(() => (props.account ? `/@${props.account.acct}` : null));
</script>

<template>
  <component
    :is="to ? RouterLink : 'span'"
    v-bind="to ? { to } : {}"
    class="account-chip"
  >
    <Avatar v-if="account" :src="account.avatar" :name="name" size="sm" />
    <span class="account-chip__names">
      <span class="account-chip__display">
        <EmojiText v-if="account" :text="name" :emojis="account.emojis" />
        <template v-else>{{ fallback }}</template>
      </span>
      <span v-if="account" class="account-chip__acct">@{{ account.acct }}</span>
    </span>
  </component>
</template>

<style scoped>
.account-chip {
  display: inline-flex;
  align-items: center;
  gap: 0.5rem;
  min-width: 0;
  color: inherit;
  text-decoration: none;
}
a.account-chip:hover .account-chip__display {
  text-decoration: underline;
}
.account-chip__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.account-chip__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.account-chip__acct {
  color: var(--text-muted, #666);
  font-size: 0.8rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
