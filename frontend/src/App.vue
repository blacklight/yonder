<script setup lang="ts">
import { onMounted, onUnmounted, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import AppLayout from "@/components/layout/AppLayout.vue";
import Toasts from "@/components/ui/Toasts.vue";
import { useTheme } from "@/composables/useTheme";
import { useAuthStore } from "@/stores/auth";
import { localLinkNavHandler } from "@/utils/localLinks";

const { apply } = useTheme();
const auth = useAuthStore();
const route = useRoute();
const router = useRouter();
const onLocalLink = localLinkNavHandler(router);
onMounted(() => {
  apply();
  // Reloads keep the token (localStorage) but lose the in-memory
  // account/role — re-fetch so staff UI and the user block reappear.
  void auth.restore();
  // Content links rewritten to `/`-paths navigate via the router.
  document.addEventListener("click", onLocalLink);
});
onUnmounted(() => document.removeEventListener("click", onLocalLink));
// Route guards only run on navigation — a mid-session sign-out or a
// 401 token eviction must bounce protected pages to sign-in too.
watch(
  () => auth.isLoggedIn,
  (logged) => {
    if (!logged && route.meta.requiresAuth) {
      void router.push({
        name: "sign-in",
        query: { redirect: route.fullPath },
      });
    }
  },
);
</script>

<template>
  <AppLayout>
    <router-view />
  </AppLayout>
  <Toasts />
</template>
