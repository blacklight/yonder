<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import { listAdminReports, type AdminReport } from "@/api/moderation";
import AccountRowSkeleton from "@/components/ui/AccountRowSkeleton.vue";

const { t } = useI18n();

const reports = ref<AdminReport[]>([]);
const resolved = ref(false);
const loading = ref(true);
const error = ref("");

function acct(a: AdminReport["target_account"]) {
  if (!a) return "?";
  return a.acct;
}

async function load() {
  loading.value = true;
  error.value = "";
  try {
    reports.value = await listAdminReports({ resolved: resolved.value });
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

onMounted(load);
</script>

<template>
  <main class="page">
    <h1>{{ t("nav.moderation") }}</h1>
    <div class="reports__filters">
      <label class="reports__filter">
        <input v-model="resolved" type="checkbox" @change="load" />
        {{ t("moderation.showResolved") }}
      </label>
    </div>
    <ul
      v-if="loading"
      class="reports__list"
      role="status"
      :aria-label="t('common.loading')"
    >
      <li v-for="n in 4" :key="n" class="reports__item reports__item--skel">
        <AccountRowSkeleton :avatar="false" />
      </li>
    </ul>
    <p v-else-if="error" class="reports__error" role="alert">{{ error }}</p>
    <p v-else-if="reports.length === 0" class="reports__status">
      {{ t("moderation.empty") }}
    </p>
    <ul v-else class="reports__list">
      <li v-for="r in reports" :key="r.id" class="reports__item">
        <router-link
          :to="{ name: 'moderation-report', params: { id: r.id } }"
          class="reports__link"
        >
          <span class="reports__id">#{{ r.id }}</span>
          <span class="reports__category">{{
            t(`moderation.categories.${r.category}`)
          }}</span>
          <span class="reports__pair">
            @{{ acct(r.account) }} → @{{ acct(r.target_account) }}
          </span>
          <span class="reports__meta">
            <time :datetime="r.created_at">{{
              new Date(r.created_at).toLocaleDateString()
            }}</time>
            <span v-if="r.action_taken" class="reports__resolved">
              {{ t("moderation.resolved") }}
            </span>
            <span v-else-if="r.assigned_account" class="reports__assigned">
              {{ t("moderation.assigned") }}
            </span>
          </span>
        </router-link>
        <p v-if="r.comment" class="reports__comment">{{ r.comment }}</p>
      </li>
    </ul>
  </main>
</template>

<style scoped>
.reports__filters {
  margin-bottom: var(--space-3);
}
.reports__filter {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--font-sm, 0.8125rem);
  color: var(--text-muted);
}
.reports__list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.reports__item {
  border-top: 1px solid var(--border);
  padding: var(--space-3) 0;
}
/* The skeleton row supplies its own padding. */
.reports__item--skel {
  padding: 0;
}
.reports__link {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: var(--space-3);
  color: var(--text);
  text-decoration: none;
}
.reports__link:hover .reports__pair {
  text-decoration: underline;
}
.reports__id {
  color: var(--text-muted);
  font-size: var(--text-xs);
}
.reports__category {
  font-weight: 600;
  font-size: var(--font-sm, 0.8125rem);
}
.reports__meta {
  margin-left: auto;
  display: inline-flex;
  gap: var(--space-2);
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.reports__resolved {
  color: var(--accent-link);
}
.reports__assigned {
  color: var(--text-muted);
}
.reports__comment {
  margin: var(--space-1) 0 0;
  font-size: var(--font-sm, 0.8125rem);
  color: var(--text-muted);
}
.reports__status {
  color: var(--text-muted);
}
.reports__error {
  color: var(--danger);
}
</style>
