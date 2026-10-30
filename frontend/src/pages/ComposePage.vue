<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import Card from "@/components/ui/Card.vue";
import Button from "@/components/ui/Button.vue";
import Modal from "@/components/ui/Modal.vue";
import ComposeBox from "@/components/compose/ComposeBox.vue";
import { FontAwesomeIcon } from "@/icons";
import { useComposeStore, type DraftEntry } from "@/stores/compose";
import { formatRelativeDate } from "@/utils/relativeDate";

const { t } = useI18n();
const compose = useComposeStore();

const draftsOpen = ref(false);
// A saved draft waiting on the "replace current draft?" confirm —
// opening one while the composer has typed text asks first.
const pendingOpen = ref<DraftEntry | null>(null);

/** One-line preview: first non-empty text line, else a marker for
 *  context/media-only drafts. */
function draftLabel(d: DraftEntry): string {
  const line = d.text.split("\n").find((l) => l.trim());
  if (line) return line.length > 80 ? `${line.slice(0, 80)}…` : line;
  if (d.mediaIds.length)
    return t("compose.draftMedia", { n: d.mediaIds.length });
  if (d.inReplyToId && d.replyToAcct)
    return t("compose.draftReplyTo", { user: d.replyToAcct });
  return t("compose.draftUntitled");
}

const sortedDrafts = computed(() =>
  [...compose.drafts].sort((a, b) => b.updatedAt.localeCompare(a.updatedAt)),
);

function openDraft(entry: DraftEntry) {
  // Reopening the draft that's already loaded is a no-op; replacing a
  // dirty draft with a *different* entry asks first.
  if (compose.draft.refId === entry.id) {
    draftsOpen.value = false;
    return;
  }
  if (compose.isDirty) {
    pendingOpen.value = entry;
    return;
  }
  compose.loadDraftEntry(entry.id);
  draftsOpen.value = false;
}

function confirmOpen() {
  const entry = pendingOpen.value;
  pendingOpen.value = null;
  if (entry) {
    compose.loadDraftEntry(entry.id);
    draftsOpen.value = false;
  }
}

function saveCurrent() {
  compose.saveAsDraft();
}
</script>

<template>
  <Card>
    <template #header>
      <div class="compose-page__head">
        <span>{{ t("compose.title") }}</span>
        <button
          type="button"
          class="compose-page__drafts-toggle"
          data-testid="drafts-toggle"
          @click="draftsOpen = !draftsOpen"
        >
          <FontAwesomeIcon icon="folder-open" />
          {{ t("compose.drafts") }}
          <span v-if="compose.drafts.length" class="compose-page__count">{{
            compose.drafts.length
          }}</span>
        </button>
      </div>
    </template>

    <div v-if="draftsOpen" class="drafts" data-testid="drafts-panel">
      <div class="drafts__bar">
        <Button
          size="sm"
          :disabled="!compose.isDirty && !compose.hasContext"
          @click="saveCurrent"
        >
          {{ t("compose.draftSave") }}
        </Button>
      </div>
      <p v-if="!sortedDrafts.length" class="drafts__empty">
        {{ t("compose.draftsEmpty") }}
      </p>
      <ul v-else class="drafts__list">
        <li v-for="entry in sortedDrafts" :key="entry.id" class="drafts__row">
          <button
            type="button"
            class="drafts__open"
            :data-testid="`draft-open-${entry.id}`"
            @click="openDraft(entry)"
          >
            <span class="drafts__label">{{ draftLabel(entry) }}</span>
            <time class="drafts__when" :datetime="entry.updatedAt">{{
              formatRelativeDate(entry.updatedAt)
            }}</time>
          </button>
          <button
            type="button"
            class="drafts__delete"
            :title="t('compose.draftDelete')"
            :aria-label="t('compose.draftDelete')"
            :data-testid="`draft-delete-${entry.id}`"
            @click="compose.removeDraft(entry.id)"
          >
            <FontAwesomeIcon icon="trash-can" />
          </button>
        </li>
      </ul>
    </div>

    <ComposeBox class="compose-page__box" standalone />

    <Modal
      v-if="pendingOpen"
      :title="t('status.confirm.title')"
      @close="pendingOpen = null"
    >
      <p>{{ t("compose.discardText") }}</p>
      <div class="confirm-btns">
        <Button variant="secondary" @click="pendingOpen = null">
          {{ t("ui.cancel") }}
        </Button>
        <Button
          variant="danger"
          data-testid="draft-open-confirm"
          @click="confirmOpen"
        >
          {{ t("ui.confirm") }}
        </Button>
      </div>
    </Modal>
  </Card>
</template>

<style scoped>
/* Standalone page only: give the form most of the viewport. `dvh`
   tracks mobile browser chrome (vh fallback for old engines); the
   floor keeps the form usable on short screens — the page scrolls
   rather than crushing the textarea. Embedded composers keep their
   natural compact size. */
.compose-page__box {
  height: 80vh;
  height: 80dvh;
  min-height: 26rem;
}
.compose-page__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
}
.compose-page__drafts-toggle {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-1) var(--space-2);
  border-radius: var(--radius-md);
  color: var(--text-muted);
  font-size: var(--text-sm);
  cursor: pointer;
}
.compose-page__drafts-toggle:hover {
  color: var(--text);
}
.compose-page__count {
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 0 var(--space-1);
  font-size: var(--text-xs);
}
.drafts {
  border-bottom: 1px solid var(--border);
  padding-bottom: var(--space-3);
  margin-bottom: var(--space-3);
}
.drafts__bar {
  display: flex;
  justify-content: flex-end;
  margin-bottom: var(--space-2);
}
.drafts__empty {
  color: var(--text-faint);
  font-size: var(--text-sm);
}
.drafts__list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.drafts__row {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.drafts__open {
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: var(--space-2);
  padding: var(--space-2);
  border-radius: var(--radius-md);
  text-align: left;
  cursor: pointer;
}
.drafts__open:hover {
  background: var(--bg-raised);
}
.drafts__label {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.drafts__when {
  flex-shrink: 0;
  color: var(--text-faint);
  font-size: var(--text-xs);
}
.drafts__delete {
  flex-shrink: 0;
  padding: var(--space-2);
  color: var(--text-faint);
  cursor: pointer;
}
.drafts__delete:hover {
  color: var(--danger);
}
.confirm-btns {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-2);
  margin-top: var(--space-3);
}
</style>
