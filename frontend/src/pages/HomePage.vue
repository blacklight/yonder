<script setup lang="ts">
import { useI18n } from "vue-i18n";
import AppLayout from "@/components/layout/AppLayout.vue";
import Card from "@/components/ui/Card.vue";

const { t } = useI18n();
</script>

<template>
  <AppLayout>
    <Card>
      <template #header>{{ t("home.welcome") }}</template>
      {{ t("home.placeholder") }}
    </Card>
  </AppLayout>
</template>
