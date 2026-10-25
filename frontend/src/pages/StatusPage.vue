<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { request } from "@/api/client";
import type { Status } from "@/api/entities";
import StatusCard from "@/components/status/StatusCard.vue";
import { ApiError } from "@/api/errors";

const props = defineProps<{ id: string }>();
const { t } = useI18n();
const status = ref<Status | null>(null);
const error = ref<number | null>(null);

onMounted(async () => {
  try {
    status.value = await request<Status>({ path: `/statuses/${props.id}` });
  } catch (e) {
    error.value = e instanceof ApiError ? e.status : 500;
  }
});
</script>

<template>
  <main class="page">
    <StatusCard v-if="status" :status="status" />
    <p v-else-if="error === 404">{{ t("status.notFound") }}</p>
    <p v-else-if="error">{{ t("status.loadError") }}</p>
    <p v-else aria-busy="true">{{ t("common.loading") }}</p>
  </main>
</template>
