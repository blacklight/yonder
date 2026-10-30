<script setup lang="ts">
// Skeleton for a `StatusCard` — same raised-card chrome, header row
// and footer so a loading timeline keeps its shape while posts
// arrive.
import Skeleton from "@/components/ui/Skeleton.vue";
</script>

<template>
  <div class="status-skel" aria-hidden="true">
    <header class="status-skel__header">
      <Skeleton circle width="48px" />
      <div class="status-skel__names">
        <Skeleton width="34%" />
        <Skeleton width="48%" height="0.75em" />
      </div>
    </header>
    <div class="status-skel__body">
      <Skeleton />
      <Skeleton width="88%" />
      <Skeleton width="64%" />
    </div>
    <div class="status-skel__footer">
      <Skeleton v-for="n in 4" :key="n" width="1.1rem" height="1.1rem" />
    </div>
  </div>
</template>

<style scoped>
.status-skel {
  background: var(--bg-raised);
  padding: var(--space-4);
  border-bottom: 1px solid var(--border-dim);
}
.status-skel__header {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin-bottom: var(--space-3);
}
.status-skel__names {
  display: flex;
  flex: 1;
  min-width: 0;
  flex-direction: column;
  gap: var(--space-2);
}
.status-skel__body {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.status-skel__footer {
  display: flex;
  gap: var(--space-5);
  margin-top: var(--space-4);
}
</style>
