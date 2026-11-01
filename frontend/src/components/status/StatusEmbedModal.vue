<script setup lang="ts">
import { computed, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import Modal from "@/components/ui/Modal.vue";
import { iframeEmbed, scriptEmbed } from "@/utils/embed";
import { FontAwesomeIcon } from "@/icons";

const props = defineProps<{
  statusId: string;
  /** Accessible `title` attribute baked into the `<iframe>` snippet. */
  title: string;
}>();
const emit = defineEmits<{ close: [] }>();
const { t } = useI18n();

const snippets = computed(() => [
  {
    id: "script",
    label: t("embed.script"),
    hint: t("embed.scriptHint"),
    code: scriptEmbed(props.statusId, props.title),
  },
  {
    id: "iframe",
    label: t("embed.iframe"),
    hint: t("embed.iframeHint"),
    code: iframeEmbed(props.statusId, props.title),
  },
]);

const copied = ref<string | null>(null);
let resetTimer: ReturnType<typeof setTimeout> | null = null;

async function copy(id: string, code: string) {
  try {
    await navigator.clipboard.writeText(code);
    copied.value = id;
    if (resetTimer) clearTimeout(resetTimer);
    resetTimer = setTimeout(() => (copied.value = null), 2000);
  } catch {
    // Clipboard denied — the textarea stays selectable for manual copy.
  }
}

onUnmounted(() => {
  if (resetTimer) clearTimeout(resetTimer);
});
</script>

<template>
  <Modal :title="t('embed.title')" @close="emit('close')">
    <p class="embed-modal__hint">{{ t("embed.hint") }}</p>
    <section
      v-for="snippet in snippets"
      :key="snippet.id"
      class="embed-modal__snippet"
    >
      <h3 class="embed-modal__label">{{ snippet.label }}</h3>
      <p class="embed-modal__desc">{{ snippet.hint }}</p>
      <textarea
        class="embed-modal__code"
        :value="snippet.code"
        readonly
        rows="4"
        :data-testid="`embed-${snippet.id}`"
        @click="($event.target as HTMLTextAreaElement).select()"
      />
      <button
        type="button"
        class="embed-modal__copy"
        :data-testid="`embed-copy-${snippet.id}`"
        @click="copy(snippet.id, snippet.code)"
      >
        <FontAwesomeIcon :icon="copied === snippet.id ? 'check' : 'copy'" />
        {{ copied === snippet.id ? t("embed.copied") : t("embed.copy") }}
      </button>
    </section>
  </Modal>
</template>

<style scoped>
.embed-modal__hint {
  margin: 0 0 var(--space-3);
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.embed-modal__snippet + .embed-modal__snippet {
  margin-top: var(--space-4);
}
.embed-modal__label {
  margin: 0 0 var(--space-1);
  font-size: var(--text-sm);
}
.embed-modal__desc {
  margin: 0 0 var(--space-2);
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.embed-modal__code {
  width: 100%;
  resize: vertical;
  font-family: var(--font-mono, monospace);
  font-size: var(--text-sm);
  padding: var(--space-2);
  color: var(--text);
  background: var(--bg);
  border: 1px solid var(--border-dim);
  border-radius: var(--radius-sm);
}
.embed-modal__copy {
  margin-top: var(--space-2);
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-1) var(--space-3);
  border: 1px solid var(--border-dim);
  border-radius: var(--radius-sm);
  background: none;
  color: var(--text);
  font-size: var(--text-sm);
  cursor: pointer;
}
.embed-modal__copy:hover {
  background: var(--bg-sunken);
}
</style>
