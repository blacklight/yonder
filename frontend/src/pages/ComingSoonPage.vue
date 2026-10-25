<script setup lang="ts">
import { useI18n } from "vue-i18n";
import AppLayout from "@/components/layout/AppLayout.vue";
import Card from "@/components/ui/Card.vue";

defineProps<{ titleKey: string }>();
const { t } = useI18n();
</script>

<template>
  <AppLayout>
    <Card>
      <template #header>{{ t(titleKey) }}</template>
      {{ t("pages.comingSoon") }}
    </Card>
  </AppLayout>
</template>
