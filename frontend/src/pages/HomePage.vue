<script setup lang="ts">
import { useI18n } from "vue-i18n";
import TimelinePage from "./TimelinePage.vue";
import { useAuthStore } from "@/stores/auth";

const { t } = useI18n();
const auth = useAuthStore();
</script>

<template>
  <p v-if="!auth.isLoggedIn" class="home__intro">
    {{ t("timeline.homeIntro") }}
  </p>
  <TimelinePage
    :timeline="auth.isLoggedIn ? 'home' : 'public'"
    :stream="auth.isLoggedIn ? 'user' : 'public'"
    :empty-key="auth.isLoggedIn ? 'home' : 'explore'"
  />
</template>

<style scoped>
.home__intro {
  background: var(--bg-raised);
  color: var(--text-muted);
  font-size: 0.8em;
  text-align: center;
  margin: 0 0 var(--space-4);
  padding: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-full);
  opacity: 0.75;
}
</style>
