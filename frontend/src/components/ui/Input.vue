<script setup lang="ts">
import { computed, useId } from "vue";

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
  }>(),
  { modelValue: "", type: "text", disabled: false },
);
const emit = defineEmits<{ "update:modelValue": [value: string] }>();

const id = useId();
const errorId = `${id}-error`;
const describedBy = computed(() => (props.error ? errorId : undefined));

function onInput(event: Event) {
  emit("update:modelValue", (event.target as HTMLInputElement).value);
}
</script>

<template>
  <div class="field">
    <label class="field__label" :for="id">{{ label }}</label>
    <input
      :id="id"
      class="field__input"
      :class="{ 'field__input--error': error }"
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
.field__input {
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-raised);
}
.field__input--error {
  border-color: var(--danger);
}
.field__input:disabled {
  opacity: 0.6;
}
.field__error {
  margin: 0;
  font-size: var(--text-sm);
  color: var(--danger);
}
</style>
