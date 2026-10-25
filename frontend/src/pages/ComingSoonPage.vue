<script setup lang="ts">
import { useI18n } from "vue-i18n";
import Card from "@/components/ui/Card.vue";

defineProps<{ titleKey: string }>();
const { t } = useI18n();
</script>

<template>
  <Card>
    <template #header>{{ t(titleKey) }}</template>
    {{ t("pages.comingSoon") }}
  </Card>
</template>
