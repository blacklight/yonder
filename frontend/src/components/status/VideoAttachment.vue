<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, useTemplateRef } from "vue";
import type { MediaAttachment } from "@/api/entities";

const props = withDefaults(
  defineProps<{
    media: MediaAttachment;
    autoplay?: boolean;
  }>(),
  { autoplay: false },
);

const video = useTemplateRef<HTMLVideoElement>("video");

const poster = computed(() =>
  props.media.preview_url && props.media.preview_url !== props.media.url
    ? props.media.preview_url
    : undefined,
);

// HLS playlists need hls.js outside Safari — detect by declared
// content type or the `.m3u8` extension.
const isHls = computed(() => {
  const mt = props.media.yonder?.content_type ?? "";
  if (
    mt === "application/x-mpegURL" ||
    mt === "application/vnd.apple.mpegurl"
  ) {
    return true;
  }
  return props.media.url.split("?")[0].endsWith(".m3u8");
});

let hls: import("hls.js").default | null = null;

onMounted(async () => {
  const el = video.value;
  if (!el || !isHls.value) return;
  if (el.canPlayType("application/vnd.apple.mpegurl")) return;
  const { default: Hls } = await import("hls.js");
  if (!Hls.isSupported()) return;
  hls = new Hls({ capLevelToPlayerSize: true });
  hls.loadSource(props.media.url);
  hls.attachMedia(el);
});

onBeforeUnmount(() => {
  hls?.destroy();
  hls = null;
});
</script>

<template>
  <video
    ref="video"
    :src="media.url"
    :poster="poster"
    controls
    playsinline
    :autoplay="autoplay || undefined"
    class="video-attachment"
  />
</template>

<style scoped>
.video-attachment {
  display: block;
  background: #000;
}
</style>
