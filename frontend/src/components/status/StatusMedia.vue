<script setup lang="ts">
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import Modal from "@/components/ui/Modal.vue";
import type { MediaAttachment } from "@/api/entities";

defineProps<{
  media: MediaAttachment[];
  sensitive: boolean;
}>();
const { t } = useI18n();

const revealed = ref(false);
const altModal = ref<MediaAttachment | null>(null);
</script>

<template>
  <div v-if="media.length" class="status-media">
    <div
      v-if="sensitive && !revealed"
      class="status-media__cover"
      data-testid="sensitive-cover"
    >
      <button
        type="button"
        class="status-media__reveal"
        @click="revealed = true"
      >
        {{ t("status.showSensitive") }}
      </button>
    </div>
    <div
      v-else
      class="status-media__grid"
      :class="`status-media__grid--${media.length}`"
    >
      <figure v-for="m in media" :key="m.id" class="status-media__item">
        <img
          v-if="m.type === 'image' || m.type === 'gifv'"
          :src="m.preview_url ?? m.url"
          :alt="m.description ?? ''"
          class="status-media__img"
        />
        <video
          v-else-if="m.type === 'video'"
          :src="m.url"
          controls
          class="status-media__img"
        />
        <audio v-else-if="m.type === 'audio'" :src="m.url" controls />
        <a v-else :href="m.url" target="_blank" rel="noopener">{{
          m.description ?? m.url
        }}</a>
        <button
          v-if="m.description"
          type="button"
          class="status-media__alt"
          @click="altModal = m"
        >
          ALT
        </button>
      </figure>
    </div>
    <Modal
      v-if="altModal !== null"
      :title="t('status.altText')"
      @close="altModal = null"
    >
      <p>{{ altModal?.description }}</p>
    </Modal>
  </div>
</template>

<style scoped>
.status-media {
  max-height: 30rem;
}
.status-media__cover {
  padding: var(--space-5);
  background: var(--bg-sunken);
  border-radius: var(--radius-md);
  text-align: center;
}
.status-media__reveal {
  border: 1px solid var(--border);
  background: var(--bg-raised);
  border-radius: var(--radius-md);
  padding: var(--space-2) var(--space-4);
  color: var(--text);
  cursor: pointer;
  font: inherit;
}
.status-media__grid {
  display: grid;
  gap: var(--space-2);
  grid-template-columns: repeat(auto-fit, minmax(10rem, 1fr));
}
.status-media__item {
  margin: 0;
  position: relative;
}
.status-media__img {
  width: 100%;
  max-height: 30rem;
  /* fix the ratio */
  object-fit: cover;
  border-radius: var(--radius-md);
  display: block;
}
.status-media__alt {
  position: absolute;
  bottom: var(--space-2);
  right: var(--space-2);
  border: 0;
  border-radius: var(--radius-sm);
  background: rgb(0 0 0 / 70%);
  color: #fff;
  font-size: var(--text-xs);
  font-weight: 700;
  padding: 2px 6px;
  cursor: pointer;
}
</style>
