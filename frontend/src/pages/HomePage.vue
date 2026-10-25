<script setup lang="ts">
import { useI18n } from "vue-i18n";
import AppLayout from "@/components/layout/AppLayout.vue";
import TimelinePage from "./TimelinePage.vue";
import { useAuthStore } from "@/stores/auth";

const { t } = useI18n();
const auth = useAuthStore();
</script>

<template>
  <AppLayout>
    <p v-if="!auth.isLoggedIn" class="home__intro">
      {{ t("timeline.homeIntro") }}
    </p>
    <TimelinePage
      :timeline="auth.isLoggedIn ? 'home' : 'public'"
      :stream="auth.isLoggedIn ? 'user' : 'public'"
      :empty-key="auth.isLoggedIn ? 'home' : 'explore'"
    />
  </AppLayout>
</template>

<style scoped>
.home__intro {
  color: var(--text-muted);
  margin: 0 0 var(--space-4);
}
</style>
