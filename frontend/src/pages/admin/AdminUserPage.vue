<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import { showAdminAccount, type AdminAccount } from "@/api/adminUsers";
import AccountActions from "@/components/admin/AccountActions.vue";

const props = defineProps<{ id: string }>();
const { t } = useI18n();

const account = ref<AdminAccount | null>(null);
const error = ref("");
const loading = ref(true);

const acct = computed(() => {
  const a = account.value;
  if (!a) return "";
  return a.domain ? `${a.username}@${a.domain}` : a.username;
});

async function load() {
  loading.value = true;
  error.value = "";
  try {
    account.value = await showAdminAccount(props.id);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

onMounted(load);
// Vue reuses the component when the :id param changes (back/forward).
watch(() => props.id, load);

function onUpdated(a: AdminAccount) {
  account.value = a;
}
</script>

<template>
  <div class="admin-user">
    <p v-if="loading" class="admin-user__status">{{ t("common.loading") }}</p>
    <p v-else-if="error" class="admin-user__error" role="alert">{{ error }}</p>
    <template v-else-if="account">
      <h2>@{{ acct }}</h2>
      <div class="admin-user__badges">
        <span class="admin-user__badge admin-user__badge--role">
          {{ t(`admin.users.roles.${account.role}`) }}
        </span>
        <span v-if="account.domain" class="admin-user__badge">
          {{ t("admin.users.remote") }}
        </span>
        <span
          v-if="account.suspended"
          class="admin-user__badge admin-user__badge--warn"
        >
          {{ t("admin.users.state.suspended") }}
        </span>
        <span
          v-if="account.limited"
          class="admin-user__badge admin-user__badge--warn"
        >
          {{ t("admin.users.state.limited") }}
        </span>
        <span
          v-if="account.disabled"
          class="admin-user__badge admin-user__badge--warn"
        >
          {{ t("admin.users.state.disabled") }}
        </span>
        <span
          v-if="account.approved === false"
          class="admin-user__badge admin-user__badge--warn"
        >
          {{ t("admin.users.state.pending") }}
        </span>
        <span v-if="account.email_verified === false" class="admin-user__badge">
          {{ t("admin.users.state.unverified") }}
        </span>
        <span v-if="account.force_sensitive" class="admin-user__badge">
          {{ t("admin.users.state.sensitive") }}
        </span>
      </div>
      <p class="admin-user__meta">
        <span v-if="account.email" class="admin-user__email">{{
          account.email
        }}</span>
        <router-link :to="`/@${acct}`">{{
          t("admin.users.viewProfile")
        }}</router-link>
      </p>
      <AccountActions :account="account" @updated="onUpdated" />
    </template>
  </div>
</template>

<style scoped>
.admin-user__badges {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
  margin: var(--space-2) 0;
}
.admin-user__badge {
  padding: 0 var(--space-2);
  border-radius: var(--radius-sm);
  background: var(--bg-raised);
  color: var(--text-muted);
  font-size: var(--text-xs);
}
.admin-user__badge--role {
  background: var(--accent-soft, var(--bg-raised));
  color: var(--accent-link);
}
.admin-user__badge--warn {
  background: var(--danger);
  color: var(--accent-fg, #fff);
}
.admin-user__meta {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-3);
  font-size: var(--text-xs);
}
.admin-user__email {
  color: var(--text-muted);
}
.admin-user__status {
  color: var(--text-muted);
}
.admin-user__error {
  color: var(--danger);
}
</style>
