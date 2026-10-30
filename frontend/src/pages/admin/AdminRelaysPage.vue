<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import {
  addRelay,
  authorizeRelay,
  deleteRelay,
  listRelays,
  type AdminRelay,
} from "@/api/adminRelays";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import Spinner from "@/components/ui/Spinner.vue";
import ConfirmDialog from "@/components/admin/ConfirmDialog.vue";

const { t } = useI18n();

const relays = ref<AdminRelay[]>([]);
const loading = ref(true);
const error = ref("");
const pending = ref(false);
const inboxUrl = ref("");
const removing = ref<AdminRelay | null>(null);

onMounted(load);

async function load() {
  loading.value = true;
  error.value = "";
  try {
    relays.value = await listRelays();
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

async function add() {
  pending.value = true;
  error.value = "";
  try {
    relays.value = [await addRelay(inboxUrl.value.trim()), ...relays.value];
    inboxUrl.value = "";
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

async function authorize(relay: AdminRelay) {
  pending.value = true;
  error.value = "";
  try {
    const updated = await authorizeRelay(relay.id);
    relays.value = relays.value.map((r) => (r.id === relay.id ? updated : r));
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}

async function remove() {
  const relay = removing.value;
  removing.value = null;
  if (!relay) return;
  pending.value = true;
  error.value = "";
  try {
    await deleteRelay(relay.id);
    relays.value = relays.value.filter((r) => r.id !== relay.id);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    pending.value = false;
  }
}
</script>

<template>
  <div class="relays">
    <h2>{{ t("admin.nav.relays") }}</h2>
    <p class="relays__hint">{{ t("admin.relays.hint") }}</p>

    <form class="relays__form" @submit.prevent="add">
      <Input
        v-model="inboxUrl"
        :label="t('admin.relays.inboxUrl')"
        placeholder="https://relay.example/inbox"
        inputmode="url"
      />
      <Button type="submit" :loading="pending" :disabled="!inboxUrl.trim()">
        {{ t("admin.relays.add") }}
      </Button>
    </form>

    <p v-if="loading" class="relays__status"><Spinner /></p>
    <p v-else-if="relays.length === 0" class="relays__status">
      {{ t("admin.relays.empty") }}
    </p>
    <ul v-else class="relays__list">
      <li v-for="r in relays" :key="r.id" class="relays__item">
        <code class="relays__url">{{ r.inbox_url }}</code>
        <span
          class="relays__state"
          :class="{ 'relays__state--accepted': r.state === 'accepted' }"
        >
          {{ t(`admin.relays.states.${r.state}`) }}
        </span>
        <span class="relays__buttons">
          <Button
            v-if="r.state === 'pending'"
            variant="secondary"
            size="sm"
            :disabled="pending"
            @click="authorize(r)"
          >
            {{ t("admin.relays.authorize") }}
          </Button>
          <Button
            variant="danger"
            size="sm"
            :disabled="pending"
            @click="removing = r"
          >
            {{ t("admin.relays.remove") }}
          </Button>
        </span>
      </li>
    </ul>
    <p v-if="error" class="relays__error" role="alert">{{ error }}</p>

    <ConfirmDialog
      v-if="removing"
      :title="t('admin.relays.remove')"
      :message="t('admin.relays.confirmRemove', { url: removing.inbox_url })"
      danger
      :pending="pending"
      @confirm="remove"
      @cancel="removing = null"
    />
  </div>
</template>

<style scoped>
.relays__hint {
  font-size: var(--text-xs);
  color: var(--text-muted);
}
.relays__form {
  display: flex;
  align-items: end;
  gap: var(--space-3);
  max-width: 32rem;
  margin-bottom: var(--space-4);
}
.relays__list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.relays__item {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: var(--space-3);
  padding: var(--space-3) 0;
  border-top: 1px solid var(--border);
}
.relays__url {
  flex: 1;
  min-width: 0;
  word-break: break-all;
  font-size: var(--font-sm, 0.8125rem);
}
.relays__state {
  font-size: var(--text-xs);
  padding: 0 var(--space-2);
  border-radius: var(--radius-sm);
  background: var(--bg-raised);
  color: var(--text-muted);
}
.relays__state--accepted {
  background: var(--accent-soft, var(--bg-raised));
  color: var(--accent-link);
}
.relays__buttons {
  display: inline-flex;
  gap: var(--space-2);
}
.relays__status {
  color: var(--text-muted);
}
.relays__error {
  color: var(--danger);
}
</style>
