<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import Button from "@/components/ui/Button.vue";
import { useAuthStore } from "@/stores/auth";
import { followTag, unfollowTag } from "@/api/tags";
import type { Tag } from "@/api/entities";

/**
 * One hashtag row — name links to the tag timeline, the counter
 * shows the window totals (`uses`/`accounts`) when the response
 * carries them and otherwise sums the trailing-7-day `history`.
 */
const props = defineProps<{ tag: Tag }>();
const emit = defineEmits<{ change: [tag: Tag] }>();
const { t } = useI18n();
const auth = useAuthStore();

const following = ref(props.tag.following);
// A refetched list replaces the prop — drop the stale local copy.
watch(
  () => props.tag.following,
  (v) => {
    following.value = v;
  },
);
const busy = ref(false);

const uses = computed(() =>
  props.tag.uses != null
    ? Number(props.tag.uses)
    : (props.tag.history ?? []).reduce((n, h) => n + Number(h.uses), 0),
);
const accounts = computed(() =>
  props.tag.accounts != null
    ? Number(props.tag.accounts)
    : (props.tag.history ?? []).reduce((n, h) => n + Number(h.accounts), 0),
);

async function toggle() {
  if (busy.value) return;
  busy.value = true;
  try {
    const updated = following.value
      ? await unfollowTag(props.tag.name)
      : await followTag(props.tag.name);
    following.value = updated.following;
    emit("change", { ...props.tag, following: updated.following });
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <div class="tag-card" data-testid="tag-card">
    <div class="tag-card__main">
      <router-link
        :to="`/tags/${encodeURIComponent(tag.name)}`"
        class="tag-card__name"
        >#{{ tag.name }}</router-link
      >
      <span class="tag-card__usage">{{
        t("tags.usage", { uses, accounts })
      }}</span>
    </div>
    <Button
      v-if="auth.isLoggedIn"
      size="sm"
      :variant="following ? 'secondary' : 'primary'"
      :disabled="busy"
      class="tag-card__follow"
      data-testid="tag-follow"
      @click="toggle"
    >
      {{ following ? t("profile.unfollow") : t("profile.follow") }}
    </Button>
  </div>
</template>

<style scoped>
.tag-card {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-raised);
}
.tag-card__main {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  min-width: 0;
}
.tag-card__name {
  font-weight: 600;
  color: var(--accent-link);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.tag-card__usage {
  font-size: var(--text-sm);
  color: var(--text-muted);
}
.tag-card__follow {
  flex-shrink: 0;
}
</style>
