<script setup lang="ts">
import { onMounted, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { useTheme } from "@/composables/useTheme";
import { useAuthStore } from "@/stores/auth";

const { apply } = useTheme();
const auth = useAuthStore();
const route = useRoute();
const router = useRouter();
onMounted(() => {
  apply();
  // Reloads keep the token (localStorage) but lose the in-memory
  // account/role — re-fetch so staff UI and the user block reappear.
  void auth.restore();
});
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
  <router-view />
</template>
