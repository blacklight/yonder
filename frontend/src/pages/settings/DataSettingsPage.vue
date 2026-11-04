<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import { ApiError } from "@/api/errors";
import {
  beginImportConnection,
  cancelImport,
  createImport,
  createImportSession,
  listImportConnections,
  listImports,
  resumeImport,
  revokeImportConnection,
} from "@/api/settings";
import type { ImportConnection, TransferRow } from "@/api/entities";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";
import { EXPORT_KINDS as csvKinds, useExports } from "@/composables/useExports";
import { importErrorKey } from "@/utils/importError";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";
import Skeleton from "@/components/ui/Skeleton.vue";
import SettingsNav from "@/components/settings/SettingsNav.vue";

const { t } = useI18n();
const route = useRoute();
const router = useRouter();

// ---------------------------------------------------------------------------
// Exports — inline CSV/JSON downloads and the queued archive job share
// one instance; `errorArea` keeps each error in its own section.
// `followers` is export-only upstream and not a Mastodon surface; omit it.
// ---------------------------------------------------------------------------

const {
  rows: exportRows,
  loading: exportsLoading,
  busy: exportBusy,
  error: exportError,
  errorArea,
  refresh: refreshExports,
  requestArchive,
  downloadArchive,
  downloadKind,
} = useExports();

// ---------------------------------------------------------------------------
// Archive/CSV upload import — kind + merge/overwrite + optional federation.
// ---------------------------------------------------------------------------

const importKinds = [
  "archive",
  "following",
  "blocking",
  "muting",
  "domain_blocks",
  "bookmarks",
  "lists",
  "filters",
] as const;
const upload = reactive({
  kind: "archive" as string,
  mode: "merge" as "merge" | "overwrite",
  federate: false,
  file: null as File | null,
});
const uploading = ref(false);
const uploadError = ref("");

function pickFile(ev: Event) {
  upload.file = (ev.target as HTMLInputElement).files?.[0] ?? null;
}

async function submitUpload() {
  if (!upload.file) return;
  if (
    upload.mode === "overwrite" &&
    !window.confirm(t("settingsData.overwriteConfirm"))
  )
    return;
  uploadError.value = "";
  uploading.value = true;
  try {
    const row = await createImport(
      upload.kind,
      upload.mode,
      upload.file,
      upload.federate,
    );
    imports.value = [row, ...imports.value];
    upload.file = null;
  } catch (e) {
    uploadError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    uploading.value = false;
  }
}

// ---------------------------------------------------------------------------
// Remote OAuth connections — begin navigates to the source server; the
// callback lands back here with `?connected=1` / `?import_error=`.
// ---------------------------------------------------------------------------

const connections = ref<ImportConnection[]>([]);
const connError = ref("");
const connNotice = ref("");
const beginForm = reactive({ origin: "" });
const beginning = ref(false);

async function refreshConnections() {
  try {
    connections.value = await listImportConnections();
  } catch (e) {
    connError.value = e instanceof ApiError ? e.message : String(e);
  }
}

async function beginConnection() {
  if (!session.categories.length) return;
  connError.value = "";
  beginning.value = true;
  try {
    const { authorization_url } = await beginImportConnection(
      beginForm.origin.trim(),
      session.categories,
    );
    window.location.assign(authorization_url);
  } catch (e) {
    connError.value = e instanceof ApiError ? e.message : String(e);
    beginning.value = false;
  }
}

const confirmingRevoke = ref<string | null>(null);
const reauthorizing = ref<string | null>(null);

/** Scopes a connection was granted → the categories they cover. The
 * broad `read` grant covers every importable category. */
const SCOPE_CATEGORIES: Record<string, string[]> = {
  "read:follows": ["following"],
  "read:blocks": ["blocks"],
  "read:mutes": ["mutes"],
  "read:bookmarks": ["bookmarks"],
  "read:favourites": ["favourites"],
};
function grantedCategories(conn: ImportConnection): string[] {
  const scopes = conn.scopes.split(/\s+/).filter(Boolean);
  if (scopes.includes("read")) return [...apiCategories];
  return scopes.flatMap((s) => SCOPE_CATEGORIES[s] ?? []);
}

/** Re-consent a connection. The request unions the connection's
 * existing grant with the selected categories, so reauthorization can
 * only widen scopes, never silently narrow them (the upsert stores
 * exactly what the provider granted). */
async function reauthorize(conn: ImportConnection) {
  connError.value = "";
  reauthorizing.value = conn.id;
  try {
    const categories = [
      ...new Set([...grantedCategories(conn), ...session.categories]),
    ];
    const { authorization_url } = await beginImportConnection(
      conn.origin,
      categories,
    );
    window.location.assign(authorization_url);
  } catch (e) {
    connError.value = e instanceof ApiError ? e.message : String(e);
    reauthorizing.value = null;
  }
}

async function revokeConnection(id: string) {
  connError.value = "";
  try {
    await revokeImportConnection(id);
    connections.value = connections.value.filter((c) => c.id !== id);
  } catch (e) {
    connError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    confirmingRevoke.value = null;
  }
}

// ---------------------------------------------------------------------------
// Remote import session — categories, cap/order/range, federation opt-in.
// ---------------------------------------------------------------------------

const apiCategories = [
  "statuses",
  "dms",
  "bookmarks",
  "favourites",
  "following",
  "blocks",
  "mutes",
  "profile",
  "observations",
] as const;
const session = reactive({
  connection_id: "",
  categories: [] as string[],
  limit: null as number | null,
  order: "newest" as "newest" | "oldest",
  since: "",
  until: "",
  federate: false,
  overwrite_profile: false,
});
const creatingSession = ref(false);
const sessionError = ref("");

const activeConnections = computed(() =>
  connections.value.filter((c) => c.state === "active"),
);

function toggleCategory(cat: string) {
  const i = session.categories.indexOf(cat);
  if (i >= 0) session.categories.splice(i, 1);
  else session.categories.push(cat);
}

async function submitSession() {
  sessionError.value = "";
  creatingSession.value = true;
  try {
    await createImportSession({
      connection_id: session.connection_id,
      categories: session.categories,
      limit: session.limit ?? undefined,
      order: session.order,
      since: session.since ? new Date(session.since).toISOString() : undefined,
      until: session.until ? new Date(session.until).toISOString() : undefined,
      federate: session.federate,
      overwrite_profile: session.overwrite_profile,
    });
    connNotice.value = t("settingsData.sessionQueued");
    await refreshImports();
  } catch (e) {
    sessionError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    creatingSession.value = false;
  }
}

// ---------------------------------------------------------------------------
// Imports — per-category progress, cancel, resume.
// ---------------------------------------------------------------------------

const imports = ref<TransferRow[]>([]);
const importsLoading = ref(true);
const importsError = ref("");
const actingOn = ref<string | null>(null);

async function refreshImports() {
  try {
    imports.value = await listImports();
  } catch (e) {
    importsError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    importsLoading.value = false;
  }
}

const liveImports = computed(() =>
  imports.value.some((i) => ["pending", "running"].includes(i.state)),
);

async function cancel(id: string) {
  actingOn.value = id;
  importsError.value = "";
  try {
    await cancelImport(id);
    await refreshImports();
  } catch (e) {
    importsError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    actingOn.value = null;
  }
}

async function resume(row: TransferRow) {
  actingOn.value = row.id;
  importsError.value = "";
  try {
    const conn = activeConnections.value.find(
      (c) => c.origin === row.source_origin,
    );
    await resumeImport(row.id, conn?.id);
    await refreshImports();
  } catch (e) {
    importsError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    actingOn.value = null;
  }
}

// Poll while anything is live; stop when the queue drains.
let pollTimer: ReturnType<typeof setInterval> | undefined;
function poll() {
  clearInterval(pollTimer);
  pollTimer = setInterval(async () => {
    await refreshImports();
    if (!liveImports.value) clearInterval(pollTimer);
  }, 4000);
}

function stateLabel(row: TransferRow) {
  return t(`settingsData.state.${row.state}`, row.state);
}

/** Per-category rollup: `statuses: done, 120 · following: failed`. */
function categorySummary(row: TransferRow): string {
  return Object.entries(row.category_state ?? {})
    .map(([cat, st]) => {
      const status =
        st.unsupported === true
          ? "unsupported"
          : st.error
            ? "failed"
            : st.done === true
              ? "done"
              : row.state === "running"
                ? "running"
                : "pending";
      const bits = [t(`settingsData.catState.${status}`)];
      for (const key of ["processed", "skipped", "failed"] as const) {
        const n = st[key];
        if (typeof n === "number" && n > 0)
          bits.push(t(`settingsData.catCount.${key}`, { n }));
      }
      return `${t(`settingsData.cat.${cat}`, cat)}: ${bits.join(", ")}`;
    })
    .join(" · ");
}

onUnmounted(() => clearInterval(pollTimer));

onMounted(async () => {
  // OAuth callback landing: `?connected=1` confirms, `?import_error=`
  // carries a fixed reason code from the backend (allowlisted).
  if (route.query.connected === "1") {
    connNotice.value = t("settingsData.connected");
  } else if (typeof route.query.import_error === "string") {
    connError.value = t(importErrorKey(route.query.import_error));
  }
  if (route.query.connected || route.query.import_error) {
    void router.replace({ query: {} });
  }
  void refreshExports();
  void refreshConnections();
  void refreshImports().then(() => {
    if (liveImports.value) poll();
  });
});
</script>

<template>
  <div>
    <SettingsNav />
    <Card>
      <template #header>{{ t("settingsData.title") }}</template>
      <div class="data">
        <p class="data__hint">{{ t("settingsData.intro") }}</p>

        <section class="data__group">
          <h3 class="data__group-title">{{ t("settingsData.groupCsv") }}</h3>
          <p
            v-if="exportError && errorArea === 'csv'"
            class="data__error"
            role="alert"
          >
            {{ exportError }}
          </p>
          <ul class="data__kinds">
            <li v-for="kind in csvKinds" :key="kind">
              <Button
                variant="secondary"
                size="sm"
                :loading="exportBusy === kind"
                @click="downloadKind(kind)"
              >
                {{ t(`settingsData.kind.${kind}`) }}
              </Button>
            </li>
          </ul>
        </section>

        <section class="data__group">
          <h3 class="data__group-title">
            {{ t("settingsData.groupArchive") }}
          </h3>
          <p class="data__hint">{{ t("settingsData.archiveHint") }}</p>
          <p
            v-if="exportError && errorArea === 'archive'"
            class="data__error"
            role="alert"
          >
            {{ exportError }}
          </p>
          <div class="data__actions">
            <Button :loading="exportBusy === 'archive'" @click="requestArchive">
              {{ t("settingsData.requestArchive") }}
            </Button>
          </div>
          <ul v-if="exportsLoading" class="data__list">
            <li class="data__row"><Skeleton width="55%" /></li>
          </ul>
          <ul v-else-if="exportRows.length" class="data__list">
            <li v-for="row in exportRows" :key="row.id" class="data__row">
              <div class="data__main">
                <span class="data__name">{{ row.kind }}</span>
                <small class="data__meta">
                  {{ stateLabel(row) }}
                  ·
                  <span :title="formatAbsoluteDate(row.created_at)">{{
                    formatRelativeDate(row.created_at)
                  }}</span>
                  <template v-if="row.expires_at">
                    ·
                    {{
                      t("settingsData.expires", {
                        when: formatRelativeDate(row.expires_at),
                      })
                    }}
                  </template>
                </small>
              </div>
              <div class="data__actions">
                <Button
                  v-if="row.state === 'done'"
                  variant="secondary"
                  size="sm"
                  :loading="exportBusy === row.id"
                  @click="downloadArchive(row)"
                >
                  {{ t("settingsData.download") }}
                </Button>
              </div>
            </li>
          </ul>
          <p v-else class="data__hint">{{ t("settingsData.noExports") }}</p>
        </section>

        <section class="data__group">
          <h3 class="data__group-title">{{ t("settingsData.groupUpload") }}</h3>
          <form class="data__form" @submit.prevent="submitUpload">
            <label class="data__field">
              <span>{{ t("settingsData.uploadKind") }}</span>
              <select v-model="upload.kind">
                <option v-for="kind in importKinds" :key="kind" :value="kind">
                  {{ t(`settingsData.kind.${kind}`) }}
                </option>
              </select>
            </label>
            <label class="data__field">
              <span>{{ t("settingsData.uploadMode") }}</span>
              <select v-model="upload.mode">
                <option value="merge">{{ t("settingsData.modeMerge") }}</option>
                <option value="overwrite">
                  {{ t("settingsData.modeOverwrite") }}
                </option>
              </select>
            </label>
            <label class="data__field">
              <span>{{ t("settingsData.uploadFile") }}</span>
              <input type="file" @change="pickFile" />
            </label>
            <label v-if="upload.kind === 'archive'" class="data__check">
              <input v-model="upload.federate" type="checkbox" />
              <span>{{ t("settingsData.federate") }}</span>
            </label>
            <p v-if="upload.federate" class="data__warn" role="note">
              {{ t("settingsData.federateWarn") }}
            </p>
            <p v-if="uploadError" class="data__error" role="alert">
              {{ uploadError }}
            </p>
            <div>
              <Button
                type="submit"
                :loading="uploading"
                :disabled="uploading || !upload.file"
              >
                {{ t("settingsData.uploadSubmit") }}
              </Button>
            </div>
          </form>
        </section>

        <section class="data__group">
          <h3 class="data__group-title">{{ t("settingsData.groupRemote") }}</h3>
          <p class="data__hint">{{ t("settingsData.remoteHint") }}</p>
          <p v-if="connNotice" class="data__notice" role="status">
            {{ connNotice }}
          </p>
          <p v-if="connError" class="data__error" role="alert">
            {{ connError }}
          </p>
          <!-- The same selection drives the consent scopes at connect
               time and the session's category filter afterwards. -->
          <fieldset class="data__cats">
            <legend>{{ t("settingsData.categoriesLabel") }}</legend>
            <label v-for="cat in apiCategories" :key="cat" class="data__check">
              <input
                type="checkbox"
                :checked="session.categories.includes(cat)"
                @change="toggleCategory(cat)"
              />
              <span>{{ t(`settingsData.cat.${cat}`) }}</span>
            </label>
          </fieldset>
          <form class="data__form" @submit.prevent="beginConnection">
            <p v-if="!session.categories.length" class="data__hint">
              {{ t("settingsData.categoriesRequired") }}
            </p>
            <div class="data__field">
              <Input
                v-model="beginForm.origin"
                :label="t('settingsData.originLabel')"
                :placeholder="t('settingsData.originPlaceholder')"
              />
            </div>
            <div>
              <Button
                type="submit"
                :loading="beginning"
                :disabled="
                  beginning ||
                  !beginForm.origin.trim() ||
                  !session.categories.length
                "
              >
                {{ t("settingsData.connect") }}
              </Button>
            </div>
          </form>
          <ul v-if="connections.length" class="data__list">
            <li v-for="conn in connections" :key="conn.id" class="data__row">
              <div class="data__main">
                <span class="data__name">{{ conn.origin }}</span>
                <small class="data__meta">
                  {{ conn.source_acct ?? conn.source_actor_uri }} ·
                  {{ t(`settingsData.connState.${conn.state}`, conn.state) }}
                </small>
              </div>
              <div class="data__actions">
                <template v-if="confirmingRevoke === conn.id">
                  <Button
                    variant="danger"
                    size="sm"
                    @click="revokeConnection(conn.id)"
                    >{{ t("settingsData.revokeConfirm") }}</Button
                  >
                  <Button
                    variant="secondary"
                    size="sm"
                    @click="confirmingRevoke = null"
                    >{{ t("common.cancel") }}</Button
                  >
                </template>
                <template v-else>
                  <Button
                    variant="secondary"
                    size="sm"
                    :loading="reauthorizing === conn.id"
                    @click="reauthorize(conn)"
                    >{{ t("settingsData.reauthorize") }}</Button
                  >
                  <Button
                    variant="secondary"
                    size="sm"
                    @click="confirmingRevoke = conn.id"
                    >{{ t("settingsData.revoke") }}</Button
                  >
                </template>
              </div>
            </li>
          </ul>

          <form
            v-if="activeConnections.length"
            class="data__form"
            @submit.prevent="submitSession"
          >
            <h4 class="data__sub-title">
              {{ t("settingsData.sessionTitle") }}
            </h4>
            <label class="data__field">
              <span>{{ t("settingsData.connectionLabel") }}</span>
              <select v-model="session.connection_id" required>
                <option value="" disabled>
                  {{ t("settingsData.connectionPick") }}
                </option>
                <option
                  v-for="conn in activeConnections"
                  :key="conn.id"
                  :value="conn.id"
                >
                  {{ conn.source_acct ?? conn.origin }}
                </option>
              </select>
            </label>
            <div class="data__grid">
              <label class="data__field">
                <span>{{ t("settingsData.limitLabel") }}</span>
                <input
                  v-model.number="session.limit"
                  type="number"
                  min="1"
                  :placeholder="t('settingsData.limitPlaceholder')"
                />
              </label>
              <label class="data__field">
                <span>{{ t("settingsData.orderLabel") }}</span>
                <select v-model="session.order">
                  <option value="newest">
                    {{ t("settingsData.orderNewest") }}
                  </option>
                  <option value="oldest">
                    {{ t("settingsData.orderOldest") }}
                  </option>
                </select>
              </label>
              <label class="data__field">
                <span>{{ t("settingsData.sinceLabel") }}</span>
                <input v-model="session.since" type="date" />
              </label>
              <label class="data__field">
                <span>{{ t("settingsData.untilLabel") }}</span>
                <input v-model="session.until" type="date" />
              </label>
            </div>
            <label class="data__check">
              <input v-model="session.federate" type="checkbox" />
              <span>{{ t("settingsData.federate") }}</span>
            </label>
            <p v-if="session.federate" class="data__warn" role="note">
              {{ t("settingsData.federateWarn") }}
            </p>
            <label class="data__check">
              <input v-model="session.overwrite_profile" type="checkbox" />
              <span>{{ t("settingsData.overwriteProfile") }}</span>
            </label>
            <p v-if="sessionError" class="data__error" role="alert">
              {{ sessionError }}
            </p>
            <div>
              <Button
                type="submit"
                :loading="creatingSession"
                :disabled="creatingSession || !session.connection_id"
              >
                {{ t("settingsData.sessionSubmit") }}
              </Button>
            </div>
          </form>
        </section>

        <section class="data__group">
          <h3 class="data__group-title">
            {{ t("settingsData.groupImports") }}
          </h3>
          <p v-if="importsError" class="data__error" role="alert">
            {{ importsError }}
          </p>
          <ul v-if="importsLoading" class="data__list">
            <li class="data__row"><Skeleton width="50%" /></li>
          </ul>
          <ul v-else-if="imports.length" class="data__list">
            <li v-for="row in imports" :key="row.id" class="data__row">
              <div class="data__main">
                <span class="data__name">
                  {{ t(`settingsData.kind.${row.kind}`, row.kind) }}
                  <template v-if="row.source_origin">
                    — {{ row.source_origin }}
                  </template>
                </span>
                <small class="data__meta">
                  {{ stateLabel(row) }}
                  <template v-if="row.total != null">
                    ·
                    {{
                      t("settingsData.progress", {
                        done: row.processed,
                        total: row.total,
                        failed: row.failed,
                      })
                    }}
                  </template>
                  ·
                  <span :title="formatAbsoluteDate(row.created_at)">{{
                    formatRelativeDate(row.created_at)
                  }}</span>
                  <template v-if="row.error"> · {{ row.error }}</template>
                </small>
                <small
                  v-if="
                    row.category_state && Object.keys(row.category_state).length
                  "
                  class="data__cats-line"
                >
                  {{ categorySummary(row) }}
                </small>
              </div>
              <div class="data__actions">
                <Button
                  v-if="['pending', 'running'].includes(row.state)"
                  variant="secondary"
                  size="sm"
                  :loading="actingOn === row.id"
                  @click="cancel(row.id)"
                  >{{ t("settingsData.cancel") }}</Button
                >
                <Button
                  v-if="['failed', 'paused', 'cancelled'].includes(row.state)"
                  variant="secondary"
                  size="sm"
                  :loading="actingOn === row.id"
                  @click="resume(row)"
                  >{{ t("settingsData.resume") }}</Button
                >
              </div>
            </li>
          </ul>
          <p v-else class="data__hint">{{ t("settingsData.noImports") }}</p>
        </section>
      </div>
    </Card>
  </div>
</template>

<style scoped>
.data {
  display: flex;
  flex-direction: column;
  gap: 1.25rem;
}
.data__group {
  display: flex;
  flex-direction: column;
  gap: 0.6rem;
}
.data__group-title,
.data__sub-title {
  margin: 0;
}
.data__hint {
  color: var(--text-secondary);
  font-size: 0.875rem;
  margin: 0;
}
.data__error {
  color: var(--danger);
  margin: 0;
}
.data__warn {
  color: var(--warning, var(--danger));
  font-size: 0.875rem;
  margin: 0;
}
.data__notice {
  color: var(--success, var(--accent));
  margin: 0;
}
.data__kinds {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
  list-style: none;
  margin: 0;
  padding: 0;
}
.data__list {
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
  list-style: none;
  margin: 0;
  padding: 0;
}
.data__row {
  align-items: center;
  display: flex;
  gap: 0.75rem;
  justify-content: space-between;
}
.data__main {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.data__meta,
.data__cats-line {
  color: var(--text-secondary);
}
.data__cats-line {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
}
.data__actions {
  display: flex;
  gap: 0.5rem;
}
.data__form {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
}
.data__grid {
  display: grid;
  gap: 0.75rem;
  grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
}
.data__field {
  display: flex;
  flex-direction: column;
  gap: 0.25rem;
}
.data__field select {
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-2) var(--space-3);
}
.data__check {
  align-items: center;
  display: flex;
  gap: 0.5rem;
}
.data__cats {
  border: 0;
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem 1rem;
  margin: 0;
  padding: 0;
}
.data__cats legend {
  color: var(--text-secondary);
  font-size: 0.875rem;
  padding: 0;
}
</style>
