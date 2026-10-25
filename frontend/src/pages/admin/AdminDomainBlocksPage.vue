<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import {
  deleteDomainBlock,
  listDomainBlocks,
  patchDomainBlock,
  upsertDomainBlock,
  type AdminDomainBlock,
} from "@/api/adminDomainBlocks";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import ConfirmDialog from "@/components/admin/ConfirmDialog.vue";

const { t } = useI18n();

const blocks = ref<AdminDomainBlock[]>([]);
const loading = ref(true);
const error = ref("");
const pending = ref(false);
const removing = ref<AdminDomainBlock | null>(null);

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

    <p v-if="loading" class="blocks__status">{{ t("common.loading") }}</p>
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
.blocks__status {
  color: var(--text-muted);
}
.blocks__error {
  color: var(--danger);
}
</style>
