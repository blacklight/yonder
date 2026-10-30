<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { updateCredentials, verifyCredentials } from "@/api/accounts";
import { safeHttpUrl } from "@/utils/sanitize";
import { useAuthStore } from "@/stores/auth";
import Avatar from "@/components/ui/Avatar.vue";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";
import Skeleton from "@/components/ui/Skeleton.vue";
import SettingsNav from "@/components/settings/SettingsNav.vue";
import { FontAwesomeIcon } from "@/icons";

const { t } = useI18n();
const auth = useAuthStore();

// Backend defaults (`content.max_*`) — the instance endpoint doesn't
// expose them, so the PATCH 422 stays authoritative.
const MAX_DISPLAY_NAME = 100;
const MAX_BIO = 5000;
const MAX_FIELDS = 10;

const state = ref<"loading" | "ok" | "error">("loading");
const pending = ref(false);
const saved = ref(false);
const error = ref("");

const displayName = ref("");
const note = ref("");
const fields = ref<{ name: string; value: string }[]>([]);
const locked = ref(false);
const bot = ref(false);
const discoverable = ref(true);
const hideCollections = ref(false);
const indexable = ref(true);

const acct = ref("");
const avatarFile = ref<File | null>(null);
const headerFile = ref<File | null>(null);
const avatarClear = ref(false);
const headerClear = ref(false);
const avatarInput = ref<HTMLInputElement | null>(null);
const headerInput = ref<HTMLInputElement | null>(null);
const currentAvatar = ref<string | undefined>();
const currentHeader = ref<string | null>(null);
const avatarObjectUrl = ref<string | null>(null);
const headerObjectUrl = ref<string | null>(null);

const avatarPreview = computed(() =>
  avatarClear.value
    ? undefined
    : (avatarObjectUrl.value ?? currentAvatar.value),
);
const headerPreview = computed(() =>
  headerClear.value ? null : (headerObjectUrl.value ?? currentHeader.value),
);

function pick(kind: "avatar" | "header") {
  (kind === "avatar" ? avatarInput : headerInput).value?.click();
}

// Object URLs double as the preview; revoke the old one on each pick.
function onFile(kind: "avatar" | "header", e: Event) {
  const input = e.target as HTMLInputElement;
  const file = input.files?.[0] ?? null;
  const url = file ? URL.createObjectURL(file) : null;
  if (kind === "avatar") {
    if (avatarObjectUrl.value) URL.revokeObjectURL(avatarObjectUrl.value);
    avatarObjectUrl.value = url;
    avatarFile.value = file;
    if (file) avatarClear.value = false;
  } else {
    if (headerObjectUrl.value) URL.revokeObjectURL(headerObjectUrl.value);
    headerObjectUrl.value = url;
    headerFile.value = file;
    if (file) headerClear.value = false;
  }
  // Reset so picking the same file twice still fires `change`.
  input.value = "";
}

function clearImage(kind: "avatar" | "header") {
  if (kind === "avatar") {
    avatarClear.value = true;
    avatarFile.value = null;
    if (avatarObjectUrl.value) URL.revokeObjectURL(avatarObjectUrl.value);
    avatarObjectUrl.value = null;
  } else {
    headerClear.value = true;
    headerFile.value = null;
    if (headerObjectUrl.value) URL.revokeObjectURL(headerObjectUrl.value);
    headerObjectUrl.value = null;
  }
}

function addField() {
  if (fields.value.length < MAX_FIELDS)
    fields.value.push({ name: "", value: "" });
}

function removeField(index: number) {
  fields.value.splice(index, 1);
}

async function save() {
  pending.value = true;
  error.value = "";
  saved.value = false;
  try {
    // Multipart unconditionally — it's the only transport that
    // carries files, and the text parts ride along fine.
    const form = new FormData();
    form.set("display_name", displayName.value);
    form.set("note", note.value);
    const flags: Record<string, boolean> = {
      locked: locked.value,
      bot: bot.value,
      discoverable: discoverable.value,
      hide_collections: hideCollections.value,
      indexable: indexable.value,
    };
    for (const [key, value] of Object.entries(flags))
      form.set(key, String(value));
    // An empty list still needs one blank `fields_attributes` pair —
    // absent keys leave the stored fields untouched instead of
    // clearing them.
    const rows = fields.value.length ? fields.value : [{ name: "", value: "" }];
    rows.forEach((f, i) => {
      form.set(`fields_attributes[${i}][name]`, f.name);
      form.set(`fields_attributes[${i}][value]`, f.value);
    });
    if (avatarFile.value) form.set("avatar", avatarFile.value);
    else if (avatarClear.value) form.set("avatar", "");
    if (headerFile.value) form.set("header", headerFile.value);
    else if (headerClear.value) form.set("header", "");

    const updated = await updateCredentials(form);
    auth.account = updated;
    currentAvatar.value = safeHttpUrl(updated.avatar) ?? undefined;
    currentHeader.value = safeHttpUrl(updated.header);
    avatarFile.value = null;
    headerFile.value = null;
    avatarClear.value = false;
    headerClear.value = false;
    if (avatarObjectUrl.value) URL.revokeObjectURL(avatarObjectUrl.value);
    if (headerObjectUrl.value) URL.revokeObjectURL(headerObjectUrl.value);
    avatarObjectUrl.value = null;
    headerObjectUrl.value = null;
    saved.value = true;
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

onMounted(async () => {
  try {
    // Always re-fetch — `source` (raw note/fields) only exists on the
    // CredentialAccount, not on the public entity in `auth.account`.
    const account = await verifyCredentials();
    acct.value = account.acct;
    displayName.value = account.display_name;
    note.value = account.source?.note ?? "";
    fields.value = (account.source?.fields ?? []).map((f) => ({
      name: f.name,
      value: f.value,
    }));
    locked.value = account.locked;
    bot.value = account.bot;
    discoverable.value = account.discoverable ?? true;
    hideCollections.value = account.hide_collections ?? false;
    indexable.value = account.indexable ?? true;
    currentAvatar.value = safeHttpUrl(account.avatar) ?? undefined;
    currentHeader.value = safeHttpUrl(account.header);
    state.value = "ok";
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
    state.value = "error";
  }
});

onUnmounted(() => {
  if (avatarObjectUrl.value) URL.revokeObjectURL(avatarObjectUrl.value);
  if (headerObjectUrl.value) URL.revokeObjectURL(headerObjectUrl.value);
});
</script>

<template>
  <div>
    <SettingsNav />
    <Card>
      <template #header>{{ t("settingsProfile.title") }}</template>
      <p v-if="state === 'error'" class="profile-edit__error" role="alert">
        {{ error || t("settingsProfile.loadError") }}
      </p>
      <div
        v-else-if="state === 'loading'"
        class="profile-edit-skel"
        role="status"
        :aria-label="t('common.loading')"
      >
        <Skeleton circle width="96px" />
        <Skeleton v-for="n in 4" :key="n" height="2.25rem" />
      </div>
      <form
        v-else-if="state === 'ok'"
        class="profile-edit"
        @submit.prevent="save"
      >
        <section class="profile-edit__group">
          <h3 class="profile-edit__group-title">
            {{ t("settingsProfile.groupIdentity") }}
          </h3>
          <div class="profile-edit__row">
            <span>{{ t("settingsProfile.avatar") }}</span>
            <div class="profile-edit__media">
              <Avatar
                :src="avatarPreview"
                :name="displayName || acct"
                size="lg"
              />
              <div class="profile-edit__media-actions">
                <Button variant="secondary" size="sm" @click="pick('avatar')">{{
                  t("settingsProfile.change")
                }}</Button>
                <Button
                  v-if="avatarPreview"
                  variant="secondary"
                  size="sm"
                  @click="clearImage('avatar')"
                  >{{ t("settingsProfile.remove") }}</Button
                >
              </div>
              <input
                ref="avatarInput"
                type="file"
                accept="image/*"
                class="profile-edit__file"
                :aria-label="t('settingsProfile.avatar')"
                @change="onFile('avatar', $event)"
              />
            </div>
          </div>
          <div class="profile-edit__row">
            <span>{{ t("settingsProfile.header") }}</span>
            <div class="profile-edit__media">
              <img
                v-if="headerPreview"
                :src="headerPreview"
                class="profile-edit__header"
                alt=""
              />
              <div v-else class="profile-edit__header-placeholder" />
              <div class="profile-edit__media-actions">
                <Button variant="secondary" size="sm" @click="pick('header')">{{
                  t("settingsProfile.change")
                }}</Button>
                <Button
                  v-if="headerPreview"
                  variant="secondary"
                  size="sm"
                  @click="clearImage('header')"
                  >{{ t("settingsProfile.remove") }}</Button
                >
              </div>
              <input
                ref="headerInput"
                type="file"
                accept="image/*"
                class="profile-edit__file"
                :aria-label="t('settingsProfile.header')"
                @change="onFile('header', $event)"
              />
            </div>
          </div>
          <div class="profile-edit__row profile-edit__row--top">
            <span>{{ t("settingsProfile.displayName") }}</span>
            <Input
              v-model="displayName"
              :label="t('settingsProfile.displayName')"
              class="profile-edit__sr-label"
              :maxlength="MAX_DISPLAY_NAME"
            />
          </div>
          <div class="profile-edit__row profile-edit__row--top">
            <span>{{ t("settingsProfile.bio") }}</span>
            <textarea
              v-model="note"
              class="profile-edit__bio"
              rows="5"
              :maxlength="MAX_BIO"
              :aria-label="t('settingsProfile.bio')"
            />
          </div>
        </section>

        <section class="profile-edit__group">
          <h3 class="profile-edit__group-title">
            {{ t("settingsProfile.fields") }}
          </h3>
          <div
            v-for="(field, i) in fields"
            :key="i"
            class="profile-edit__field-row"
          >
            <Input
              v-model="field.name"
              :label="t('settingsProfile.fieldName')"
              :disabled="pending"
            />
            <Input
              v-model="field.value"
              :label="t('settingsProfile.fieldValue')"
              :disabled="pending"
            />
            <Button
              variant="secondary"
              size="sm"
              class="profile-edit__field-remove"
              :aria-label="t('settingsProfile.removeField')"
              @click="removeField(i)"
            >
              <FontAwesomeIcon icon="trash-can" aria-hidden="true" />
            </Button>
          </div>
          <Button
            v-if="fields.length < MAX_FIELDS"
            variant="secondary"
            size="sm"
            @click="addField"
          >
            <FontAwesomeIcon icon="plus" aria-hidden="true" />
            {{ t("settingsProfile.addField") }}
          </Button>
        </section>

        <section class="profile-edit__group">
          <h3 class="profile-edit__group-title">
            {{ t("settingsProfile.groupFlags") }}
          </h3>
          <label class="profile-edit__flag">
            <input v-model="locked" type="checkbox" :disabled="pending" />
            <span>
              {{ t("settingsProfile.locked") }}
              <small>{{ t("settingsProfile.lockedHint") }}</small>
            </span>
          </label>
          <label class="profile-edit__flag">
            <input v-model="bot" type="checkbox" :disabled="pending" />
            <span>
              {{ t("settingsProfile.bot") }}
              <small>{{ t("settingsProfile.botHint") }}</small>
            </span>
          </label>
          <label class="profile-edit__flag">
            <input v-model="discoverable" type="checkbox" :disabled="pending" />
            <span>
              {{ t("settingsProfile.discoverable") }}
              <small>{{ t("settingsProfile.discoverableHint") }}</small>
            </span>
          </label>
          <label class="profile-edit__flag">
            <input
              v-model="hideCollections"
              type="checkbox"
              :disabled="pending"
            />
            <span>
              {{ t("settingsProfile.hideCollections") }}
              <small>{{ t("settingsProfile.hideCollectionsHint") }}</small>
            </span>
          </label>
          <label class="profile-edit__flag">
            <input v-model="indexable" type="checkbox" :disabled="pending" />
            <span>
              {{ t("settingsProfile.indexable") }}
              <small>{{ t("settingsProfile.indexableHint") }}</small>
            </span>
          </label>
        </section>

        <p v-if="error" class="profile-edit__error" role="alert">{{ error }}</p>
        <div class="profile-edit__footer">
          <Button type="submit" :loading="pending" :disabled="pending">
            {{ t("settingsProfile.save") }}
          </Button>
          <p v-if="saved" class="profile-edit__saved">
            {{ t("settingsProfile.saved") }}
          </p>
        </div>
      </form>
    </Card>
  </div>
</template>

<style scoped>
.profile-edit {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
}
.profile-edit-skel {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
  max-width: 26rem;
}
.profile-edit__group {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.profile-edit__group-title {
  margin: 0;
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-sm);
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--text-muted);
}
.profile-edit__row {
  display: grid;
  grid-template-columns: minmax(8rem, 11rem) minmax(0, 26rem);
  align-items: center;
  gap: var(--space-2) var(--space-4);
  padding: var(--space-3) 0;
}
.profile-edit__row + .profile-edit__row {
  border-top: 1px solid var(--border);
}
.profile-edit__row > span {
  font-weight: 600;
  color: var(--text-muted);
}
.profile-edit__row--top {
  align-items: start;
}
.profile-edit__row--top > span {
  padding-top: var(--space-1);
}
.profile-edit__media {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-3);
}
.profile-edit__media-actions {
  display: flex;
  gap: var(--space-2);
}
.profile-edit__file {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
}
.profile-edit__header,
.profile-edit__header-placeholder {
  width: 100%;
  max-width: 20rem;
  aspect-ratio: 3 / 1;
  border-radius: var(--radius-md);
  background: var(--bg-sunken);
}
.profile-edit__header {
  object-fit: cover;
  display: block;
}
.profile-edit__sr-label :deep(.field__label) {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
}
.profile-edit__bio {
  width: 100%;
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  font: inherit;
  background: var(--bg);
  color: var(--text);
  resize: vertical;
}
.profile-edit__field-row {
  display: grid;
  grid-template-columns: 1fr 1fr auto;
  align-items: end;
  gap: var(--space-2);
}
.profile-edit__flag {
  display: flex;
  align-items: flex-start;
  gap: var(--space-2);
  padding: var(--space-1) 0;
}
.profile-edit__flag span {
  display: flex;
  flex-direction: column;
}
.profile-edit__flag small {
  color: var(--text-muted);
}
.profile-edit__footer {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.profile-edit__error {
  color: var(--danger);
  margin: 0;
}
.profile-edit__saved {
  color: var(--success);
  margin: 0;
}
@media (max-width: 768px) {
  .profile-edit__row,
  .profile-edit__field-row {
    grid-template-columns: 1fr;
  }
  .profile-edit__field-remove {
    justify-self: start;
  }
}
</style>
