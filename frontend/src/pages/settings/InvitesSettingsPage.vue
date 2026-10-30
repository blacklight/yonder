<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import { createInvite, listInvites, revokeInvite } from "@/api/settings";
import { getInstance } from "@/api/instance";
import type { Invite } from "@/api/entities";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";
import { useAuthStore } from "@/stores/auth";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Skeleton from "@/components/ui/Skeleton.vue";
import SettingsNav from "@/components/settings/SettingsNav.vue";

const { t } = useI18n();
const auth = useAuthStore();

// `registrations.invites_enabled` gates invite creation for non-staff
// users (staff may always create). `null` while the instance entity is
// still loading — the form shows optimistically and the backend still
// enforces the policy server-side.
const invitesEnabled = ref<boolean | null>(null);
const canCreate = computed(
  () => auth.isStaff || invitesEnabled.value !== false,
);

const maxUseOptions = ["", 1, 5, 10, 25, 50, 100] as const;
const expiryOptions = [
  { value: "", label: "expiryNever" },
  { value: 86400, label: "expiryDay" },
  { value: 604800, label: "expiryWeek" },
  { value: 2592000, label: "expiryMonth" },
  { value: 7776000, label: "expiryQuarter" },
  { value: 31536000, label: "expiryYear" },
] as const;

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

const form = reactive({
  maxUses: "" as "" | number,
  expiry: "" as "" | number,
});
const creating = ref(false);
const createError = ref("");

async function submit() {
  createError.value = "";
  creating.value = true;
  try {
    const body: { max_uses?: number; expires_in?: number } = {};
    if (form.maxUses) body.max_uses = Number(form.maxUses);
    if (form.expiry) body.expires_in = Number(form.expiry);
    const created = await createInvite(body);
    invites.value = [created, ...invites.value];
    form.maxUses = "";
    form.expiry = "";
  } catch (e) {
    createError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    creating.value = false;
  }
}

// ---------------------------------------------------------------------------
// List + revoke
// ---------------------------------------------------------------------------

const invites = ref<Invite[]>([]);
const loading = ref(true);
const listError = ref("");

// Two-click revoke: the first click arms the row, the second (danger)
// click calls the API.
const confirming = ref<string | null>(null);
const revoking = ref(false);
const copiedId = ref<string | null>(null);

async function refresh() {
  listError.value = "";
  try {
    invites.value = await listInvites();
  } catch (e) {
    listError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

// The entity carries the fully rendered `url`; fall back to building
// it from the code so older payloads still copy.
function inviteUrl(invite: Invite) {
  return invite.url ?? `${window.location.origin}/invite/${invite.code}`;
}

function isExpired(invite: Invite) {
  return invite.expires_at
    ? new Date(invite.expires_at).getTime() <= Date.now()
    : false;
}

function isExhausted(invite: Invite) {
  return (
    invite.max_uses != null &&
    invite.uses != null &&
    invite.uses >= invite.max_uses
  );
}

let copiedTimer: ReturnType<typeof setTimeout> | undefined;
async function copyInvite(invite: Invite) {
  await navigator.clipboard.writeText(inviteUrl(invite));
  copiedId.value = invite.id;
  clearTimeout(copiedTimer);
  copiedTimer = setTimeout(() => (copiedId.value = null), 2000);
}

async function revoke(id: string) {
  revoking.value = true;
  listError.value = "";
  try {
    await revokeInvite(id);
    invites.value = invites.value.filter((iv) => iv.id !== id);
  } catch (e) {
    listError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    revoking.value = false;
    confirming.value = null;
  }
}

onMounted(async () => {
  void refresh();
  try {
    const instance = await getInstance();
    invitesEnabled.value = instance.registrations?.invites_enabled ?? null;
  } catch {
    /* unknown policy — the form degrades to the optimistic path */
  }
});
</script>

<template>
  <div>
    <SettingsNav />
    <Card>
      <template #header>{{ t("settingsInvites.title") }}</template>
      <div class="invites">
        <p class="invites__hint">{{ t("settingsInvites.intro") }}</p>

        <section class="invites__group">
          <h3 class="invites__group-title">
            {{ t("settingsInvites.groupCreate") }}
          </h3>
          <p v-if="!canCreate" class="invites__hint" role="note">
            {{ t("settingsInvites.disabled") }}
          </p>
          <form v-else class="invites__form" @submit.prevent="submit">
            <label class="invites__field">
              <span>{{ t("settingsInvites.maxUsesLabel") }}</span>
              <select v-model="form.maxUses">
                <option
                  v-for="opt in maxUseOptions"
                  :key="String(opt)"
                  :value="opt"
                >
                  {{ opt === "" ? t("settingsInvites.maxUsesUnlimited") : opt }}
                </option>
              </select>
            </label>
            <label class="invites__field">
              <span>{{ t("settingsInvites.expiryLabel") }}</span>
              <select v-model="form.expiry">
                <option
                  v-for="opt in expiryOptions"
                  :key="opt.value"
                  :value="opt.value"
                >
                  {{ t(`settingsInvites.${opt.label}`) }}
                </option>
              </select>
            </label>
            <p v-if="createError" class="invites__error" role="alert">
              {{ createError }}
            </p>
            <div>
              <Button type="submit" :loading="creating" :disabled="creating">
                {{ t("settingsInvites.create") }}
              </Button>
            </div>
          </form>
        </section>

        <section class="invites__group">
          <h3 class="invites__group-title">
            {{ t("settingsInvites.groupList") }}
          </h3>
          <p v-if="listError" class="invites__error" role="alert">
            {{ listError }}
          </p>
          <ul
            v-if="loading"
            class="invites__list"
            role="status"
            :aria-label="t('common.loading')"
          >
            <li v-for="n in 2" :key="n" class="invites__row">
              <div class="invites__main invites__skel">
                <Skeleton width="45%" />
                <Skeleton width="60%" height="0.75em" />
              </div>
            </li>
          </ul>
          <ul v-else-if="invites.length" class="invites__list">
            <li v-for="invite in invites" :key="invite.id" class="invites__row">
              <div class="invites__main">
                <span class="invites__name">
                  <code class="invites__url">{{ inviteUrl(invite) }}</code>
                  <span v-if="isExpired(invite)" class="invites__badge">{{
                    t("settingsInvites.expired")
                  }}</span>
                  <span
                    v-else-if="isExhausted(invite)"
                    class="invites__badge"
                    >{{ t("settingsInvites.exhausted") }}</span
                  >
                </span>
                <small class="invites__meta">
                  <span v-if="invite.max_uses != null">{{
                    t("settingsInvites.uses", {
                      used: invite.uses ?? 0,
                      max: invite.max_uses,
                    })
                  }}</span>
                  <span v-else>{{
                    t("settingsInvites.usesUnlimited", {
                      used: invite.uses ?? 0,
                    })
                  }}</span>

                  <span v-if="invite.created_at">
                    ·
                    <span :title="formatAbsoluteDate(invite.created_at)">{{
                      t("settingsInvites.created", {
                        when: formatRelativeDate(invite.created_at),
                      })
                    }}</span>
                  </span>
                  <span v-if="invite.expires_at">
                    ·
                    <span :title="formatAbsoluteDate(invite.expires_at)">{{
                      t("settingsInvites.expires", {
                        when: formatRelativeDate(invite.expires_at),
                      })
                    }}</span>
                  </span>
                  <span v-else> · {{ t("settingsInvites.neverExpires") }}</span>
                </small>
              </div>
              <div class="invites__actions">
                <Button
                  variant="secondary"
                  size="sm"
                  @click="copyInvite(invite)"
                >
                  {{
                    copiedId === invite.id
                      ? t("settingsInvites.copied")
                      : t("settingsInvites.copy")
                  }}
                </Button>
                <template v-if="confirming === invite.id">
                  <Button
                    variant="danger"
                    size="sm"
                    :loading="revoking"
                    @click="revoke(invite.id)"
                    >{{ t("settingsInvites.revokeConfirm") }}</Button
                  >
                  <Button
                    variant="secondary"
                    size="sm"
                    @click="confirming = null"
                    >{{ t("settingsInvites.cancel") }}</Button
                  >
                </template>
                <Button
                  v-else
                  variant="secondary"
                  size="sm"
                  @click="confirming = invite.id"
                  >{{ t("settingsInvites.revoke") }}</Button
                >
              </div>
            </li>
          </ul>
          <p v-else-if="!loading" class="invites__hint">
            {{ t("settingsInvites.empty") }}
          </p>
        </section>
      </div>
    </Card>
  </div>
</template>

<style scoped>
.invites {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
}
.invites__group {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.invites__group-title {
  margin: 0;
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-sm);
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--text-muted);
}
.invites__form {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  max-width: 26rem;
}
.invites__field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  font-size: var(--text-sm);
  max-width: 14rem;
}
.invites__hint {
  margin: 0;
  font-size: var(--text-sm);
  color: var(--text-muted);
}
.invites__error {
  margin: 0;
  color: var(--danger);
}
.invites__list {
  display: flex;
  flex-direction: column;
  margin: 0;
  padding: 0;
  list-style: none;
}
.invites__row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  padding: var(--space-2) 0;
  border-top: 1px solid var(--border);
  flex-wrap: wrap;
}
.invites__skel {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  padding: var(--space-1) 0;
}
.invites__main {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  min-width: 0;
}
.invites__name {
  font-weight: 600;
}
.invites__url {
  font-weight: 400;
  word-break: break-all;
}
.invites__badge {
  padding: 0 var(--space-2);
  border-radius: var(--radius-full);
  background: var(--accent-soft);
  color: var(--accent);
  font-size: var(--text-xs);
  font-weight: 700;
  text-transform: uppercase;
}
.invites__meta {
  color: var(--text-muted);
}
.invites__actions {
  display: flex;
  gap: var(--space-2);
}
@media (max-width: 768px) {
  .invites__row {
    align-items: flex-start;
    flex-direction: column;
  }
}
select {
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-2) var(--space-3);
}
</style>
