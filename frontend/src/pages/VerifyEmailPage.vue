<script setup lang="ts">
import { ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import { ApiError } from "@/api/errors";
import { verifyEmail } from "@/api/accounts";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";

// Landing page for the `/verify-email/:token` link in the
// verification mail — the token is consumed by POSTing it to
// `/api/v1/accounts/verify_email` from here, so mail scanners that
// prefetch the link can't burn the single-use token.
const { t } = useI18n();
const route = useRoute();
const router = useRouter();

const state = ref<"pending" | "verified" | "failed">("pending");
const error = ref("");

// React to the route param rather than reading it once on mount —
// the token may arrive with a later navigation.
watch(
  () => route.params.token,
  async (raw) => {
    const token = typeof raw === "string" ? raw : "";
    if (!token || state.value !== "pending") return;
    try {
      await verifyEmail(token);
      state.value = "verified";
    } catch (e) {
      error.value = e instanceof ApiError ? e.message : "";
      state.value = "failed";
    }
  },
  { immediate: true },
);
</script>

<template>
  <Card class="verify-email">
    <template #header>{{ t("verifyEmail.title") }}</template>
    <div class="verify-email__body">
      <p v-if="state === 'pending'" role="status">
        {{ t("verifyEmail.verifying") }}
      </p>
      <template v-else-if="state === 'verified'">
        <p role="status">{{ t("verifyEmail.verified") }}</p>
        <div>
          <Button @click="router.push({ name: 'sign-in' })">
            {{ t("verifyEmail.signIn") }}
          </Button>
        </div>
      </template>
      <template v-else>
        <p class="verify-email__error" role="alert">
          {{ error || t("verifyEmail.failed") }}
        </p>
        <p class="verify-email__hint">{{ t("verifyEmail.failedHint") }}</p>
      </template>
    </div>
  </Card>
</template>

<style scoped>
.verify-email {
  max-width: 28rem;
  margin: 0 auto;
}
.verify-email__body {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.verify-email__body p {
  margin: 0;
}
.verify-email__error {
  color: var(--danger);
}
.verify-email__hint {
  color: var(--text-muted);
  font-size: var(--text-sm);
}
</style>
