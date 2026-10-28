<script setup lang="ts">
import { computed, ref, useTemplateRef } from "vue";
import { useI18n } from "vue-i18n";
import { FontAwesomeIcon } from "@/icons";
import type { MediaAttachment } from "@/api/entities";

const props = defineProps<{
  media: MediaAttachment;
}>();
const { t } = useI18n();

const audio = useTemplateRef<HTMLAudioElement>("audio");
const playing = ref(false);
const current = ref(0);
const duration = ref(0);
const volume = ref(1);
const muted = ref(false);

// `preview_url` falls back to `url` server-side when the remote had no
// poster art — it only counts as cover art when it actually differs.
const art = computed(() =>
  props.media.preview_url && props.media.preview_url !== props.media.url
    ? props.media.preview_url
    : null,
);
const title = computed(
  () =>
    props.media.description ??
    props.media.url.split("/").pop()?.split("?")[0] ??
    "",
);
const volumeIcon = computed(() =>
  muted.value || volume.value === 0 ? "volume-xmark" : "volume-high",
);

function fmt(sec: number): string {
  if (!Number.isFinite(sec) || sec < 0) return "0:00";
  const s = Math.floor(sec);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function toggle() {
  const el = audio.value;
  if (!el) return;
  if (el.paused) void el.play();
  else el.pause();
}

function seek(e: Event) {
  const el = audio.value;
  const v = Number((e.target as HTMLInputElement).value);
  if (el) el.currentTime = v;
  current.value = v;
}

function setVolume(e: Event) {
  const el = audio.value;
  const v = Number((e.target as HTMLInputElement).value);
  volume.value = v;
  if (el) {
    el.volume = v;
    el.muted = false;
    muted.value = false;
  }
}

function toggleMute() {
  const el = audio.value;
  if (!el) return;
  el.muted = !el.muted;
  muted.value = el.muted;
}

function onMeta() {
  if (audio.value && Number.isFinite(audio.value.duration)) {
    duration.value = audio.value.duration;
  }
}
</script>

<template>
  <div class="audio-player">
    <audio
      ref="audio"
      :src="media.url"
      preload="metadata"
      @loadedmetadata="onMeta"
      @durationchange="onMeta"
      @timeupdate="current = audio?.currentTime ?? 0"
      @play="playing = true"
      @pause="playing = false"
      @ended="playing = false"
    />
    <div class="audio-player__art">
      <img v-if="art" :src="art" :alt="title" class="audio-player__cover" />
      <FontAwesomeIcon v-else icon="music" class="audio-player__placeholder" />
    </div>
    <div class="audio-player__main">
      <div class="audio-player__title" :title="title">{{ title }}</div>
      <div class="audio-player__controls">
        <button
          type="button"
          class="audio-player__btn audio-player__play"
          :aria-label="
            playing ? t('status.media.pause') : t('status.media.play')
          "
          :title="playing ? t('status.media.pause') : t('status.media.play')"
          @click="toggle"
        >
          <FontAwesomeIcon :icon="playing ? 'pause' : 'play'" />
        </button>
        <span class="audio-player__time"
          >{{ fmt(current) }} / {{ fmt(duration) }}</span
        >
        <input
          type="range"
          class="audio-player__seek"
          :value="current"
          :max="duration || 0"
          step="0.1"
          :aria-label="t('status.media.seek')"
          @input="seek"
        />
        <div class="audio-player__volume">
          <button
            type="button"
            class="audio-player__btn"
            :aria-label="
              muted ? t('status.media.unmute') : t('status.media.mute')
            "
            :title="muted ? t('status.media.unmute') : t('status.media.mute')"
            @click="toggleMute"
          >
            <FontAwesomeIcon :icon="volumeIcon" />
          </button>
          <input
            type="range"
            class="audio-player__volume-slider"
            :value="muted ? 0 : volume"
            min="0"
            max="1"
            step="0.05"
            :aria-label="t('status.media.volume')"
            @input="setVolume"
          />
        </div>
        <a
          class="audio-player__btn audio-player__download"
          :href="media.url"
          download
          :aria-label="t('status.media.download')"
          :title="t('status.media.download')"
        >
          <FontAwesomeIcon icon="download" />
        </a>
      </div>
    </div>
  </div>
</template>

<style scoped>
.audio-player {
  display: flex;
  gap: var(--space-3);
  align-items: stretch;
  padding: var(--space-3);
  background: var(--bg-sunken);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
}
.audio-player__art {
  flex: 0 0 5.5rem;
  width: 5.5rem;
  border-radius: var(--radius-sm);
  overflow: hidden;
  background: var(--accent-soft);
  display: flex;
  align-items: center;
  justify-content: center;
}
.audio-player__cover {
  width: 100%;
  height: 100%;
  object-fit: cover;
  display: block;
}
.audio-player__placeholder {
  color: var(--accent);
  font-size: var(--text-xl);
}
.audio-player__main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  justify-content: center;
  gap: var(--space-2);
}
.audio-player__title {
  font-weight: 600;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.audio-player__controls {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.audio-player__btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 2rem;
  height: 2rem;
  border: 0;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text-muted);
  cursor: pointer;
}
.audio-player__btn:hover {
  background: var(--bg-raised);
  color: var(--accent);
}
.audio-player__play {
  color: var(--accent);
  font-size: var(--text-lg);
}
.audio-player__time {
  font-size: var(--text-xs);
  color: var(--text-muted);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
.audio-player__seek {
  flex: 1;
  min-width: 0;
  accent-color: var(--accent);
  cursor: pointer;
}
.audio-player__volume {
  display: flex;
  align-items: center;
}
.audio-player__volume-slider {
  width: 0;
  opacity: 0;
  transition:
    width 0.15s ease,
    opacity 0.15s ease;
  accent-color: var(--accent);
  cursor: pointer;
}
.audio-player__volume:hover .audio-player__volume-slider,
.audio-player__volume:focus-within .audio-player__volume-slider {
  width: 4.5rem;
  opacity: 1;
}
@media (max-width: 480px) {
  .audio-player__art {
    flex-basis: 4rem;
    width: 4rem;
  }
  .audio-player__time {
    display: none;
  }
}
</style>
