<script setup lang="ts">
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import AppLayout from "@/components/layout/AppLayout.vue";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";
import { ApiError } from "@/api/errors";
import { useAuthStore } from "@/stores/auth";

// Token-entry sign-in — the OAuth/authorization-code flow that
// replaces this page lands in epic 25.
const { t } = useI18n();
const route = useRoute();
const router = useRouter();
const auth = useAuthStore();

const token = ref("");
const pending = ref(false);
const error = ref("");

async function submit() {
  error.value = "";
  if (!token.value.trim()) {
    error.value = t("auth.tokenRequired");
    return;
  }
  pending.value = true;
  try {
    await auth.signInWithToken(token.value.trim());
    // Only in-app paths — anything else could bounce off-origin.
    const r = route.query.redirect;
    const redirect =
      typeof r === "string" && r.startsWith("/") && !r.startsWith("//")
        ? r
        : "/";
    await router.push(redirect);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : t("auth.signInFailed");
  } finally {
    pending.value = false;
  }
}
</script>

<template>
  <AppLayout>
    <Card class="signin">
      <template #header>{{ t("auth.signInTitle") }}</template>
      <form class="signin__form" @submit.prevent="submit">
        <p class="signin__hint">{{ t("auth.tokenHint") }}</p>
        <Input
          v-model="token"
          :label="t('auth.tokenLabel')"
          :error="error || undefined"
          :disabled="pending"
          type="password"
        />
        <Button type="submit" :loading="pending" :disabled="pending">
          {{ t("auth.signIn") }}
        </Button>
      </form>
    </Card>
  </AppLayout>
</template>

<style scoped>
.signin {
  max-width: 28rem;
  margin: 0 auto;
}
.signin__form {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.signin__hint {
  color: var(--text-muted);
  margin: 0;
}
</style>
