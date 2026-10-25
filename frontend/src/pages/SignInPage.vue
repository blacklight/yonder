<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";
import { FontAwesomeIcon } from "@/icons";
import { ApiError } from "@/api/errors";
import { MfaRequired, passwordToken, twoFactorVerify } from "@/api/oauth";
import { useAuthStore } from "@/stores/auth";

// Password grant sign-in (`POST /oauth/token`). A 2FA-enrolled account
// answers `mfa_required` and completes the login on `/oauth/2fa/verify`
// — both return the same token entity.
const { t } = useI18n();
const route = useRoute();
const router = useRouter();
const auth = useAuthStore();

const username = ref("");
const password = ref("");
const code = ref("");
const mfaToken = ref<string | null>(null);
// The factors the pending login accepts — `mfa_required` carries them.
// A WebAuthn-only account cannot complete the code exchange at all,
// so the code form is hidden rather than presented as a dead end.
const mfaMethods = ref<string[]>([]);
const canVerifyCode = computed(
  () =>
    mfaMethods.value.length === 0 ||
    mfaMethods.value.includes("totp") ||
    mfaMethods.value.includes("backup_code"),
);
const mfaHint = computed(() =>
  canVerifyCode.value ? t("auth.mfaHintTotp") : t("auth.mfaHintWebauthn"),
);
const pending = ref(false);
const error = ref("");

function redirectPath(): string {
  // Only in-app paths — anything else could bounce off-origin.
  const r = route.query.redirect;
  return typeof r === "string" && r.startsWith("/") && !r.startsWith("//")
    ? r
    : "/";
}

async function finish(accessToken: string) {
  await auth.signInWithToken(accessToken);
  await router.push(redirectPath());
}

async function submit() {
  error.value = "";
  pending.value = true;
  try {
    if (mfaToken.value) {
      // An empty submit would burn one of the pending login's
      // second-factor attempts server-side — guard it here.
      if (!canVerifyCode.value || !code.value.trim()) {
        error.value = t("auth.codeRequired");
        return;
      }
      const res = await twoFactorVerify({
        mfaToken: mfaToken.value,
        code: code.value.trim(),
      });
      await finish(res.access_token);
    } else {
      if (!username.value.trim() || !password.value) {
        error.value = t("auth.credentialsRequired");
        return;
      }
      const res = await passwordToken({
        username: username.value.trim(),
        password: password.value,
      });
      await finish(res.access_token);
    }
  } catch (e) {
    if (e instanceof MfaRequired) {
      mfaToken.value = e.mfaToken;
      mfaMethods.value = e.methods;
      code.value = "";
    } else {
      error.value = e instanceof ApiError ? e.message : t("auth.signInFailed");
      // A failed second factor invalidates the pending login — return
      // to the credential form.
      mfaToken.value = null;
    }
  } finally {
    pending.value = false;
  }
}
</script>

<template>
  <Card class="signin">
    <template #header>{{ t("auth.signInTitle") }}</template>
    <form class="signin__form" @submit.prevent="submit">
      <template v-if="mfaToken">
        <p class="signin__hint">{{ mfaHint }}</p>
        <template v-if="canVerifyCode">
          <Input
            v-model="code"
            :label="t('auth.codeLabel')"
            :error="error || undefined"
            :disabled="pending"
            inputmode="numeric"
            autocomplete="one-time-code"
          />
          <Button type="submit" :loading="pending" :disabled="pending">
            <FontAwesomeIcon icon="shield-halved" />
            {{ t("auth.verify") }}
          </Button>
        </template>
      </template>
      <template v-else>
        <Input
          v-model="username"
          :label="t('auth.usernameLabel')"
          :disabled="pending"
          autocomplete="username"
        />
        <Input
          v-model="password"
          :label="t('auth.passwordLabel')"
          :error="error || undefined"
          :disabled="pending"
          type="password"
          autocomplete="current-password"
        />
        <Button type="submit" :loading="pending" :disabled="pending">
          <FontAwesomeIcon icon="right-to-bracket" />
          {{ t("auth.signIn") }}
        </Button>
        <p class="signin__alt">
          {{ t("auth.noAccount") }}
          <router-link to="/register">{{ t("auth.register") }}</router-link>
        </p>
      </template>
    </form>
  </Card>
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
.signin__alt {
  color: var(--text-muted);
  margin: 0;
}
</style>
