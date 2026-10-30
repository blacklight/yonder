<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import {
  listAuditLog,
  type AuditEntry,
  type AuditFilters,
} from "@/api/adminAudit";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import Spinner from "@/components/ui/Spinner.vue";

const { t } = useI18n();

const entries = ref<AuditEntry[]>([]);
const next = ref<string | undefined>();
const loading = ref(true);
const error = ref("");
const filters = reactive<AuditFilters>({
  action: "",
  actor_account_id: "",
  target_type: "",
  target_id: "",
});

function clean(f: AuditFilters): AuditFilters {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(f)) {
    if (v !== "" && v !== undefined) out[k] = v;
  }
  return out as AuditFilters;
}

async function load(append = false) {
  loading.value = true;
  error.value = "";
  try {
    const page = await listAuditLog({ ...clean(filters), limit: 40 });
    entries.value = append ? [...entries.value, ...page.items] : page.items;
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
  const max_id = new URL(next.value, window.location.origin).searchParams.get(
    "max_id",
  );
  loading.value = true;
  try {
    const page = await listAuditLog({
      ...clean(filters),
      max_id: max_id ?? undefined,
      limit: 40,
    });
    entries.value = [...entries.value, ...page.items];
    next.value = page.next;
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

function actor(e: AuditEntry) {
  if (!e.actor_account) return t("admin.audit.system");
  const a = e.actor_account;
  return a.domain ? `@${a.username}@${a.domain}` : `@${a.username}`;
}

onMounted(() => load());
</script>

<template>
  <div class="audit">
    <h2>{{ t("admin.nav.audit") }}</h2>
    <form class="audit__filters" @submit.prevent="load()">
      <Input
        v-model="filters.action"
        :label="t('admin.audit.filterAction')"
        placeholder="config."
        clearable
      />
      <Input
        v-model="filters.actor_account_id"
        :label="t('admin.audit.filterActor')"
        inputmode="numeric"
        clearable
      />
      <Input
        v-model="filters.target_type"
        :label="t('admin.audit.filterTargetType')"
        placeholder="config"
        clearable
      />
      <Input
        v-model="filters.target_id"
        :label="t('admin.audit.filterTargetId')"
        inputmode="numeric"
        clearable
      />
      <Button type="submit" variant="secondary" size="sm">
        {{ t("admin.audit.apply") }}
      </Button>
    </form>

    <p v-if="loading && entries.length === 0" class="audit__status">
      <Spinner />
    </p>
    <p v-else-if="error" class="audit__error" role="alert">{{ error }}</p>
    <p v-else-if="entries.length === 0" class="audit__status">
      {{ t("admin.audit.empty") }}
    </p>
    <table v-else class="audit__table">
      <thead>
        <tr>
          <th>{{ t("admin.audit.cols.when") }}</th>
          <th>{{ t("admin.audit.cols.actor") }}</th>
          <th>{{ t("admin.audit.cols.action") }}</th>
          <th>{{ t("admin.audit.cols.target") }}</th>
          <th>{{ t("admin.audit.cols.details") }}</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="e in entries" :key="e.id">
          <td class="audit__when">
            <time :datetime="e.created_at">{{
              new Date(e.created_at).toLocaleString()
            }}</time>
          </td>
          <td>{{ actor(e) }}</td>
          <td>
            <code>{{ e.action }}</code>
          </td>
          <td>
            <code
              >{{ e.target_type
              }}{{ e.target_id ? `#${e.target_id}` : "" }}</code
            >
          </td>
          <td class="audit__details">
            <code>{{ JSON.stringify(e.details) }}</code>
          </td>
        </tr>
      </tbody>
    </table>
    <Button
      v-if="next"
      variant="secondary"
      size="sm"
      :loading="loading"
      @click="loadMore"
    >
      {{ t("common.loadMore") }}
    </Button>
  </div>
</template>

<style scoped>
.audit__filters {
  display: flex;
  flex-wrap: wrap;
  align-items: end;
  gap: var(--space-3);
  margin-bottom: var(--space-4);
}
.audit__filters :deep(.field) {
  max-width: 12rem;
}
.audit__table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--font-sm, 0.8125rem);
}
.audit__table th {
  text-align: left;
  font-size: var(--text-xs);
  color: var(--text-muted);
  border-bottom: 1px solid var(--border);
  padding: var(--space-1) var(--space-2) var(--space-1) 0;
}
.audit__table td {
  padding: var(--space-2) var(--space-2) var(--space-2) 0;
  border-bottom: 1px solid var(--border);
  vertical-align: top;
}
.audit__when {
  white-space: nowrap;
}
.audit__details code {
  font-size: var(--text-xs);
  word-break: break-all;
}
.audit__status {
  color: var(--text-muted);
}
.audit__error {
  color: var(--danger);
}
</style>
