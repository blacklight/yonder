<script setup lang="ts">
import { useI18n } from "vue-i18n";
import type { PollDraft } from "@/stores/compose";

const props = defineProps<{
  poll: PollDraft;
  maxOptions: number;
  maxCharactersPerOption: number;
  minExpiration: number;
  maxExpiration: number;
}>();
const emit = defineEmits<{
  update: [poll: PollDraft];
  remove: [];
}>();
const { t } = useI18n();

const EXPIRATIONS = [
  { s: 300, key: "5m" },
  { s: 1800, key: "30m" },
  { s: 3600, key: "1h" },
  { s: 21600, key: "6h" },
  { s: 86400, key: "1d" },
  { s: 604800, key: "7d" },
];

function patch(p: Partial<PollDraft>) {
  emit("update", { ...props.poll, ...p });
}

function setOption(i: number, value: string) {
  const options = [...props.poll.options];
  options[i] = value;
  patch({ options });
}
</script>

<template>
  <fieldset class="poll" data-testid="poll-builder">
    <legend class="poll__legend">
      {{ t("compose.poll") }}
      <button type="button" class="poll__remove" @click="emit('remove')">
        {{ t("compose.pollRemove") }}
      </button>
    </legend>
    <input
      v-for="(opt, i) in poll.options"
      :key="i"
      :value="opt"
      :placeholder="t('compose.pollOption', { n: i + 1 })"
      :maxlength="maxCharactersPerOption"
      class="poll__option"
      @input="setOption(i, ($event.target as HTMLInputElement).value)"
    />
    <button
      v-if="poll.options.length < maxOptions"
      type="button"
      class="poll__add"
      @click="patch({ options: [...poll.options, ''] })"
    >
      +
    </button>
    <div class="poll__row">
      <label>
        {{ t("compose.pollExpires") }}
        <select
          :value="poll.expiresIn"
          @change="
            patch({
              expiresIn: Number(($event.target as HTMLSelectElement).value),
            })
          "
        >
          <option
            v-for="e in EXPIRATIONS.filter(
              (x) => x.s >= minExpiration && x.s <= maxExpiration,
            )"
            :key="e.s"
            :value="e.s"
          >
            {{ e.key }}
          </option>
        </select>
      </label>
      <label>
        <input
          type="checkbox"
          :checked="poll.multiple"
          @change="
            patch({ multiple: ($event.target as HTMLInputElement).checked })
          "
        />
        {{ t("compose.pollMultiple") }}
      </label>
    </div>
  </fieldset>
</template>

<style scoped>
.poll {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-3);
}
.poll__legend {
  padding: 0 var(--space-1);
  font-weight: 600;
}
.poll__remove {
  border: 0;
  background: none;
  color: var(--text-muted);
  cursor: pointer;
  font: inherit;
  font-size: var(--text-sm);
}
.poll__option {
  padding: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  font: inherit;
  background: var(--bg);
  color: var(--text);
}
.poll__add {
  align-self: flex-start;
  border: 1px dashed var(--border);
  background: none;
  border-radius: var(--radius-sm);
  padding: var(--space-1) var(--space-3);
  cursor: pointer;
  color: var(--text);
}
.poll__row {
  display: flex;
  gap: var(--space-3);
  align-items: center;
  font-size: var(--text-sm);
  color: var(--text-muted);
}
</style>
