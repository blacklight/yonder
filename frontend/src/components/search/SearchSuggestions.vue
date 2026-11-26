<script setup lang="ts">
import { computed, useTemplateRef } from "vue";
import { useRouter } from "vue-router";
import Avatar from "@/components/ui/Avatar.vue";
import EmojiText from "@/components/ui/EmojiText.vue";
import { useSearchStore } from "@/stores/search";

const props = defineProps<{ owner: string }>();
const router = useRouter();
const store = useSearchStore();

const visible = computed(() => store.suggestionOwner === props.owner);
const suggestionAccounts = computed(() =>
  visible.value ? (store.suggestions?.accounts ?? []) : [],
);
const suggestionHashtags = computed(() =>
  visible.value ? (store.suggestions?.hashtags ?? []) : [],
);
const hasSuggestions = computed(
  () => suggestionAccounts.value.length + suggestionHashtags.value.length > 0,
);
const list = useTemplateRef<HTMLUListElement>("list");

function optionElements(): HTMLElement[] {
  return Array.from(
    list.value?.querySelectorAll<HTMLElement>(".search__suggestion") ?? [],
  );
}

function focusEdge(edge: "first" | "last"): boolean {
  const items = optionElements();
  const item = edge === "first" ? items[0] : items.at(-1);
  item?.focus();
  return !!item;
}

function onSuggestionKeydown(event: KeyboardEvent) {
  if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
  const items = optionElements();
  const current = event.currentTarget as HTMLElement;
  const index = items.indexOf(current);
  if (!items.length || index < 0) return;
  event.preventDefault();
  const delta = event.key === "ArrowDown" ? 1 : -1;
  items[(index + delta + items.length) % items.length]?.focus();
}

defineExpose({ focusEdge });

function goAccount(acct: string) {
  store.clearSuggestions();
  router.push(`/@${acct}`);
}
function goTag(name: string) {
  store.clearSuggestions();
  router.push(`/tags/${encodeURIComponent(name)}`);
}
</script>

<template>
  <ul
    v-if="hasSuggestions"
    ref="list"
    class="search__suggestions"
    role="listbox"
  >
    <li
      v-for="a in suggestionAccounts"
      :key="a.id"
      class="search__suggestion"
      role="option"
      aria-selected="false"
      tabindex="0"
      @click="goAccount(a.acct)"
      @keydown="onSuggestionKeydown"
      @keydown.enter="goAccount(a.acct)"
    >
      <Avatar :src="a.avatar" :name="a.display_name || a.acct" size="sm" />
      <span class="search__names">
        <span class="search__display"
          ><EmojiText :text="a.display_name || a.username" :emojis="a.emojis"
        /></span>
        <span class="search__acct">@{{ a.acct }}</span>
      </span>
    </li>
    <li
      v-for="h in suggestionHashtags"
      :key="h.name"
      class="search__suggestion"
      role="option"
      aria-selected="false"
      tabindex="0"
      @click="goTag(h.name)"
      @keydown="onSuggestionKeydown"
      @keydown.enter="goTag(h.name)"
    >
      <span class="search__tag">#{{ h.name }}</span>
    </li>
  </ul>
</template>

<style scoped>
.search__suggestions {
  position: absolute;
  top: 100%;
  left: 0;
  right: 0;
  z-index: 20;
  margin: var(--space-1) 0 0;
  padding: var(--space-2) 0;
  list-style: none;
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  box-shadow: 0 4px 12px rgb(0 0 0 / 0.12);
  max-height: 20rem;
  overflow-y: auto;
}
.search__suggestion {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  cursor: pointer;
}
.search__suggestion:hover {
  background: var(--bg-hover, var(--bg-sunken));
}
.search__suggestion:focus {
  background: var(--accent-soft);
}
.search__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.search__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.search__acct {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.search__tag {
  font-weight: 600;
}
</style>
