<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import { ApiError } from "@/api/errors";
import { lookup } from "@/api/accounts";
import {
  createAdminAccount,
  deleteAdminAccount,
  listAdminAccounts,
  type AdminAccount,
  type AdminAccountFilters,
} from "@/api/adminUsers";
import type { AdminRole } from "@/api/admin";
import { useAuthStore } from "@/stores/auth";
import Input from "@/components/ui/Input.vue";
import Button from "@/components/ui/Button.vue";
import ConfirmDialog from "@/components/admin/ConfirmDialog.vue";

const { t } = useI18n();
const router = useRouter();
const auth = useAuthStore();

// --- Account index --------------------------------------------------------

const accounts = ref<AdminAccount[]>([]);
const next = ref<string | undefined>();
const loading = ref(true);
const error = ref("");
const filters = reactive({
  search: "",
  origin: "local" as "" | "local" | "remote",
  status: "",
});

function cleanFilters(): AdminAccountFilters {
  const f: AdminAccountFilters = { limit: 40 };
  if (filters.search.trim()) f.search = filters.search.trim();
  if (filters.origin) f.origin = filters.origin;
  if (filters.status)
    f.status = filters.status as AdminAccountFilters["status"];
  return f;
}

async function load(append = false, max_id?: string) {
  loading.value = true;
  error.value = "";
  try {
    const page = await listAdminAccounts({ ...cleanFilters(), max_id });
    accounts.value = append ? [...accounts.value, ...page.items] : page.items;
    next.value = page.next;
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

async function loadMore() {
  if (!next.value) return;
  // The backend emits relative `Link` targets (path?query) — resolve
  // against the origin before parsing.
  const max_id =
    new URL(next.value, window.location.origin).searchParams.get("max_id") ??
    undefined;
  await load(true, max_id);
}

onMounted(() => load());

function acctOf(a: AdminAccount) {
  return a.domain ? `${a.username}@${a.domain}` : a.username;
}

const isSelf = (a: AdminAccount) => a.id === String(auth.account?.id ?? "");

// --- Create ---------------------------------------------------------------

const blankCreate = () => ({
  username: "",
  email: "",
  password: "",
  role: "user" as AdminRole,
  skip_verification: false,
});
const create = reactive(blankCreate());
const creating = ref(false);
const createError = ref("");
const notice = ref("");

async function submitCreate() {
  creating.value = true;
  createError.value = "";
  notice.value = "";
  try {
    const account = await createAdminAccount({
      username: create.username.trim(),
      email: create.email.trim(),
      password: create.password,
      role: create.role,
      skip_verification: create.skip_verification,
    });
    notice.value = t("admin.users.create.created", {
      acct: acctOf(account),
    });
    Object.assign(create, blankCreate());
    // The new row belongs in the current view only when it matches
    // the filters — refresh instead of guessing.
    await load();
  } catch (e) {
    createError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    creating.value = false;
  }
}

// --- Delete ---------------------------------------------------------------

const removing = ref<AdminAccount | null>(null);
const pending = ref(false);

async function remove() {
  const account = removing.value;
  removing.value = null;
  if (!account) return;
  pending.value = true;
  error.value = "";
  try {
    await deleteAdminAccount(account.id);
    accounts.value = accounts.value.filter((a) => a.id !== account.id);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

// --- Handle lookup (remote accounts not yet cached) ------------------------

const acct = ref("");
const lookupError = ref("");
const lookupPending = ref(false);

async function go() {
  const handle = acct.value.trim().replace(/^@/, "");
  if (!handle) return;
  lookupPending.value = true;
  lookupError.value = "";
  try {
    const account = await lookup(handle);
    await router.push({ name: "admin-user", params: { id: account.id } });
  } catch (e) {
    lookupError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    lookupPending.value = false;
  }
}
</script>

<template>
  <div class="admin-users">
    <h2>{{ t("admin.nav.users") }}</h2>

    <form class="admin-users__filters" @submit.prevent="load()">
      <Input
        v-model="filters.search"
        :label="t('admin.users.filters.search')"
        :placeholder="t('admin.users.filters.searchHint')"
        inputmode="search"
        clearable
      />
      <label class="admin-users__field">
        <span>{{ t("admin.users.filters.origin") }}</span>
        <select v-model="filters.origin" @change="load()">
          <option value="">{{ t("admin.users.filters.all") }}</option>
          <option value="local">{{ t("admin.users.filters.local") }}</option>
          <option value="remote">{{ t("admin.users.filters.remote") }}</option>
        </select>
      </label>
      <label class="admin-users__field">
        <span>{{ t("admin.users.filters.status") }}</span>
        <select v-model="filters.status" @change="load()">
          <option value="">{{ t("admin.users.filters.all") }}</option>
          <option value="active">
            {{ t("admin.users.filters.statuses.active") }}
          </option>
          <option value="pending">
            {{ t("admin.users.filters.statuses.pending") }}
          </option>
          <option value="disabled">
            {{ t("admin.users.filters.statuses.disabled") }}
          </option>
          <option value="silenced">
            {{ t("admin.users.filters.statuses.silenced") }}
          </option>
          <option value="suspended">
            {{ t("admin.users.filters.statuses.suspended") }}
          </option>
        </select>
      </label>
      <Button type="submit" variant="secondary" size="sm" :loading="loading">
        {{ t("admin.users.filters.apply") }}
      </Button>
    </form>

    <p v-if="loading && accounts.length === 0" class="admin-users__status">
      {{ t("common.loading") }}
    </p>
    <p v-else-if="accounts.length === 0" class="admin-users__status">
      {{ t("admin.users.empty") }}
    </p>
    <ul v-else class="admin-users__list">
      <li v-for="a in accounts" :key="a.id" class="admin-users__item">
        <div class="admin-users__main">
          <div class="admin-users__head">
            <router-link
              class="admin-users__acct"
              :to="{ name: 'admin-user', params: { id: a.id } }"
              >@{{ acctOf(a) }}</router-link
            >
            <span class="admin-users__role">{{
              t(`admin.users.roles.${a.role}`)
            }}</span>
          </div>
          <div v-if="a.email" class="admin-users__email">{{ a.email }}</div>
          <div class="admin-users__badges">
            <span v-if="a.domain" class="admin-users__badge">{{
              t("admin.users.remote")
            }}</span>
            <span
              v-if="a.suspended"
              class="admin-users__badge admin-users__badge--warn"
              >{{ t("admin.users.state.suspended") }}</span
            >
            <span
              v-if="a.limited"
              class="admin-users__badge admin-users__badge--warn"
              >{{ t("admin.users.state.limited") }}</span
            >
            <span
              v-if="a.disabled"
              class="admin-users__badge admin-users__badge--warn"
              >{{ t("admin.users.state.disabled") }}</span
            >
            <span
              v-if="a.approved === false"
              class="admin-users__badge admin-users__badge--warn"
              >{{ t("admin.users.state.pending") }}</span
            >
            <span v-if="a.email_verified === false" class="admin-users__badge"
              >{{ t("admin.users.state.unverified") }}</span
            >
          </div>
          <div class="admin-users__when">
            {{ t("admin.users.joined") }}
            <time :datetime="a.created_at">{{
              new Date(a.created_at).toLocaleDateString()
            }}</time>
          </div>
        </div>
        <div class="admin-users__actions">
          <Button
            variant="secondary"
            size="sm"
            @click="router.push({ name: 'admin-user', params: { id: a.id } })"
            >{{ t("admin.users.manage") }}</Button
          >
          <Button
            v-if="!isSelf(a)"
            variant="danger"
            size="sm"
            :disabled="pending"
            @click="removing = a"
            >{{ t("admin.users.actions.delete") }}</Button
          >
        </div>
      </li>
    </ul>
    <Button
      v-if="next"
      variant="secondary"
      size="sm"
      :loading="loading"
      @click="loadMore"
    >
      {{ t("common.loadMore") }}
    </Button>
    <p v-if="error" class="admin-users__error" role="alert">{{ error }}</p>
    <p v-if="notice" class="admin-users__notice">{{ notice }}</p>

    <details class="admin-users__panel">
      <summary>{{ t("admin.users.create.title") }}</summary>
      <form class="admin-users__create" @submit.prevent="submitCreate">
        <div class="inputs">
          <div class="admin-users__create-row">
            <Input
              v-model="create.username"
              :label="t('admin.users.create.username')"
              autocomplete="off"
              required
            />
            <Input
              v-model="create.email"
              :label="t('admin.users.create.email')"
              type="email"
              autocomplete="off"
              required
            />
          </div>
          <div class="admin-users__create-row">
            <Input
              v-model="create.password"
              :label="t('admin.users.create.password')"
              type="password"
              autocomplete="new-password"
              required
            />
            <label class="admin-users__field">
              <span>{{ t("admin.users.create.role") }}</span>
              <select v-model="create.role">
                <option value="user">{{ t("admin.users.roles.user") }}</option>
                <option value="moderator">
                  {{ t("admin.users.roles.moderator") }}
                </option>
                <option value="admin">{{ t("admin.users.roles.admin") }}</option>
              </select>
            </label>
          </div>
          <label class="admin-users__check"
            ><input v-model="create.skip_verification" type="checkbox" />
            {{ t("admin.users.create.skipVerification") }}</label
          >
        </div>
        <div class="buttons">
          <Button
            type="submit"
            :loading="creating"
            :disabled="
              !create.username.trim() || !create.email.trim() || !create.password
            "
          >
            {{ t("admin.users.create.submit") }}
          </Button>
        </div>
        <div v-if="createError" class="notice">
          <p class="admin-users__error" role="alert">
            {{ createError }}
          </p>
        </div>
      </form>
    </details>

    <details class="admin-users__panel">
      <summary>{{ t("admin.users.lookupLabel") }}</summary>
      <form class="admin-users__lookup" @submit.prevent="go">
        <Input
          v-model="acct"
          :label="t('admin.users.lookupLabel')"
          :placeholder="t('admin.users.lookupHint')"
          :error="lookupError || undefined"
          inputmode="search"
          clearable
        />
        <Button type="submit" :loading="lookupPending" :disabled="!acct.trim()">
          {{ t("admin.users.lookup") }}
        </Button>
      </form>
    </details>

    <ConfirmDialog
      v-if="removing"
      :title="t('admin.users.actions.delete')"
      :message="t('admin.users.confirm.delete', { acct: acctOf(removing) })"
      danger
      :pending="pending"
      @confirm="remove"
      @cancel="removing = null"
    />
  </div>
</template>

<style scoped>
.admin-users__filters {
  display: flex;
  flex-wrap: wrap;
  align-items: end;
  gap: var(--space-3);
  margin-bottom: var(--space-4);
}
.admin-users__filters :deep(.field) {
  min-width: 14rem;
}
.admin-users__field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  font-size: var(--font-sm, 0.8125rem);
  color: var(--text-muted);
  font-weight: 600;
}
.admin-users__field select {
  padding: var(--space-1) var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg);
  color: var(--text);
  font: inherit;
}
.admin-users__list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.admin-users__item {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) 0;
  border-top: 1px solid var(--border);
}
.admin-users__main {
  flex: 1 1 16rem;
  min-width: 0;
}
.admin-users__head {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: var(--space-2);
}
.admin-users__acct {
  font-weight: 600;
  word-break: break-all;
}
.admin-users__role {
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.admin-users__email {
  font-size: var(--font-sm, 0.8125rem);
  color: var(--text-muted);
  word-break: break-all;
}
.admin-users__badges {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-1);
  margin-top: var(--space-1);
}
.admin-users__badges:empty {
  margin-top: 0;
}
.admin-users__badge {
  padding: 0 var(--space-2);
  border-radius: var(--radius-sm);
  background: var(--bg-raised);
  color: var(--text-muted);
  font-size: var(--text-xs);
}
.admin-users__badge--warn {
  background: var(--danger);
  color: var(--accent-fg, #fff);
}
.admin-users__when {
  margin-top: var(--space-1);
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.admin-users__actions {
  display: inline-flex;
  gap: var(--space-2);
  margin-inline-start: auto;
}
.admin-users__status {
  color: var(--text-muted);
}
.admin-users__error {
  font-size: var(--text-xs);
  color: var(--danger);
}
.admin-users__notice {
  font-size: var(--text-xs);
  color: var(--accent-link);
}
.admin-users__panel {
  margin-top: var(--space-4);
}
.admin-users__panel summary {
  cursor: pointer;
  font-weight: 600;
  color: var(--text-muted);
}
.admin-users__create {
  max-width: 32rem;
  margin-top: var(--space-3);
}
.admin-users__create-row {
  display: flex;
  gap: var(--space-3);
  align-items: end;
}
.admin-users__create-row > * {
  flex: 1;
}
.admin-users__check {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--font-sm, 0.8125rem);
  margin: var(--space-2) 0;
}
.admin-users__lookup {
  display: flex;
  align-items: end;
  gap: var(--space-3);
  max-width: 28rem;
  margin-top: var(--space-3);
}
</style>
