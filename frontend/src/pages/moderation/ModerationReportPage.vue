<script setup lang="ts">
import { onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import {
  assignReportToSelf,
  getAdminReport,
  reopenReport,
  resolveReport,
  unassignReport,
  type AdminReport,
} from "@/api/moderation";
import { adminStatusAction } from "@/api/admin";
import { showAdminAccount, type AdminAccount } from "@/api/adminUsers";
import { useAuthStore } from "@/stores/auth";
import Button from "@/components/ui/Button.vue";
import AccountActions from "@/components/admin/AccountActions.vue";
import ConfirmDialog from "@/components/admin/ConfirmDialog.vue";

const props = defineProps<{ id: string }>();
const { t } = useI18n();
const auth = useAuthStore();

const report = ref<AdminReport | null>(null);
const target = ref<AdminAccount | null>(null);
const loading = ref(true);
const error = ref("");
const pending = ref(false);
const statusConfirm = ref<null | "delete" | "sensitive">(null);

onMounted(load);
// The component is reused when navigating between report ids.
watch(() => props.id, load);

async function load() {
  loading.value = true;
  error.value = "";
  try {
    report.value = await getAdminReport(props.id);
    if (report.value.target_account) {
      // The admin view carries the moderation state Account lacks.
      target.value = await showAdminAccount(report.value.target_account.id);
    }
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

async function act(fn: () => Promise<AdminReport>) {
  pending.value = true;
  error.value = "";
  try {
    report.value = await fn();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

const resolve = () => act(() => resolveReport(props.id));
const reopen = () => act(() => reopenReport(props.id));
const assignSelf = () => act(() => assignReportToSelf(props.id));
const unassign = () => act(() => unassignReport(props.id));

async function statusAction() {
  const type = statusConfirm.value;
  statusConfirm.value = null;
  if (!type || !report.value?.status_id) return;
  pending.value = true;
  error.value = "";
  try {
    await adminStatusAction(report.value.status_id, type);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

function acct(a: AdminReport["target_account"]) {
  return a?.acct ?? "?";
}
</script>

<template>
  <main class="page">
    <p v-if="loading" class="report__status">{{ t("common.loading") }}</p>
    <p v-else-if="error && !report" class="report__error" role="alert">
      {{ error }}
    </p>
    <template v-else-if="report">
      <h1>{{ t("moderation.reportTitle", { id: report.id }) }}</h1>
      <dl class="report__facts">
        <dt>{{ t("moderation.fields.category") }}</dt>
        <dd>{{ t(`moderation.categories.${report.category}`) }}</dd>
        <dt>{{ t("moderation.fields.reporter") }}</dt>
        <dd>@{{ acct(report.account) }}</dd>
        <dt>{{ t("moderation.fields.target") }}</dt>
        <dd>@{{ acct(report.target_account) }}</dd>
        <dt v-if="report.comment">{{ t("moderation.fields.comment") }}</dt>
        <dd v-if="report.comment">{{ report.comment }}</dd>
        <dt>{{ t("moderation.fields.created") }}</dt>
        <dd>{{ new Date(report.created_at).toLocaleString() }}</dd>
        <template v-if="report.action_taken">
          <dt>{{ t("moderation.fields.resolvedAt") }}</dt>
          <dd>
            {{
              report.action_taken_at
                ? new Date(report.action_taken_at).toLocaleString()
                : "—"
            }}
          </dd>
        </template>
        <dt>{{ t("moderation.fields.assignee") }}</dt>
        <dd>
          {{
            report.assigned_account
              ? `@${report.assigned_account.acct}`
              : t("moderation.unassigned")
          }}
        </dd>
        <template v-if="report.status_id">
          <dt>{{ t("moderation.fields.status") }}</dt>
          <dd>
            <router-link :to="`/statuses/${report.status_id}`">
              #{{ report.status_id }}
            </router-link>
          </dd>
        </template>
      </dl>

      <div class="report__actions">
        <template v-if="!report.action_taken">
          <Button size="sm" :disabled="pending" @click="resolve">
            {{ t("moderation.actions.resolve") }}
          </Button>
          <Button
            v-if="!report.assigned_account"
            variant="secondary"
            size="sm"
            :disabled="pending"
            @click="assignSelf"
          >
            {{ t("moderation.actions.assignSelf") }}
          </Button>
          <Button
            v-else
            variant="secondary"
            size="sm"
            :disabled="pending"
            @click="unassign"
          >
            {{ t("moderation.actions.unassign") }}
          </Button>
        </template>
        <Button
          v-else
          variant="secondary"
          size="sm"
          :disabled="pending"
          @click="reopen"
        >
          {{ t("moderation.actions.reopen") }}
        </Button>
        <template v-if="report.status_id">
          <Button
            variant="secondary"
            size="sm"
            :disabled="pending"
            @click="statusConfirm = 'sensitive'"
          >
            {{ t("moderation.actions.markSensitive") }}
          </Button>
          <Button
            variant="danger"
            size="sm"
            :disabled="pending"
            @click="statusConfirm = 'delete'"
          >
            {{ t("moderation.actions.deletePost") }}
          </Button>
        </template>
      </div>
      <p v-if="error" class="report__error" role="alert">{{ error }}</p>

      <section v-if="target" class="report__target">
        <h2 class="report__subtitle">
          {{ t("moderation.targetActions") }}
        </h2>
        <AccountActions
          :account="target"
          :report-id="Number(report.id)"
          @updated="target = $event"
        />
      </section>
      <p v-if="!auth.isAdmin" class="report__hint">
        {{ t("moderation.moderatorHint") }}
      </p>
    </template>

    <ConfirmDialog
      v-if="statusConfirm"
      :title="
        t(
          `moderation.actions.${statusConfirm === 'delete' ? 'deletePost' : 'markSensitive'}`,
        )
      "
      :message="t(`moderation.confirm.${statusConfirm}`)"
      :danger="statusConfirm === 'delete'"
      :pending="pending"
      @confirm="statusAction"
      @cancel="statusConfirm = null"
    />
  </main>
</template>

<style scoped>
.report__facts {
  display: grid;
  grid-template-columns: minmax(8rem, auto) 1fr;
  gap: var(--space-1) var(--space-4);
  margin: var(--space-3) 0;
}
.report__facts dt {
  color: var(--text-muted);
  font-size: var(--text-xs);
  padding-top: var(--space-1);
}
.report__facts dd {
  margin: 0;
}
.report__actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}
.report__target {
  margin-top: var(--space-6);
}
.report__subtitle {
  font-size: var(--font-sm, 0.8125rem);
  margin-bottom: var(--space-2);
}
.report__hint {
  margin-top: var(--space-4);
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.report__status {
  color: var(--text-muted);
}
.report__error {
  color: var(--danger);
}
</style>
