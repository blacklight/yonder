<script setup lang="ts">
import { onMounted } from "vue";
import { useTheme } from "@/composables/useTheme";

const { apply } = useTheme();
onMounted(apply);
</script>

<template>
  <router-view />
</template>
