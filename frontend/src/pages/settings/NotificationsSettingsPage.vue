<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { usePushNotifications } from "@/composables/usePushNotifications";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Skeleton from "@/components/ui/Skeleton.vue";
import SettingsNav from "@/components/settings/SettingsNav.vue";

const { t } = useI18n();
const push = usePushNotifications();
const {
  state: pushState,
  error: pushError,
  isSupported: pushSupported,
  isSubscribed: pushSubscribed,
} = push;

/** The `data.alerts` types surfaced as toggles (backend ALERT_TYPES). */
const ALERT_TYPES = [
  "mention",
  "favourite",
  "reblog",
  "quote",
  "follow",
  "follow_request",
  "poll",
  "status",
  "update",
] as const;

const POLICIES = ["all", "followed", "follower", "none"] as const;

const loading = ref(true);
const saving = ref(false);

const alerts = ref<Record<string, boolean>>({});
const policy = ref("all");

const stateLabel = computed(() =>
  t(`settingsNotifications.status.${pushState.value}`),
);

async function togglePush() {
  if (pushSubscribed.value) {
    await push.disable();
  } else {
    await push.enable();
  }
  syncFromSubscription();
}

function syncFromSubscription() {
  const sub = push.subscription.value;
  alerts.value = { ...(sub?.alerts ?? {}) };
  policy.value = sub?.policy ?? "all";
}

async function onAlertChange() {
  if (!pushSubscribed.value) return;
  saving.value = true;
  await push.update({ ...alerts.value });
  saving.value = false;
}

async function onPolicyChange() {
  if (!pushSubscribed.value) return;
  saving.value = true;
  await push.update(undefined, policy.value);
  saving.value = false;
}

onMounted(async () => {
  await push.load();
  syncFromSubscription();
  loading.value = false;
});
</script>

<template>
  <div>
    <SettingsNav />
    <Card>
      <template #header>{{ t("settingsNotifications.title") }}</template>
      <div class="notif-settings">
        <Skeleton v-if="loading" />

        <template v-else>
          <section class="notif-settings__group">
            <h3 class="notif-settings__group-title">
              {{ t("settingsNotifications.push.title") }}
            </h3>
            <p class="notif-settings__hint">
              {{ t("settingsNotifications.push.hint") }}
            </p>
            <div v-if="pushSupported" class="notif-settings__row">
              <span class="notif-settings__status">{{ stateLabel }}</span>
              <Button
                size="sm"
                :disabled="
                  pushState === 'loading' ||
                  pushState === 'denied' ||
                  pushState === 'unavailable'
                "
                @click="togglePush"
              >
                {{
                  pushSubscribed
                    ? t("settingsNotifications.disable")
                    : t("settingsNotifications.enable")
                }}
              </Button>
            </div>
            <p v-else class="notif-settings__hint" role="note">
              {{ t("settingsNotifications.status.unsupported") }}
            </p>
            <p v-if="pushError" class="notif-settings__error" role="alert">
              {{ pushError }}
            </p>
          </section>

          <template v-if="pushSubscribed">
            <section class="notif-settings__group">
              <h3 class="notif-settings__group-title">
                {{ t("settingsNotifications.alerts.title") }}
              </h3>
              <p class="notif-settings__hint">
                {{ t("settingsNotifications.alerts.hint") }}
              </p>
              <label
                v-for="kind in ALERT_TYPES"
                :key="kind"
                class="notif-settings__flag"
              >
                <input
                  v-model="alerts[kind]"
                  type="checkbox"
                  :disabled="saving"
                  @change="onAlertChange"
                />
                <span>{{ t(`settingsNotifications.types.${kind}`) }}</span>
              </label>
            </section>

            <section class="notif-settings__group">
              <h3 class="notif-settings__group-title">
                {{ t("settingsNotifications.policy.title") }}
              </h3>
              <label class="notif-settings__row">
                <span class="notif-settings__label">
                  {{ t("settingsNotifications.policy.fromLabel") }}
                </span>
                <select
                  v-model="policy"
                  class="notif-settings__select"
                  :disabled="saving"
                  @change="onPolicyChange"
                >
                  <option v-for="p in POLICIES" :key="p" :value="p">
                    {{ t(`settingsNotifications.policy.options.${p}`) }}
                  </option>
                </select>
              </label>
            </section>
          </template>
        </template>
      </div>
    </Card>
  </div>
</template>

<style scoped>
.notif-settings {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
}
.notif-settings__group {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.notif-settings__group-title {
  font-size: var(--text-sm);
  font-weight: 600;
  color: var(--text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
}
.notif-settings__hint {
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.notif-settings__row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
}
.notif-settings__status {
  font-size: var(--text-sm);
}
.notif-settings__error {
  color: var(--color-error, #b00020);
  font-size: var(--text-sm);
}
.notif-settings__flag {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--text-sm);
  cursor: pointer;
}
.notif-settings__label {
  font-size: var(--text-sm);
}
.notif-settings__select {
  min-width: 12rem;
}
</style>
