<script setup lang="ts">
// Skeleton for an account-list row — avatar circle plus the
// display-name/acct lines. `avatar: false` suits text-only rows
// (admin tables) that have no leading image.
import Skeleton from "./Skeleton.vue";

withDefaults(
  defineProps<{
    /** Match the `Avatar` size the real rows use. */
    size?: "sm" | "md";
    avatar?: boolean;
  }>(),
  { size: "sm", avatar: true },
);
</script>

<template>
  <div class="acct-skel" aria-hidden="true">
    <Skeleton v-if="avatar" circle :width="size === 'md' ? '48px' : '32px'" />
    <div class="acct-skel__lines">
      <Skeleton width="38%" />
      <Skeleton width="62%" height="0.75em" />
    </div>
  </div>
</template>

<style scoped>
.acct-skel {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  /* Fill flex parents (admin rows) as well as block ones. */
  width: 100%;
  padding: var(--space-3);
}
.acct-skel__lines {
  display: flex;
  flex: 1;
  min-width: 0;
  flex-direction: column;
  gap: var(--space-2);
}
</style>
