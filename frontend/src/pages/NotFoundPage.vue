<script setup lang="ts">
import { useI18n } from "vue-i18n";
import Card from "@/components/ui/Card.vue";

const { t } = useI18n();
</script>

<template>
  <Card>
    <template #header>{{ t("not_found.title") }}</template>
    {{ t("not_found.message") }}
  </Card>
</template>
