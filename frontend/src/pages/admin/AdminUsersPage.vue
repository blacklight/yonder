<script setup lang="ts">
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import { ApiError } from "@/api/errors";
import { lookup } from "@/api/accounts";
import Input from "@/components/ui/Input.vue";
import Button from "@/components/ui/Button.vue";

// There is no admin account-list endpoint — moderation reaches users
// through reports; admins reach them by handle here.
const { t } = useI18n();
const router = useRouter();

const acct = ref("");
const error = ref("");
const pending = ref(false);

async function go() {
  const handle = acct.value.trim().replace(/^@/, "");
  if (!handle) return;
  pending.value = true;
  error.value = "";
  try {
    const account = await lookup(handle);
    await router.push({
      name: "admin-user",
      params: { id: account.id },
    });
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}
</script>

<template>
  <div class="admin-users">
    <h2>{{ t("admin.nav.users") }}</h2>
    <form class="admin-users__lookup" @submit.prevent="go">
      <Input
        v-model="acct"
        :label="t('admin.users.lookupLabel')"
        :placeholder="t('admin.users.lookupHint')"
        :error="error || undefined"
        inputmode="search"
        clearable
      />
      <Button type="submit" :loading="pending" :disabled="!acct.trim()">
        {{ t("admin.users.lookup") }}
      </Button>
    </form>
  </div>
</template>

<style scoped>
.admin-users__lookup {
  display: flex;
  align-items: end;
  gap: var(--space-3);
  max-width: 28rem;
}
</style>
