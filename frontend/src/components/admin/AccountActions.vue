<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import { adminAccountAction, setRole, type AdminRole } from "@/api/admin";
import {
  emailResetLink,
  resetPasswordLink,
  reverseAdminAction,
  type AdminAccount,
  type AdminReversal,
} from "@/api/adminUsers";
import { useAuthStore } from "@/stores/auth";
import Button from "@/components/ui/Button.vue";
import ConfirmDialog from "./ConfirmDialog.vue";

const props = defineProps<{
  account: AdminAccount;
  /** Attach the action to a report (POST body `report_id`). */
  reportId?: number;
}>();
const emit = defineEmits<{ updated: [account: AdminAccount] }>();
const { t } = useI18n();
const auth = useAuthStore();

const pending = ref(false);
const error = ref("");
const notice = ref("");
const confirm = ref<null | {
  action: () => Promise<void>;
  key: string;
  danger?: boolean;
}>(null);
const resetUrl = ref("");

interface ActionDef {
  key: string;
  danger?: boolean;
  /** Also asks for confirmation (non-destructive actions like role changes). */
  confirm?: boolean;
  adminOnly?: boolean;
  hidden?: boolean;
  run: () => Promise<void>;
}

async function doAccountAction(type: string) {
  const updated = await adminAccountAction(props.account.id, {
    type,
    ...(props.reportId ? { report_id: props.reportId } : {}),
  });
  emit("updated", updated);
}

async function doReverse(verb: AdminReversal) {
  emit("updated", await reverseAdminAction(props.account.id, verb));
}

async function doRole(role: AdminRole) {
  emit("updated", await setRole(props.account.id, role));
}

async function doResetPassword() {
  resetUrl.value = (await resetPasswordLink(props.account.id)).reset_url;
}

async function doEmailReset() {
  await emailResetLink(props.account.id);
  notice.value = t("admin.users.emailResetSent");
}

const acct = computed(() =>
  props.account.domain
    ? `${props.account.username}@${props.account.domain}`
    : props.account.username,
);

// Reversal verbs are shown only when the matching state applies.
const actions = computed<ActionDef[]>(() =>
  [
    {
      key: "limit",
      run: () => doAccountAction("silence"),
      hidden: props.account.limited,
    },
    {
      key: "unlimit",
      run: () => doReverse("unsilence"),
      hidden: !props.account.limited,
    },
    {
      key: "suspend",
      danger: true,
      run: () => doAccountAction("suspend"),
      hidden: props.account.suspended,
    },
    {
      key: "unsuspend",
      run: () => doReverse("unsuspend"),
      hidden: !props.account.suspended,
    },
    {
      key: "markSensitive",
      run: () => doAccountAction("sensitive"),
      hidden: props.account.force_sensitive,
    },
    {
      key: "unmarkSensitive",
      run: () => doReverse("unsensitive"),
      hidden: !props.account.force_sensitive,
    },
    {
      key: "disable",
      danger: true,
      run: () => doAccountAction("disable"),
      hidden: props.account.disabled,
    },
    {
      key: "enable",
      run: () => doReverse("enable"),
      hidden: !props.account.disabled,
    },
    {
      key: "delete",
      danger: true,
      adminOnly: true,
      run: () => doAccountAction("delete"),
    },
    {
      key: "resetPassword",
      run: doResetPassword,
      hidden: !!props.account.domain,
    },
    {
      key: "emailReset",
      run: doEmailReset,
      hidden: !!props.account.domain,
    },
    {
      key: "disable2fa",
      run: () => doAccountAction("disable_2fa"),
      hidden: !!props.account.domain,
    },
    {
      key: "verifyEmail",
      run: () => doAccountAction("verify_email"),
      hidden: !!props.account.domain,
    },
    {
      key: "requirePasswordReset",
      run: () => doAccountAction("require_password_reset"),
      hidden: !!props.account.domain,
    },
    {
      key: "makeModerator",
      adminOnly: true,
      confirm: true,
      run: () => doRole("moderator"),
      hidden: props.account.role !== "user",
    },
    {
      key: "makeAdmin",
      adminOnly: true,
      confirm: true,
      run: () => doRole("admin"),
      hidden: props.account.role === "admin",
    },
    {
      key: "demote",
      adminOnly: true,
      confirm: true,
      run: () => doRole("user"),
      hidden: props.account.role === "user",
    },
  ].filter(
    (a) =>
      !a.hidden &&
      (!a.adminOnly || auth.isAdmin) &&
      // The backend rejects self-targeting actions (422) — don't offer them.
      props.account.id !== String(auth.account?.id ?? ""),
  ),
);

const confirmMessage = computed(() =>
  confirm.value
    ? t(`admin.users.confirm.${confirm.value.key}`, { acct: acct.value })
    : "",
);

function ask(a: ActionDef) {
  if (a.danger || a.confirm) {
    confirm.value = { key: a.key, action: a.run, danger: a.danger };
  } else {
    void run(a.run);
  }
}

async function run(fn: () => Promise<void>) {
  pending.value = true;
  error.value = "";
  notice.value = "";
  resetUrl.value = "";
  try {
    await fn();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

async function confirmRun() {
  const a = confirm.value;
  confirm.value = null;
  if (a) await run(a.action);
}

async function copyResetUrl() {
  try {
    await navigator.clipboard.writeText(resetUrl.value);
  } catch {
    // Clipboard API may be unavailable (insecure context) — the link
    // stays visible for manual copy.
  }
}
</script>

<template>
  <div class="account-actions">
    <div class="account-actions__buttons">
      <Button
        v-for="a in actions"
        :key="a.key"
        :variant="a.danger ? 'danger' : 'secondary'"
        size="sm"
        :disabled="pending"
        @click="ask(a)"
      >
        {{ t(`admin.users.actions.${a.key}`) }}
      </Button>
    </div>
    <p v-if="resetUrl" class="account-actions__link">
      <code>{{ resetUrl }}</code>
      <Button variant="secondary" size="sm" @click="copyResetUrl">
        {{ t("admin.users.copyLink") }}
      </Button>
    </p>
    <p v-if="notice" class="account-actions__notice">{{ notice }}</p>
    <p v-if="error" class="account-actions__error" role="alert">{{ error }}</p>

    <ConfirmDialog
      v-if="confirm"
      :title="t(`admin.users.actions.${confirm.key}`)"
      :message="confirmMessage"
      :danger="confirm.danger"
      :pending="pending"
      @confirm="confirmRun"
      @cancel="confirm = null"
    />
  </div>
</template>

<style scoped>
.account-actions__buttons {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}
.account-actions__link {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--text-xs);
  word-break: break-all;
}
.account-actions__notice {
  font-size: var(--text-xs);
  color: var(--accent-link);
}
.account-actions__error {
  font-size: var(--text-xs);
  color: var(--danger);
}
</style>
