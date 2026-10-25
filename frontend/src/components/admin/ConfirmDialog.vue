<script setup lang="ts">
import Modal from "@/components/ui/Modal.vue";
import Button from "@/components/ui/Button.vue";
import { useI18n } from "vue-i18n";

defineProps<{
  title: string;
  message: string;
  confirmLabel?: string;
  danger?: boolean;
  pending?: boolean;
}>();
const emit = defineEmits<{ confirm: []; cancel: [] }>();
const { t } = useI18n();
</script>

<template>
  <Modal :title="title" @close="emit('cancel')">
    <p>{{ message }}</p>
    <template #footer>
      <Button variant="secondary" :disabled="pending" @click="emit('cancel')">
        {{ t("ui.cancel") }}
      </Button>
      <Button
        :variant="danger ? 'danger' : 'primary'"
        :loading="pending"
        @click="emit('confirm')"
      >
        {{ confirmLabel ?? t("ui.confirm") }}
      </Button>
    </template>
  </Modal>
</template>
