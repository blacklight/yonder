<script setup lang="ts">
import { computed, useId, useTemplateRef } from "vue";
import { useI18n } from "vue-i18n";
import { FontAwesomeIcon } from "@/icons";

const props = withDefaults(
  defineProps<{
    modelValue?: string;
    label: string;
    type?: string;
    error?: string;
    disabled?: boolean;
    placeholder?: string;
    maxlength?: number;
    autocomplete?: string;
    inputmode?:
      | "none"
      | "text"
      | "tel"
      | "url"
      | "email"
      | "numeric"
      | "decimal"
      | "search";
    required?: boolean;
    clearable?: boolean;
  }>(),
  { modelValue: "", type: "text", disabled: false, clearable: false },
);
const emit = defineEmits<{ "update:modelValue": [value: string] }>();

const { t } = useI18n();
const id = useId();
const errorId = `${id}-error`;
const describedBy = computed(() => (props.error ? errorId : undefined));
const inputEl = useTemplateRef<HTMLInputElement>("input");
const showClear = computed(
  () => props.clearable && !props.disabled && props.modelValue.length > 0,
);

function onInput(event: Event) {
  emit("update:modelValue", (event.target as HTMLInputElement).value);
}

function clear() {
  emit("update:modelValue", "");
  inputEl.value?.focus();
}
</script>

<template>
  <div class="field">
    <label class="field__label" :for="id">{{ label }}</label>
    <div class="field__control">
      <input
        :id="id"
        ref="input"
        class="field__input"
        :class="{
          'field__input--error': error,
          'field__input--clearable': showClear,
        }"
        :type="type"
        :value="modelValue"
        :disabled="disabled"
        :placeholder="placeholder"
        :maxlength="maxlength"
        :autocomplete="autocomplete"
        :inputmode="inputmode"
        :required="required"
        :aria-invalid="!!error"
        :aria-describedby="describedBy"
        @input="onInput"
      />
      <button
        v-if="showClear"
        type="button"
        class="field__clear"
        :aria-label="t('common.clear')"
        :title="t('common.clear')"
        data-testid="input-clear"
        @click="clear"
      >
        <FontAwesomeIcon icon="xmark" />
      </button>
    </div>
    <p v-if="error" :id="errorId" class="field__error" role="alert">
      {{ error }}
    </p>
  </div>
</template>

<style scoped>
.field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.field__label {
  font-size: var(--text-sm);
  font-weight: 600;
  color: var(--text-muted);
}
.field__control {
  position: relative;
  display: flex;
}
.field__input {
  flex: 1;
  min-width: 0;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-raised);
}
.field__input--clearable {
  padding-inline-end: 2rem;
}
/* The clear button replaces the native WebKit search cancel. */
.field__input--clearable::-webkit-search-cancel-button {
  appearance: none;
}
.field__input--error {
  border-color: var(--danger);
}
.field__input:disabled {
  opacity: 0.6;
}
.field__clear {
  position: absolute;
  inset-inline-end: var(--space-1);
  top: 50%;
  transform: translateY(-50%);
  display: grid;
  place-items: center;
  width: 1.75rem;
  height: 1.75rem;
  padding: 0;
  border: none;
  border-radius: 50%;
  background: transparent;
  color: var(--text-muted);
  cursor: pointer;
}
.field__clear:hover {
  background: var(--bg-sunken);
  color: var(--text);
}
.field__error {
  margin: 0;
  font-size: var(--text-sm);
  color: var(--danger);
}
</style>
