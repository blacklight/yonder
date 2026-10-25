<script setup lang="ts">
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import { generateVapidKeys } from "@/api/adminWebpush";
import Button from "@/components/ui/Button.vue";
import ConfirmDialog from "@/components/admin/ConfirmDialog.vue";
import AdminSettingsPage from "./AdminSettingsPage.vue";

const { t } = useI18n();
const pending = ref(false);
const publicKey = ref("");
const error = ref("");
const confirming = ref(false);

async function generate() {
  confirming.value = false;
  pending.value = true;
  error.value = "";
  try {
    publicKey.value = (await generateVapidKeys()).public_key;
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}
</script>

<template>
  <AdminSettingsPage title-key="admin.nav.http" :sections="['http']">
    <section class="webpush">
      <h3 class="webpush__title">{{ t("admin.webpush.title") }}</h3>
      <p class="webpush__hint">{{ t("admin.webpush.rotateHint") }}</p>
      <Button :loading="pending" @click="confirming = true">
        {{ t("admin.webpush.generate") }}
      </Button>
      <p v-if="publicKey" class="webpush__key">
        {{ t("admin.webpush.publicKey") }}:
        <code>{{ publicKey }}</code>
      </p>
      <p v-if="error" class="webpush__error" role="alert">{{ error }}</p>
    </section>

    <ConfirmDialog
      v-if="confirming"
      :title="t('admin.webpush.generate')"
      :message="t('admin.webpush.rotateHint')"
      danger
      :pending="pending"
      @confirm="generate"
      @cancel="confirming = false"
    />
  </AdminSettingsPage>
</template>

<style scoped>
.webpush {
  margin-top: var(--space-6);
}
.webpush__title {
  margin: 0 0 var(--space-2);
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-xs);
  font-weight: 700;
  letter-spacing: 0.05em;
  text-transform: uppercase;
  color: var(--text-muted);
}
.webpush__hint {
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.webpush__key {
  font-size: var(--text-xs);
  word-break: break-all;
}
.webpush__error {
  font-size: var(--text-xs);
  color: var(--danger);
}
</style>
