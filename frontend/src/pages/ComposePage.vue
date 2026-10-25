<script setup lang="ts">
import Card from "@/components/ui/Card.vue";
import ComposeBox from "@/components/compose/ComposeBox.vue";
import { useI18n } from "vue-i18n";

const { t } = useI18n();
</script>

<template>
  <Card>
    <template #header>{{ t("compose.title") }}</template>
    <ComposeBox />
  </Card>
</template>
