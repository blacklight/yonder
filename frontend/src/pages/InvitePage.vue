<script setup lang="ts">
import { useI18n } from "vue-i18n";
import AppLayout from "@/components/layout/AppLayout.vue";
import Card from "@/components/ui/Card.vue";

defineProps<{ code: string }>();
const { t } = useI18n();
</script>

<template>
  <AppLayout>
    <Card>
      <template #header>{{ t("invite.title") }}</template>
      <p>{{ t("invite.placeholder") }}</p>
      <p class="code">{{ code }}</p>
    </Card>
  </AppLayout>
</template>

<style scoped>
.code {
  font-family: var(--font-mono);
  color: var(--text-muted);
}
</style>
