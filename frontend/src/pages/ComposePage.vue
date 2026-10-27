<script setup lang="ts">
import Card from "@/components/ui/Card.vue";
import ComposeBox from "@/components/compose/ComposeBox.vue";
import { useI18n } from "vue-i18n";

const { t } = useI18n();
</script>

<template>
  <Card>
    <template #header>{{ t("compose.title") }}</template>
    <ComposeBox class="compose-page__box" />
  </Card>
</template>

<style scoped>
/* Standalone page only: give the form most of the viewport. `dvh`
   tracks mobile browser chrome (vh fallback for old engines); the
   floor keeps the form usable on short screens — the page scrolls
   rather than crushing the textarea. Embedded composers keep their
   natural compact size. */
.compose-page__box {
  height: 80vh;
  height: 80dvh;
  min-height: 26rem;
}
</style>
