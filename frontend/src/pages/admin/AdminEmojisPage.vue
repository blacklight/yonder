<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import {
  createEmojiPack,
  deleteEmoji,
  deleteEmojiPack,
  importEmojiDir,
  importEmojiZip,
  listAdminEmojis,
  listEmojiPacks,
  patchEmoji,
  renameEmojiPack,
  uploadEmoji,
  type AdminEmoji,
  type EmojiPack,
} from "@/api/adminEmoji";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import ConfirmDialog from "@/components/admin/ConfirmDialog.vue";

const { t } = useI18n();

const emojis = ref<AdminEmoji[]>([]);
const packs = ref<EmojiPack[]>([]);
const packFilter = ref("");
const loading = ref(true);
const error = ref("");
const notice = ref("");
const pending = ref(false);
const removingEmoji = ref<AdminEmoji | null>(null);
const removingPack = ref<EmojiPack | null>(null);

const upload = reactive({
  file: null as File | null,
  shortcode: "",
  category: "",
  pack_id: "",
});
const zipFile = ref<File | null>(null);
const dirImport = reactive({ from_dir: "", pack_id: "" });
const zipPackId = ref("");
const newPack = ref("");
const renaming = ref<Record<string, string>>({});

onMounted(load);

async function load() {
  loading.value = true;
  error.value = "";
  try {
    [emojis.value, packs.value] = await Promise.all([
      listAdminEmojis({
        include_disabled: true,
        pack_id: packFilter.value || undefined,
      }),
      listEmojiPacks(),
    ]);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

function onFile(e: Event, into: "upload" | "zip") {
  const input = e.target as HTMLInputElement;
  const f = input.files?.[0] ?? null;
  if (into === "upload") upload.file = f;
  else zipFile.value = f;
}

function fail(e: unknown) {
  error.value = e instanceof ApiError ? e.message : String(e);
}

async function doUpload() {
  if (!upload.file || !upload.shortcode.trim()) return;
  pending.value = true;
  error.value = "";
  try {
    const created = await uploadEmoji({
      file: upload.file,
      shortcode: upload.shortcode.trim(),
      category: upload.category.trim() || undefined,
      pack_id: upload.pack_id || undefined,
    });
    emojis.value = [created, ...emojis.value];
    upload.file = null;
    upload.shortcode = "";
    upload.category = "";
    notice.value = t("admin.emojis.uploaded");
  } catch (e) {
    fail(e);
  } finally {
    pending.value = false;
  }
}

async function doZipImport() {
  if (!zipFile.value) return;
  pending.value = true;
  error.value = "";
  try {
    const report = await importEmojiZip(
      zipFile.value,
      zipPackId.value || undefined,
    );
    notice.value = t("admin.emojis.importReport", {
      imported: report.imported,
      skipped: report.skipped,
    });
    zipFile.value = null;
    await load();
  } catch (e) {
    fail(e);
  } finally {
    pending.value = false;
  }
}

async function doDirImport() {
  if (!dirImport.from_dir.trim()) return;
  pending.value = true;
  error.value = "";
  try {
    const report = await importEmojiDir(
      dirImport.from_dir.trim(),
      dirImport.pack_id || undefined,
    );
    notice.value = t("admin.emojis.importReport", {
      imported: report.imported,
      skipped: report.skipped,
    });
    await load();
  } catch (e) {
    fail(e);
  } finally {
    pending.value = false;
  }
}

async function toggle(
  emoji: AdminEmoji,
  flag: "visible_in_picker" | "disabled",
) {
  try {
    const updated = await patchEmoji(emoji.id, { [flag]: !emoji[flag] });
    Object.assign(emoji, updated);
  } catch (e) {
    fail(e);
  }
}

async function removeEmoji() {
  const emoji = removingEmoji.value;
  removingEmoji.value = null;
  if (!emoji) return;
  try {
    await deleteEmoji(emoji.id);
    emojis.value = emojis.value.filter((e) => e.id !== emoji.id);
  } catch (e) {
    fail(e);
  }
}

async function addPack() {
  if (!newPack.value.trim()) return;
  pending.value = true;
  try {
    await createEmojiPack(newPack.value.trim());
    newPack.value = "";
    packs.value = await listEmojiPacks();
  } catch (e) {
    fail(e);
  } finally {
    pending.value = false;
  }
}

async function renamePack(pack: EmojiPack) {
  const name = renaming.value[pack.id]?.trim();
  if (!name || name === pack.name) return;
  try {
    await renameEmojiPack(pack.id, name);
    packs.value = await listEmojiPacks();
  } catch (e) {
    fail(e);
  }
}

async function removePack() {
  const pack = removingPack.value;
  removingPack.value = null;
  if (!pack) return;
  try {
    await deleteEmojiPack(pack.id);
    packs.value = packs.value.filter((p) => p.id !== pack.id);
  } catch (e) {
    fail(e);
  }
}
</script>

<template>
  <div class="emojis">
    <h2>{{ t("admin.nav.emojis") }}</h2>

    <section class="emojis__section">
      <h3 class="emojis__title">{{ t("admin.emojis.upload") }}</h3>
      <form class="emojis__row" @submit.prevent="doUpload">
        <input
          type="file"
          accept="image/*"
          :aria-label="t('admin.emojis.file')"
          @change="onFile($event, 'upload')"
        />
        <Input
          v-model="upload.shortcode"
          :label="t('admin.emojis.shortcode')"
          placeholder="party_parrot"
        />
        <Input v-model="upload.category" :label="t('admin.emojis.category')" />
        <select v-model="upload.pack_id" :aria-label="t('admin.emojis.pack')">
          <option value="">{{ t("admin.emojis.noPack") }}</option>
          <option v-for="p in packs" :key="p.id" :value="p.id">
            {{ p.name }}
          </option>
        </select>
        <Button
          type="submit"
          size="sm"
          :loading="pending"
          :disabled="!upload.file || !upload.shortcode.trim()"
        >
          {{ t("admin.emojis.uploadBtn") }}
        </Button>
      </form>
    </section>

    <section class="emojis__section">
      <h3 class="emojis__title">{{ t("admin.emojis.import") }}</h3>
      <div class="emojis__row">
        <input
          type="file"
          accept=".zip,application/zip"
          :aria-label="t('admin.emojis.zipFile')"
          @change="onFile($event, 'zip')"
        />
        <select v-model="zipPackId" :aria-label="t('admin.emojis.pack')">
          <option value="">{{ t("admin.emojis.noPack") }}</option>
          <option v-for="p in packs" :key="p.id" :value="p.id">
            {{ p.name }}
          </option>
        </select>
        <Button
          variant="secondary"
          size="sm"
          :loading="pending"
          :disabled="!zipFile"
          @click="doZipImport"
        >
          {{ t("admin.emojis.importZip") }}
        </Button>
      </div>
      <div class="emojis__row">
        <Input
          v-model="dirImport.from_dir"
          :label="t('admin.emojis.dirLabel')"
          placeholder="/srv/emoji"
        />
        <Button
          variant="secondary"
          size="sm"
          :loading="pending"
          :disabled="!dirImport.from_dir.trim()"
          @click="doDirImport"
        >
          {{ t("admin.emojis.importDir") }}
        </Button>
      </div>
      <p class="emojis__hint">{{ t("admin.emojis.dirHint") }}</p>
    </section>

    <section class="emojis__section">
      <h3 class="emojis__title">{{ t("admin.emojis.packs") }}</h3>
      <ul class="emojis__packs">
        <li v-for="p in packs" :key="p.id" class="emojis__pack">
          <input
            v-model="renaming[p.id]"
            class="emojis__pack-name"
            :placeholder="p.name"
            :aria-label="t('admin.emojis.packName')"
          />
          <span class="emojis__pack-count">
            {{ t("admin.emojis.packCount", { count: p.emoji_count ?? 0 }) }}
          </span>
          <Button variant="secondary" size="sm" @click="renamePack(p)">
            {{ t("admin.emojis.rename") }}
          </Button>
          <Button variant="danger" size="sm" @click="removingPack = p">
            {{ t("common.delete") }}
          </Button>
        </li>
      </ul>
      <div class="emojis__row">
        <Input v-model="newPack" :label="t('admin.emojis.newPack')" />
        <Button
          variant="secondary"
          size="sm"
          :loading="pending"
          :disabled="!newPack.trim()"
          @click="addPack"
        >
          {{ t("admin.emojis.createPack") }}
        </Button>
      </div>
    </section>

    <section class="emojis__section">
      <div class="emojis__row emojis__row--between">
        <h3 class="emojis__title">{{ t("admin.emojis.list") }}</h3>
        <select
          v-model="packFilter"
          :aria-label="t('admin.emojis.pack')"
          @change="load"
        >
          <option value="">{{ t("admin.emojis.allPacks") }}</option>
          <option v-for="p in packs" :key="p.id" :value="p.id">
            {{ p.name }}
          </option>
        </select>
      </div>
      <p v-if="loading" class="emojis__status">{{ t("common.loading") }}</p>
      <p v-else-if="emojis.length === 0" class="emojis__status">
        {{ t("admin.emojis.empty") }}
      </p>
      <ul v-else class="emojis__list">
        <li v-for="e in emojis" :key="e.id" class="emojis__item">
          <img
            v-if="e.url"
            :src="e.url"
            :alt="`:${e.shortcode}:`"
            class="emojis__img"
          />
          <code class="emojis__code">:{{ e.shortcode }}:</code>
          <span class="emojis__cat">{{ e.category }}</span>
          <label class="emojis__flag">
            <input
              type="checkbox"
              :checked="e.visible_in_picker"
              @change="toggle(e, 'visible_in_picker')"
            />
            {{ t("admin.emojis.visible") }}
          </label>
          <label class="emojis__flag">
            <input
              type="checkbox"
              :checked="e.disabled"
              @change="toggle(e, 'disabled')"
            />
            {{ t("admin.emojis.disabled") }}
          </label>
          <Button
            variant="danger"
            size="sm"
            :disabled="pending"
            @click="removingEmoji = e"
          >
            {{ t("common.delete") }}
          </Button>
        </li>
      </ul>
    </section>

    <p v-if="notice" class="emojis__notice">{{ notice }}</p>
    <p v-if="error" class="emojis__error" role="alert">{{ error }}</p>

    <ConfirmDialog
      v-if="removingEmoji"
      :title="t('common.delete')"
      :message="
        t('admin.emojis.confirmDelete', { code: removingEmoji.shortcode })
      "
      danger
      @confirm="removeEmoji"
      @cancel="removingEmoji = null"
    />
    <ConfirmDialog
      v-if="removingPack"
      :title="t('common.delete')"
      :message="
        t('admin.emojis.confirmDeletePack', { name: removingPack.name })
      "
      danger
      @confirm="removePack"
      @cancel="removingPack = null"
    />
  </div>
</template>

<style scoped>
.emojis__section {
  margin-bottom: var(--space-6);
}
.emojis__title {
  margin: 0 0 var(--space-2);
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-xs);
  font-weight: 700;
  letter-spacing: 0.05em;
  text-transform: uppercase;
  color: var(--text-muted);
}
.emojis__row {
  display: flex;
  align-items: end;
  flex-wrap: wrap;
  gap: var(--space-3);
  margin-bottom: var(--space-3);
}
.emojis__row--between {
  justify-content: space-between;
  align-items: center;
}
.emojis__hint,
.emojis__notice {
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.emojis__notice {
  color: var(--accent-link);
}
.emojis__packs {
  list-style: none;
  margin: 0 0 var(--space-3);
  padding: 0;
}
.emojis__pack {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-2) 0;
  border-bottom: 1px solid var(--border);
}
.emojis__pack-name {
  padding: var(--space-1) var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg);
  color: var(--text);
}
.emojis__pack-count {
  flex: 1;
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.emojis__list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.emojis__item {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: var(--space-3);
  padding: var(--space-2) 0;
  border-top: 1px solid var(--border);
}
.emojis__img {
  width: 2rem;
  height: 2rem;
  object-fit: contain;
}
.emojis__code {
  font-size: var(--font-sm, 0.8125rem);
  min-width: 8rem;
}
.emojis__cat {
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.emojis__flag {
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
  font-size: var(--text-xs);
}
.emojis__status {
  color: var(--text-muted);
}
.emojis__error {
  color: var(--danger);
}
</style>
