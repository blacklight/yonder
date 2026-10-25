<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import {
  deleteFrontend,
  installFrontend,
  installFrontendZip,
  listAdminFrontends,
  setDefaultFrontend,
  type AdminFrontends,
} from "@/api/adminFrontends";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import ConfirmDialog from "@/components/admin/ConfirmDialog.vue";
import SettingsSection from "@/components/admin/SettingsSection.vue";
import { useAdminSettingsStore } from "@/stores/adminSettings";

const { t } = useI18n();

const data = ref<AdminFrontends | null>(null);
const loading = ref(true);
const error = ref("");
const notice = ref("");
const pending = ref(false);
const removing = ref<string | null>(null);

const zip = reactive({ file: null as File | null, name: "", version: "" });
const install = reactive({
  name: "",
  source: "path" as "path" | "git",
  path: "",
  url: "",
  ref: "",
  version: "",
});

const settingsStore = useAdminSettingsStore();

onMounted(async () => {
  await load();
  // Also surface the `frontends.*` config keys (dir, max upload size).
  if (settingsStore.entries.length === 0) {
    try {
      await settingsStore.fetch();
    } catch (e) {
      error.value = e instanceof ApiError ? e.message : String(e);
    }
  }
});

async function load() {
  loading.value = true;
  error.value = "";
  try {
    data.value = await listAdminFrontends();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

function fail(e: unknown) {
  error.value = e instanceof ApiError ? e.message : String(e);
}

async function setDefault(name: string) {
  error.value = "";
  try {
    await setDefaultFrontend(name);
    if (data.value) data.value.default = name;
    notice.value = t("admin.frontends.defaultSet", { name });
  } catch (e) {
    fail(e);
  }
}

function onZip(e: Event) {
  zip.file = (e.target as HTMLInputElement).files?.[0] ?? null;
}

async function doZipInstall() {
  if (!zip.file || !zip.name.trim()) return;
  pending.value = true;
  error.value = "";
  notice.value = "";
  try {
    await installFrontendZip({
      file: zip.file,
      name: zip.name.trim(),
      version: zip.version.trim() || undefined,
    });
    notice.value = t("admin.frontends.installed", { name: zip.name });
    zip.file = null;
    zip.name = "";
    zip.version = "";
    await load();
  } catch (e) {
    fail(e);
  } finally {
    pending.value = false;
  }
}

async function doSourceInstall() {
  pending.value = true;
  error.value = "";
  notice.value = "";
  try {
    const base = {
      name: install.name.trim(),
      version: install.version.trim() || undefined,
    };
    await installFrontend(
      install.source === "path"
        ? { ...base, source: "path", path: install.path.trim() }
        : {
            ...base,
            source: "git",
            url: install.url.trim(),
            ref: install.ref.trim() || undefined,
          },
    );
    notice.value = t("admin.frontends.installed", { name: install.name });
    install.name = "";
    install.path = "";
    install.url = "";
    install.ref = "";
    install.version = "";
    await load();
  } catch (e) {
    fail(e);
  } finally {
    pending.value = false;
  }
}

const installDisabled = () =>
  !install.name.trim() ||
  (install.source === "path" ? !install.path.trim() : !install.url.trim());

async function remove() {
  const name = removing.value;
  removing.value = null;
  if (!name) return;
  pending.value = true;
  error.value = "";
  try {
    await deleteFrontend(name);
    if (data.value) {
      data.value.frontends = data.value.frontends.filter(
        (f) => f.name !== name,
      );
    }
  } catch (e) {
    fail(e);
  } finally {
    pending.value = false;
  }
}
</script>

<template>
  <div class="frontends">
    <h2>{{ t("admin.nav.frontends") }}</h2>
    <p v-if="loading" class="frontends__status">{{ t("common.loading") }}</p>
    <template v-else-if="data">
      <ul class="frontends__list">
        <li v-for="f in data.frontends" :key="f.name" class="frontends__item">
          <code class="frontends__name">{{ f.name }}</code>
          <span v-if="f.version" class="frontends__meta">v{{ f.version }}</span>
          <span v-if="f.source" class="frontends__meta">{{ f.source }}</span>
          <span v-if="f.name === data.default" class="frontends__default">
            {{ t("admin.frontends.isDefault") }}
          </span>
          <span class="frontends__buttons">
            <Button
              v-if="f.name !== data.default"
              variant="secondary"
              size="sm"
              :disabled="pending"
              @click="setDefault(f.name)"
            >
              {{ t("admin.frontends.makeDefault") }}
            </Button>
            <Button
              v-if="!f.builtin && f.name !== data.default"
              variant="danger"
              size="sm"
              :disabled="pending"
              @click="removing = f.name"
            >
              {{ t("common.delete") }}
            </Button>
          </span>
        </li>
      </ul>
    </template>

    <section class="frontends__section">
      <h3 class="frontends__title">{{ t("admin.frontends.installZip") }}</h3>
      <form class="frontends__row" @submit.prevent="doZipInstall">
        <input
          type="file"
          accept=".zip,application/zip"
          :aria-label="t('admin.frontends.zipFile')"
          @change="onZip"
        />
        <Input v-model="zip.name" :label="t('admin.frontends.name')" />
        <Input v-model="zip.version" :label="t('admin.frontends.version')" />
        <Button
          type="submit"
          size="sm"
          :loading="pending"
          :disabled="!zip.file || !zip.name.trim()"
        >
          {{ t("admin.frontends.installBtn") }}
        </Button>
      </form>
    </section>

    <section class="frontends__section">
      <h3 class="frontends__title">{{ t("admin.frontends.installSource") }}</h3>
      <div class="frontends__row">
        <label class="frontends__field">
          <span>{{ t("admin.frontends.source") }}</span>
          <select v-model="install.source">
            <option value="path">{{ t("admin.frontends.sourcePath") }}</option>
            <option value="git">{{ t("admin.frontends.sourceGit") }}</option>
          </select>
        </label>
        <Input v-model="install.name" :label="t('admin.frontends.name')" />
        <Input
          v-model="install.version"
          :label="t('admin.frontends.version')"
        />
      </div>
      <div class="frontends__row">
        <Input
          v-if="install.source === 'path'"
          v-model="install.path"
          :label="t('admin.frontends.pathLabel')"
          placeholder="/srv/frontends/mine"
        />
        <template v-else>
          <Input
            v-model="install.url"
            :label="t('admin.frontends.gitUrl')"
            placeholder="https://git.example/frontend.git"
          />
          <Input
            v-model="install.ref"
            :label="t('admin.frontends.gitRef')"
            placeholder="main"
          />
        </template>
        <Button
          variant="secondary"
          size="sm"
          :loading="pending"
          :disabled="installDisabled()"
          @click="doSourceInstall"
        >
          {{ t("admin.frontends.installBtn") }}
        </Button>
      </div>
      <p class="frontends__hint">{{ t("admin.frontends.installHint") }}</p>
    </section>

    <SettingsSection
      v-if="settingsStore.bySection('frontends').length"
      section-id="frontends"
      :title="t('admin.sections.frontends')"
      :entries="settingsStore.bySection('frontends')"
    />

    <p v-if="notice" class="frontends__notice">{{ notice }}</p>
    <p v-if="error" class="frontends__error" role="alert">{{ error }}</p>

    <ConfirmDialog
      v-if="removing"
      :title="t('common.delete')"
      :message="t('admin.frontends.confirmRemove', { name: removing })"
      danger
      :pending="pending"
      @confirm="remove"
      @cancel="removing = null"
    />
  </div>
</template>

<style scoped>
.frontends__list {
  list-style: none;
  margin: 0 0 var(--space-6);
  padding: 0;
}
.frontends__item {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: var(--space-3);
  padding: var(--space-3) 0;
  border-top: 1px solid var(--border);
}
.frontends__name {
  font-weight: 600;
}
.frontends__meta {
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.frontends__default {
  font-size: var(--text-xs);
  padding: 0 var(--space-2);
  border-radius: var(--radius-sm);
  background: var(--accent-soft, var(--bg-raised));
  color: var(--accent-link);
}
.frontends__buttons {
  margin-left: auto;
  display: inline-flex;
  gap: var(--space-2);
}
.frontends__section {
  margin-bottom: var(--space-6);
}
.frontends__title {
  margin: 0 0 var(--space-2);
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-xs);
  font-weight: 700;
  letter-spacing: 0.05em;
  text-transform: uppercase;
  color: var(--text-muted);
}
.frontends__row {
  display: flex;
  align-items: end;
  flex-wrap: wrap;
  gap: var(--space-3);
  margin-bottom: var(--space-3);
}
.frontends__field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  font-size: var(--font-sm, 0.8125rem);
}
.frontends__field select {
  padding: var(--space-1) var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg);
  color: var(--text);
  font: inherit;
}
.frontends__hint {
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.frontends__notice {
  font-size: var(--text-xs);
  color: var(--accent-link);
}
.frontends__status {
  color: var(--text-muted);
}
.frontends__error {
  color: var(--danger);
}
</style>
