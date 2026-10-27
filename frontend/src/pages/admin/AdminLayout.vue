<script setup lang="ts">
import { useI18n } from "vue-i18n";

const { t } = useI18n();
</script>

<template>
  <main class="page admin">
    <h1>{{ t("nav.admin") }}</h1>
    <router-view />
  </main>
</template>
