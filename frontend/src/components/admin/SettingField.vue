<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { FontAwesomeIcon } from "@/icons";
import { ApiError } from "@/api/errors";
import type { AdminSetting } from "@/api/adminSettings";
import { useAdminSettingsStore } from "@/stores/adminSettings";
import Button from "@/components/ui/Button.vue";
import SettingHelp from "./SettingHelp.vue";

const props = defineProps<{ entry: AdminSetting }>();
const emit = defineEmits<{ updated: [entry: AdminSetting] }>();
const { t } = useI18n();
const store = useAdminSettingsStore();

type Kind = "bool" | "number" | "text" | "json";

// String-valued keys whose values come from a fixed set — rendered as
// a dropdown instead of a free-text input.
const ENUM_OPTIONS: Record<string, string[]> = {
  "email.smtp_tls": ["off", "starttls", "implicit"],
};

function kindOf(value: unknown): Kind {
  if (typeof value === "boolean") return "bool";
  if (typeof value === "number") return "number";
  // `null` is an unset optional — edit it as a string, not JSON.
  if (typeof value === "string" || value === null) return "text";
  return "json";
}

function serialize(value: unknown): string {
  return typeof value === "object" && value !== null
    ? JSON.stringify(value)
    : String(value ?? "");
}

// Sensitive keys never reveal their value — start blank and show a
// "set / not set" placeholder instead. Everyone else starts at the
// effective value.
const kind = computed<Kind>(() =>
  props.entry.sensitive ? "text" : kindOf(props.entry.value),
);
const selectOptions = computed(() =>
  props.entry.sensitive ? undefined : ENUM_OPTIONS[props.entry.toml_path],
);
const draft = ref(props.entry.sensitive ? "" : serialize(props.entry.value));
const checked = ref(props.entry.value === true);
const error = ref("");
const saved = ref(false);
const pending = ref(false);
let savedTimer: ReturnType<typeof setTimeout> | undefined;

watch(
  () => props.entry,
  (entry) => {
    draft.value = entry.sensitive ? "" : serialize(entry.value);
    checked.value = entry.value === true;
  },
);

const dirty = computed(() =>
  kind.value === "bool"
    ? checked.value !== (props.entry.value === true)
    : draft.value !==
      (props.entry.sensitive ? "" : serialize(props.entry.value)),
);

const overridden = computed(
  () => props.entry.source === "overlay" && props.entry.writable,
);

function parse(): unknown {
  if (kind.value === "bool") return checked.value;
  if (kind.value === "number") {
    const n = Number(draft.value);
    if (draft.value.trim() === "" || Number.isNaN(n)) {
      throw new Error(t("admin.settings.invalidNumber"));
    }
    return n;
  }
  if (kind.value === "json") {
    try {
      return JSON.parse(draft.value);
    } catch {
      throw new Error(t("admin.settings.invalidJson"));
    }
  }
  return draft.value;
}

async function save() {
  error.value = "";
  saved.value = false;
  let value: unknown;
  try {
    value = parse();
  } catch (e) {
    error.value = (e as Error).message;
    return;
  }
  pending.value = true;
  try {
    const entry = await store.set(props.entry.toml_path, value);
    emit("updated", entry);
    flashSaved();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

async function reset() {
  error.value = "";
  pending.value = true;
  try {
    const entry = await store.reset(props.entry.toml_path);
    emit("updated", entry);
    flashSaved();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

function flashSaved() {
  clearTimeout(savedTimer);
  saved.value = true;
  savedTimer = setTimeout(() => (saved.value = false), 3000);
}
</script>

<template>
  <div class="setting">
    <div class="setting__head">
      <code class="setting__key">{{ entry.toml_path }}</code>
      <SettingHelp :entry="entry" />
      <span v-if="entry.requires_restart" class="setting__badge">
        {{ t("admin.settings.requiresRestart") }}
      </span>
    </div>

    <template v-if="!entry.writable">
      <p class="setting__value">{{ serialize(entry.value) || "—" }}</p>
      <p class="setting__hint">{{ t("admin.settings.readOnly") }}</p>
    </template>

    <template v-else>
      <div class="setting__control">
        <label v-if="kind === 'bool'" class="setting__check" :class="{ checked }">
          <input
            v-model="checked"
            type="checkbox"
            :disabled="pending"
            @change="save"
          />
          <FontAwesomeIcon
            :icon="checked ? 'toggle-on' : 'toggle-off'"
            aria-hidden="true"
          />
          {{ checked ? t("admin.settings.on") : t("admin.settings.off") }}
        </label>
        <template v-else-if="kind === 'json'">
          <textarea
            v-model="draft"
            class="setting__textarea"
            rows="3"
            :disabled="pending"
            :aria-label="entry.toml_path"
            spellcheck="false"
          />
        </template>
        <select
          v-else-if="selectOptions"
          v-model="draft"
          class="setting__input"
          :disabled="pending"
          :aria-label="entry.toml_path"
        >
          <option v-for="opt in selectOptions" :key="opt" :value="opt">
            {{ opt }}
          </option>
        </select>
        <input
          v-else
          v-model="draft"
          class="setting__input"
          :type="
            entry.sensitive ? 'password' : kind === 'number' ? 'number' : 'text'
          "
          :autocomplete="entry.sensitive ? 'new-password' : undefined"
          :disabled="pending"
          :placeholder="
            entry.sensitive
              ? entry.set
                ? t('admin.settings.secretSet')
                : t('admin.settings.secretUnset')
              : ''
          "
          :aria-label="entry.toml_path"
          spellcheck="false"
        />
        <Button
          v-if="kind !== 'bool'"
          size="sm"
          :disabled="!dirty"
          :loading="pending"
          @click="save"
        >
          {{ t("common.save") }}
        </Button>
        <Button
          v-if="overridden"
          variant="secondary"
          size="sm"
          :disabled="pending"
          @click="reset"
        >
          {{ t("admin.settings.reset") }}
        </Button>
        <span v-if="saved" class="setting__saved">
          {{ t("admin.settings.saved") }}
        </span>
      </div>
      <p v-if="entry.sensitive" class="setting__hint">
        {{ t("admin.settings.secretHint") }}
      </p>
      <p v-if="error" class="setting__error" role="alert">{{ error }}</p>
    </template>
  </div>
</template>

<style scoped>
.setting {
  padding: var(--space-3) 0;
}
.setting__head {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.setting__key {
  font-size: var(--font-sm, 0.8125rem);
  font-weight: 600;
}
.setting__badge {
  font-size: var(--text-xs);
  padding: 0 var(--space-1);
  border-radius: var(--radius-sm);
  background: var(--bg-raised);
  color: var(--text-muted);
}
.setting__control {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  margin-top: var(--space-1);
}
.setting__input {
  max-width: 18rem;
  flex: 1;
  min-width: 0;
  padding: var(--space-1) var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg);
  color: var(--text);
  font: inherit;
  font-family: monospace;
  font-size: var(--font-sm, 0.8125rem);
}
.setting__textarea {
  flex: 1;
  min-width: 0;
  max-width: 24rem;
  padding: var(--space-1) var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg);
  color: var(--text);
  font-family: monospace;
  font-size: var(--font-sm, 0.8125rem);
  resize: vertical;
}
.setting__check {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  cursor: pointer;
}
.setting__check input {
  /* The toggle glyph is the visible control; keep the real input in
     the a11y tree but off the visual path. */
  position: absolute;
  opacity: 0;
  width: 1px;
  height: 1px;
}
.setting__check svg {
  width: 1.75rem;
  height: 1.75rem;
  color: gray;
}
.setting__check.checked svg {
  color: green;
}
.setting__value {
  margin: var(--space-1) 0 0;
  font-family: monospace;
  font-size: var(--font-sm, 0.8125rem);
  word-break: break-all;
}
.setting__hint {
  margin: var(--space-1) 0 0;
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.setting__error {
  margin: var(--space-1) 0 0;
  font-size: var(--text-xs);
  color: var(--danger, #dc2626);
}
.setting__saved {
  font-size: var(--text-xs);
  color: var(--accent-link);
}
</style>
