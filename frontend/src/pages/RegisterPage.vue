<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import AppLayout from "@/components/layout/AppLayout.vue";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";
import { ApiError } from "@/api/errors";
import { register } from "@/api/accounts";
import { getInstance } from "@/api/instance";
import type { InstanceEntity } from "@/api/entities";
import { useAuthStore } from "@/stores/auth";

// `POST /api/v1/accounts` — the shape follows instance policy read
// from `/api/v1/instance` `registrations`: `enabled` gates the form
// (invite codes still work when closed), `approval_required` asks for
// a reason, and `message` renders the admin's custom notice.
const { t, locale } = useI18n();
const route = useRoute();
const router = useRouter();
const auth = useAuthStore();

const instance = ref<InstanceEntity | null>(null);
const username = ref("");
const email = ref("");
const password = ref("");
const confirm = ref("");
const agreement = ref(false);
const reason = ref("");
const invite = ref("");
// Prefill from `/register?invite=` (or the `/invite/:code` redirect) —
// once only, so later query changes don't clobber typed input.
watch(
  () => route.query.invite,
  (q) => {
    if (typeof q === "string" && q && !invite.value) invite.value = q;
  },
  { immediate: true },
);
const pending = ref(false);
const error = ref("");
// Set when registration succeeded but the account needs something
// before it is fully usable — the user signs in explicitly via the
// Continue button instead of being dropped on a limited timeline.
const registered = ref<{
  pendingApproval: boolean;
  verificationPending: boolean;
} | null>(null);
let pendingToken = "";

const registrations = computed(() => instance.value?.registrations);
const closed = computed(() => registrations.value?.enabled === false);
const approvalRequired = computed(
  () => registrations.value?.approval_required === true,
);

onMounted(async () => {
  try {
    instance.value = await getInstance();
  } catch {
    /* closed/approval hints degrade gracefully — the form still works */
  }
});

/** Sign in with the registration token and land on the timeline. */
async function finish(accessToken: string) {
  await auth.signInWithToken(accessToken);
  await router.push("/");
}

async function submit() {
  error.value = "";
  if (!username.value.trim() || !email.value.trim() || !password.value) {
    error.value = t("register.required");
    return;
  }
  if (password.value !== confirm.value) {
    error.value = t("register.passwordMismatch");
    return;
  }
  if (!agreement.value) {
    error.value = t("register.agreementRequired");
    return;
  }
  pending.value = true;
  try {
    const res = await register({
      username: username.value.trim(),
      email: email.value.trim(),
      password: password.value,
      agreement: true,
      locale: locale.value,
      reason: approvalRequired.value && reason.value.trim() ? reason.value.trim() : undefined,
      invite_code: invite.value.trim() || undefined,
    });
    const pendingApproval = approvalRequired.value && !invite.value.trim();
    const verificationPending = res.verification_pending === true;
    if (pendingApproval || verificationPending) {
      pendingToken = res.access_token;
      registered.value = { pendingApproval, verificationPending };
    } else {
      // Registration returns the first token — the same entity as the
      // password grant — so reuse the normal sign-in path (verify +
      // settings sync) before landing on the timeline.
      await finish(res.access_token);
    }
  } catch (e) {
    error.value =
      e instanceof ApiError
        ? e.status === 403
          ? t("register.closed")
          : e.message
        : t("register.failed");
  } finally {
    pending.value = false;
  }
}
</script>

<template>
  <AppLayout>
    <Card class="register">
      <template #header>{{ t("register.title") }}</template>
      <div v-if="registered" class="register__form">
        <p role="status">{{ t("register.registered") }}</p>
        <p v-if="registered.pendingApproval" role="note">
          {{ t("register.pendingApproval") }}
        </p>
        <p v-if="registered.verificationPending" role="note">
          {{ t("register.verifyEmail") }}
        </p>
        <Button :disabled="!pendingToken" @click="finish(pendingToken)">
          {{ t("register.continue") }}
        </Button>
      </div>
      <template v-else>
        <p v-if="registrations?.message" class="register__message">
          {{ registrations.message }}
        </p>
        <form class="register__form" @submit.prevent="submit">
        <p v-if="closed" class="register__closed" role="note">
          {{ t("register.closedInvite") }}
        </p>
        <Input
          v-model="invite"
          :label="t('register.inviteCode')"
          :disabled="pending"
        />
        <template v-if="!closed || invite.trim()">
          <Input
            v-model="username"
            :label="t('register.username')"
            :disabled="pending"
            autocomplete="username"
            required
          />
          <Input
            v-model="email"
            :label="t('register.email')"
            :disabled="pending"
            type="email"
            autocomplete="email"
            required
          />
          <Input
            v-model="password"
            :label="t('register.password')"
            :disabled="pending"
            type="password"
            autocomplete="new-password"
            required
          />
          <Input
            v-model="confirm"
            :label="t('register.confirmPassword')"
            :disabled="pending"
            type="password"
            autocomplete="new-password"
            required
          />
          <Input
            v-if="approvalRequired"
            v-model="reason"
            :label="t('register.reason')"
            :disabled="pending"
            required
          />
          <label class="register__agreement">
            <input v-model="agreement" type="checkbox" :disabled="pending" />
            {{ t("register.agreement") }}
          </label>
          <p v-if="error" class="register__error" role="alert">{{ error }}</p>
          <Button type="submit" :loading="pending" :disabled="pending">
            {{ t("register.submit") }}
          </Button>
        </template>
        <p v-else-if="error" class="register__error" role="alert">{{ error }}</p>
        <p class="register__alt">
          {{ t("register.haveAccount") }}
          <router-link to="/sign-in">{{ t("nav.signIn") }}</router-link>
        </p>
      </form>
      </template>
    </Card>
  </AppLayout>
</template>

<style scoped>
.register {
  max-width: 28rem;
  margin: 0 auto;
}
.register__form {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.register__message,
.register__alt {
  color: var(--text-muted);
  margin: 0;
}
.register__closed {
  color: var(--text-muted);
  margin: 0;
}
.register__agreement {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.register__error {
  color: var(--danger);
  margin: 0;
}
</style>
