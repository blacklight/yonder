<script setup lang="ts">
// Shared favourites/bookmarks page — both endpoints paginate via Link headers.
import { onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { requestWithResponse } from "@/api/client";
import { paginated } from "@/api/pagination";
import type { Status } from "@/api/entities";
import StatusCard from "@/components/status/StatusCard.vue";
import StatusCardSkeleton from "@/components/status/StatusCardSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import { useStatusEvents } from "@/composables/useStatusEvents";
import { dropFromStatusList, patchStatusList } from "@/utils/statusStream";

const props = defineProps<{ feed: "favourites" | "bookmarks" }>();
const { t } = useI18n();
const items = ref<Status[]>([]);
const next = ref<string | undefined>();
const loading = ref(false);
const error = ref<string | null>(null);

async function load(more = false) {
  loading.value = true;
  error.value = null;
  try {
    const max_id =
      more && next.value
        ? (new URL(next.value, location.origin).searchParams.get("max_id") ??
          undefined)
        : undefined;
    const { data, response } = await requestWithResponse<Status[]>({
      path: `/${props.feed}`,
      query: { max_id },
    });
    const page = paginated(response, data);
    items.value = more ? [...items.value, ...page.items] : page.items;
    next.value = page.next;
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

function removed(id: string) {
  items.value = dropFromStatusList(items.value, id);
}

// Edits and deletes pushed live — an un-liked/un-bookmarked row also
// disappears, but that flows through the card's own `removed` emit.
useStatusEvents({
  onStatus: (s) => (items.value = patchStatusList(items.value, s)),
  onDelete: removed,
});

onMounted(() => load());
// Both routes reuse this component — reload when the feed prop flips.
watch(
  () => props.feed,
  () => {
    items.value = [];
    next.value = undefined;
    void load();
  },
);
</script>

<template>
  <main class="page">
    <h1>{{ t(`nav.${feed}`) }}</h1>
    <StatusCard v-for="s in items" :key="s.id" :status="s" @removed="removed" />
    <div
      v-if="loading && !items.length"
      role="status"
      :aria-label="t('common.loading')"
    >
      <StatusCardSkeleton v-for="n in 3" :key="n" />
    </div>
    <p v-if="!loading && !items.length && !error">{{ t("lists.empty") }}</p>
    <p v-if="error" role="alert">{{ error }}</p>
    <Button v-if="next" :loading="loading" @click="load(true)">
      {{ t("common.loadMore") }}
    </Button>
  </main>
</template>
