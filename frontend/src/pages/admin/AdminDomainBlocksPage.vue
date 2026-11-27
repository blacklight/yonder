<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import {
  deleteDomainBlock,
  exportDomainBlocksCsv,
  importDomainBlocksCsv,
  listDomainBlocks,
  patchDomainBlock,
  upsertDomainBlock,
  type AdminDomainBlock,
  type DomainBlockImportError,
} from "@/api/adminDomainBlocks";
import {
  fetchRemoteDomainBlocks,
  remoteBlocksCsv,
  RemoteDomainBlocksError,
  type RemoteDomainBlock,
  type RemoteSeverityMode,
} from "@/api/remoteDomainBlocks";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import Spinner from "@/components/ui/Spinner.vue";
import ConfirmDialog from "@/components/admin/ConfirmDialog.vue";
import Modal from "@/components/ui/Modal.vue";
import { saveBlob } from "@/composables/useExports";

const { t } = useI18n();

const blocks = ref<AdminDomainBlock[]>([]);
const loading = ref(true);
const error = ref("");
const pending = ref(false);
const removing = ref<AdminDomainBlock | null>(null);
const exporting = ref(false);
const importing = ref(false);
const overwriteExisting = ref(false);
const importSummary = ref("");
const importErrors = ref<DomainBlockImportError[]>([]);
const fileInput = ref<HTMLInputElement | null>(null);

interface RemoteImportRow extends RemoteDomainBlock {
  selected: boolean;
}

const remoteOpen = ref(false);
const remoteInstance = ref("");
const remoteRows = ref<RemoteImportRow[]>([]);
const remoteSource = ref<"domain_blocks" | "instance" | null>(null);
const remoteOrigin = ref("");
const remoteFetched = ref(false);
const remoteFetching = ref(false);
const remoteImporting = ref(false);
const remoteError = ref("");
const remoteSeverityMode = ref<RemoteSeverityMode>("source");
const remoteComment = ref("");

const remoteSelectedCount = computed(
  () => remoteRows.value.filter((row) => row.selected).length,
);
const allRemoteSelected = computed(
  () =>
    remoteRows.value.length > 0 &&
    remoteRows.value.every((row) => row.selected),
);

const blank = () => ({
  domain: "",
  severity: "silence",
  reject_media: false,
  reject_reports: false,
  public_comment: "",
  private_comment: "",
});
const form = reactive(blank());

onMounted(load);

async function load() {
  loading.value = true;
  error.value = "";
  try {
    blocks.value = await listDomainBlocks();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

async function add() {
  pending.value = true;
  error.value = "";
  try {
    const created = await upsertDomainBlock({ ...form });
    blocks.value = [
      created,
      ...blocks.value.filter((b) => b.id !== created.id),
    ];
    Object.assign(form, blank());
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

async function setSeverity(block: AdminDomainBlock, severity: string) {
  try {
    const updated = await patchDomainBlock(block.id, { severity });
    Object.assign(block, updated);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  }
}

async function toggle(
  block: AdminDomainBlock,
  flag: "reject_media" | "reject_reports" | "obfuscate",
) {
  try {
    const updated = await patchDomainBlock(block.id, {
      [flag]: !block[flag],
    });
    Object.assign(block, updated);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  }
}

async function exportCsv() {
  exporting.value = true;
  error.value = "";
  try {
    const { blob, filename } = await exportDomainBlocksCsv();
    saveBlob(blob, filename || "domain_blocks.csv");
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    exporting.value = false;
  }
}

function pickImportFile() {
  fileInput.value?.click();
}

function openRemoteImport() {
  remoteOpen.value = true;
  remoteError.value = "";
}

function closeRemoteImport() {
  if (!remoteImporting.value) remoteOpen.value = false;
}

function setAllRemote(selected: boolean) {
  for (const row of remoteRows.value) row.selected = selected;
}

function remoteHost(): string {
  try {
    return new URL(remoteOrigin.value).hostname;
  } catch {
    return remoteInstance.value.trim();
  }
}

async function fetchRemote() {
  remoteFetching.value = true;
  remoteError.value = "";
  remoteFetched.value = false;
  remoteRows.value = [];
  remoteSource.value = null;
  try {
    const result = await fetchRemoteDomainBlocks(remoteInstance.value);
    remoteRows.value = result.blocks.map((block) => ({
      ...block,
      selected: true,
    }));
    remoteSource.value = result.source;
    remoteOrigin.value = result.origin;
    remoteFetched.value = true;
  } catch (e) {
    remoteError.value =
      e instanceof RemoteDomainBlocksError && e.code === "invalid_instance"
        ? t("admin.domainBlocks.remoteInvalidInstance")
        : t("admin.domainBlocks.remoteFetchError");
  } finally {
    remoteFetching.value = false;
  }
}

async function importRemoteSelected() {
  const selected = remoteRows.value.filter((row) => row.selected);
  if (selected.length === 0) return;
  remoteImporting.value = true;
  remoteError.value = "";
  importSummary.value = "";
  importErrors.value = [];
  try {
    const host = remoteHost();
    const csv = remoteBlocksCsv(selected, {
      severityMode: remoteSeverityMode.value,
      comment: remoteComment.value,
      sourceHost: host,
    });
    const file = new File([csv], `${host || "remote"}-domain-blocks.csv`, {
      type: "text/csv",
    });
    const result = await importDomainBlocksCsv(file, overwriteExisting.value);
    importSummary.value = t("admin.domainBlocks.importResult", {
      created: result.created,
      updated: result.updated,
      existing: result.skipped_existing,
      invalid: result.skipped_invalid,
    });
    importErrors.value = result.errors;
    remoteOpen.value = false;
    if (result.created > 0 || result.updated > 0) await load();
  } catch (e) {
    remoteError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    remoteImporting.value = false;
  }
}

async function importCsv(event: Event) {
  const input = event.target as HTMLInputElement;
  const file = input.files?.[0];
  input.value = "";
  if (!file) return;
  importing.value = true;
  error.value = "";
  importSummary.value = "";
  importErrors.value = [];
  try {
    const result = await importDomainBlocksCsv(file, overwriteExisting.value);
    importSummary.value = t("admin.domainBlocks.importResult", {
      created: result.created,
      updated: result.updated,
      existing: result.skipped_existing,
      invalid: result.skipped_invalid,
    });
    importErrors.value = result.errors;
    if (result.created > 0 || result.updated > 0) await load();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    importing.value = false;
  }
}

async function remove() {
  const block = removing.value;
  removing.value = null;
  if (!block) return;
  pending.value = true;
  error.value = "";
  try {
    await deleteDomainBlock(block.id);
    blocks.value = blocks.value.filter((b) => b.id !== block.id);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}
</script>

<template>
  <div class="blocks">
    <h2>{{ t("admin.nav.domainBlocks") }}</h2>
    <p class="blocks__hint">{{ t("admin.domainBlocks.hint") }}</p>

    <section class="blocks__transfer" aria-labelledby="domain-blocks-csv">
      <div>
        <h3 id="domain-blocks-csv">{{ t("admin.domainBlocks.csvTitle") }}</h3>
        <p class="blocks__hint">{{ t("admin.domainBlocks.csvHint") }}</p>
      </div>
      <div class="blocks__transfer-actions">
        <label class="blocks__overwrite">
          <input v-model="overwriteExisting" type="checkbox" />
          {{ t("admin.domainBlocks.overwriteExisting") }}
        </label>
        <Button variant="secondary" :loading="exporting" @click="exportCsv">
          {{ t("admin.domainBlocks.exportCsv") }}
        </Button>
        <Button
          variant="secondary"
          :loading="importing"
          @click="pickImportFile"
        >
          {{ t("admin.domainBlocks.importCsv") }}
        </Button>
        <Button variant="secondary" @click="openRemoteImport">
          {{ t("admin.domainBlocks.remoteImport") }}
        </Button>
        <input
          ref="fileInput"
          class="blocks__file-input"
          type="file"
          accept=".csv,.txt,text/csv,text/plain"
          @change="importCsv"
        />
      </div>
      <p v-if="importSummary" class="blocks__import-summary" role="status">
        {{ importSummary }}
      </p>
      <ul v-if="importErrors.length" class="blocks__import-errors">
        <li
          v-for="issue in importErrors"
          :key="`${issue.line}:${issue.domain}`"
        >
          {{
            t("admin.domainBlocks.importErrorLine", {
              line: issue.line,
              domain: issue.domain || "—",
              error: issue.error,
            })
          }}
        </li>
      </ul>
    </section>

    <form class="blocks__form" @submit.prevent="add">
      <div class="blocks__form-row">
        <Input
          v-model="form.domain"
          :label="t('admin.domainBlocks.domain')"
          placeholder="example.tld"
        />
        <label class="blocks__field">
          <span>{{ t("admin.domainBlocks.severityLabel") }}</span>
          <select v-model="form.severity">
            <option value="noop">
              {{ t("admin.domainBlocks.severities.noop") }}
            </option>
            <option value="silence">
              {{ t("admin.domainBlocks.severities.silence") }}
            </option>
            <option value="suspend">
              {{ t("admin.domainBlocks.severities.suspend") }}
            </option>
          </select>
        </label>
      </div>
      <div class="blocks__form-row blocks__checks">
        <label
          ><input v-model="form.reject_media" type="checkbox" />
          {{ t("admin.domainBlocks.rejectMedia") }}</label
        >
        <label
          ><input v-model="form.reject_reports" type="checkbox" />
          {{ t("admin.domainBlocks.rejectReports") }}</label
        >
      </div>
      <Input
        v-model="form.public_comment"
        :label="t('admin.domainBlocks.publicComment')"
      />
      <Input
        v-model="form.private_comment"
        :label="t('admin.domainBlocks.privateComment')"
      />
      <Button type="submit" :loading="pending" :disabled="!form.domain.trim()">
        {{ t("admin.domainBlocks.add") }}
      </Button>
    </form>

    <p v-if="loading" class="blocks__status"><Spinner /></p>
    <p v-else-if="blocks.length === 0" class="blocks__status">
      {{ t("admin.domainBlocks.empty") }}
    </p>
    <ul v-else class="blocks__list">
      <li v-for="b in blocks" :key="b.id" class="blocks__item">
        <div class="blocks__head">
          <code class="blocks__domain">{{ b.domain }}</code>
          <select
            :value="b.severity"
            :aria-label="t('admin.domainBlocks.severityLabel')"
            @change="setSeverity(b, ($event.target as HTMLSelectElement).value)"
          >
            <option value="noop">
              {{ t("admin.domainBlocks.severities.noop") }}
            </option>
            <option value="silence">
              {{ t("admin.domainBlocks.severities.silence") }}
            </option>
            <option value="suspend">
              {{ t("admin.domainBlocks.severities.suspend") }}
            </option>
          </select>
          <Button
            variant="danger"
            size="sm"
            :disabled="pending"
            @click="removing = b"
          >
            {{ t("admin.domainBlocks.remove") }}
          </Button>
        </div>
        <div class="blocks__flags">
          <label
            ><input
              :checked="b.reject_media"
              type="checkbox"
              @change="toggle(b, 'reject_media')"
            />
            {{ t("admin.domainBlocks.rejectMedia") }}</label
          >
          <label
            ><input
              :checked="b.reject_reports"
              type="checkbox"
              @change="toggle(b, 'reject_reports')"
            />
            {{ t("admin.domainBlocks.rejectReports") }}</label
          >
          <label
            ><input
              :checked="b.obfuscate"
              type="checkbox"
              @change="toggle(b, 'obfuscate')"
            />
            {{ t("admin.domainBlocks.obfuscate") }}</label
          >
        </div>
        <p v-if="b.public_comment" class="blocks__comment">
          {{ b.public_comment }}
        </p>
      </li>
    </ul>
    <p v-if="error" class="blocks__error" role="alert">{{ error }}</p>

    <Modal
      v-if="remoteOpen"
      :title="t('admin.domainBlocks.remoteTitle')"
      @close="closeRemoteImport"
    >
      <form class="remote__fetch" @submit.prevent="fetchRemote">
        <Input
          v-model="remoteInstance"
          :label="t('admin.domainBlocks.remoteInstance')"
          :placeholder="t('admin.domainBlocks.remoteInstancePlaceholder')"
          :disabled="remoteFetching || remoteImporting"
          inputmode="url"
        />
        <Button type="submit" :loading="remoteFetching">
          {{ t("admin.domainBlocks.fetchRemote") }}
        </Button>
      </form>
      <p v-if="remoteError" class="remote__error" role="alert">
        {{ remoteError }}
      </p>
      <template v-if="remoteRows.length">
        <p class="remote__source">
          {{
            t(
              remoteSource === "domain_blocks"
                ? "admin.domainBlocks.remoteSourceDomainBlocks"
                : "admin.domainBlocks.remoteSourceInstance",
            )
          }}
        </p>
        <div class="remote__selection">
          <label>
            <input
              :checked="allRemoteSelected"
              type="checkbox"
              @change="
                setAllRemote(($event.target as HTMLInputElement).checked)
              "
            />
            {{ t("admin.domainBlocks.selectAll") }}
          </label>
          <Button variant="secondary" size="sm" @click="setAllRemote(false)">
            {{ t("admin.domainBlocks.deselectAll") }}
          </Button>
          <span class="remote__count">
            {{
              t("admin.domainBlocks.selectedCount", {
                selected: remoteSelectedCount,
                total: remoteRows.length,
              })
            }}
          </span>
        </div>
        <div class="remote__rows">
          <div v-for="row in remoteRows" :key="row.domain" class="remote__row">
            <label class="remote__domain">
              <input v-model="row.selected" type="checkbox" />
              <code>{{ row.domain }}</code>
            </label>
            <span class="remote__severity">
              {{ t(`admin.domainBlocks.severities.${row.severity}`) }}
            </span>
            <p v-if="row.comment" class="remote__comment">{{ row.comment }}</p>
          </div>
        </div>
        <label class="blocks__field remote__field">
          <span>{{ t("admin.domainBlocks.remoteSeverityMode") }}</span>
          <select v-model="remoteSeverityMode">
            <option value="source">
              {{ t("admin.domainBlocks.remoteSeveritySource") }}
            </option>
            <option value="noop">
              {{ t("admin.domainBlocks.severities.noop") }}
            </option>
            <option value="silence">
              {{ t("admin.domainBlocks.severities.silence") }}
            </option>
            <option value="suspend">
              {{ t("admin.domainBlocks.severities.suspend") }}
            </option>
          </select>
        </label>
        <Input
          v-model="remoteComment"
          :label="t('admin.domainBlocks.remoteComment')"
        />
        <p class="blocks__hint">
          {{ t("admin.domainBlocks.remoteCommentHint") }}
        </p>
        <label class="blocks__overwrite remote__overwrite">
          <input v-model="overwriteExisting" type="checkbox" />
          {{ t("admin.domainBlocks.overwriteExisting") }}
        </label>
      </template>
      <p v-else-if="remoteFetched && !remoteFetching" class="remote__empty">
        {{ t("admin.domainBlocks.remoteNoBlocks") }}
      </p>
      <template #footer>
        <Button variant="secondary" @click="closeRemoteImport">
          {{ t("ui.cancel") }}
        </Button>
        <Button
          :loading="remoteImporting"
          :disabled="remoteSelectedCount === 0"
          @click="importRemoteSelected"
        >
          {{
            t("admin.domainBlocks.importSelected", {
              count: remoteSelectedCount,
            })
          }}
        </Button>
      </template>
    </Modal>

    <ConfirmDialog
      v-if="removing"
      :title="t('admin.domainBlocks.remove')"
      :message="
        t('admin.domainBlocks.confirmRemove', { domain: removing.domain })
      "
      danger
      :pending="pending"
      @confirm="remove"
      @cancel="removing = null"
    />
  </div>
</template>

<style scoped>
.blocks__hint {
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.blocks__transfer {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  max-width: 42rem;
  margin-bottom: var(--space-4);
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
}
.blocks__transfer h3 {
  margin: 0 0 var(--space-1);
  font-size: var(--text-base);
}
.blocks__transfer-actions {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: var(--space-3);
}
.blocks__overwrite {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--font-sm, 0.8125rem);
}
.blocks__file-input {
  display: none;
}
.blocks__import-errors {
  margin: 0;
  padding-inline-start: var(--space-5);
  font-size: var(--text-xs);
  color: var(--danger);
}
.blocks__import-summary {
  margin: 0;
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.blocks__form {
  max-width: 32rem;
  margin-bottom: var(--space-4);
}
.blocks__form-row {
  display: flex;
  gap: var(--space-3);
  align-items: end;
}
.blocks__form-row > * {
  flex: 1;
}
.blocks__checks {
  margin: var(--space-2) 0;
}
.blocks__checks label,
.blocks__flags label {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--font-sm, 0.8125rem);
}
.blocks__field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  font-size: var(--font-sm, 0.8125rem);
}
.blocks__field select,
.blocks__head select {
  padding: var(--space-1) var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg);
  color: var(--text);
  font: inherit;
}
.blocks__list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.blocks__item {
  padding: var(--space-3) 0;
  border-top: 1px solid var(--border);
}
.blocks__head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.blocks__domain {
  flex: 1;
  min-width: 0;
  font-weight: 600;
}
.blocks__flags {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-4);
  margin-top: var(--space-2);
}
.blocks__comment {
  margin: var(--space-1) 0 0;
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.remote__fetch {
  display: grid;
  grid-template-columns: 1fr auto;
  align-items: end;
  gap: var(--space-3);
}
.remote__source,
.remote__count,
.remote__empty {
  color: var(--text-muted);
  font-size: var(--text-xs);
}
.remote__selection {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: var(--space-3);
  margin: var(--space-3) 0 var(--space-2);
}
.remote__selection label,
.remote__domain {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
}
.remote__rows {
  max-height: 16rem;
  overflow: auto;
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  margin-bottom: var(--space-3);
}
.remote__row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  gap: var(--space-1) var(--space-3);
  padding: var(--space-2) var(--space-3);
  border-top: 1px solid var(--border);
}
.remote__row:first-child {
  border-top: 0;
}
.remote__domain {
  min-width: 0;
}
.remote__domain code {
  overflow-wrap: anywhere;
}
.remote__severity {
  align-self: center;
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.remote__comment {
  grid-column: 1 / -1;
  margin: 0 0 0 calc(1rem + var(--space-2));
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.remote__field {
  margin-bottom: var(--space-3);
}
.remote__overwrite {
  display: flex;
  margin-top: var(--space-2);
}
.remote__error {
  color: var(--danger);
}
.blocks__status {
  color: var(--text-muted);
}
.blocks__error {
  color: var(--danger);
}
</style>
