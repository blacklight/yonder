<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import { ApiError } from "@/api/errors";
import { restrictedLogout, restrictedSession } from "@/api/settings";
import type { RestrictedSession } from "@/api/entities";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";
import { useAuthStore } from "@/stores/auth";
import { EXPORT_KINDS as csvKinds, useExports } from "@/composables/useExports";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Skeleton from "@/components/ui/Skeleton.vue";

const { t } = useI18n();
const router = useRouter();
const auth = useAuthStore();

// Suspended-account surface — export controls and logout only; the
// router guards keep every other page unreachable for this session.
const session = ref<RestrictedSession | null>(null);
const loading = ref(true);
const loadError = ref("");

const {
  rows: exportRows,
  busy,
  error: exportError,
  errorArea,
  refresh: refreshExports,
  requestArchive,
  downloadArchive,
  downloadKind,
} = useExports();
const loggingOut = ref(false);

async function logout() {
  loggingOut.value = true;
  try {
    await restrictedLogout();
  } catch {
    // The token is revoked client-side either way — a failed call can
    // only mean the server-side row is already gone.
  } finally {
    auth.signOut();
    void router.push({ name: "sign-in" });
  }
}

onMounted(async () => {
  try {
    session.value = await restrictedSession();
  } catch (e) {
    loadError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
  void refreshExports();
});
</script>

<template>
  <div>
    <Card>
      <template #header>{{ t("settingsRestricted.title") }}</template>
      <div class="restricted">
        <div v-if="loading" class="restricted__skel">
          <Skeleton width="60%" />
          <Skeleton width="40%" height="0.75em" />
        </div>
        <p v-else-if="loadError" class="restricted__error" role="alert">
          {{ loadError }}
        </p>
        <template v-else-if="session">
          <p class="restricted__notice" role="alert">
            {{
              t("settingsRestricted.suspended", {
                acct: `@${session.account.acct}`,
              })
            }}
            <template v-if="session.suspended_at">
              {{
                t("settingsRestricted.suspendedSince", {
                  when: formatAbsoluteDate(session.suspended_at),
                })
              }}
            </template>
          </p>
          <p v-if="session.email" class="restricted__hint">
            {{ session.email }}
          </p>
          <p class="restricted__hint">{{ t("settingsRestricted.hint") }}</p>
          <p
            v-if="session.contact_email || session.contact_account"
            class="restricted__contact"
          >
            {{ t("settingsRestricted.contact") }}
            <a
              v-if="session.contact_email"
              :href="`mailto:${session.contact_email}`"
              >{{ session.contact_email }}</a
            >
            <template v-if="session.contact_account">
              ·
              <router-link :to="`/@${session.contact_account.acct}`"
                >@{{ session.contact_account.acct }}</router-link
              >
            </template>
          </p>
          <p v-else class="restricted__hint">
            {{ t("settingsRestricted.noContact") }}
          </p>
        </template>

        <section class="restricted__group">
          <h3 class="restricted__group-title">
            {{ t("settingsRestricted.groupCsv") }}
          </h3>
          <p
            v-if="exportError && errorArea === 'csv'"
            class="restricted__error"
            role="alert"
          >
            {{ exportError }}
          </p>
          <ul class="restricted__kinds">
            <li v-for="kind in csvKinds" :key="kind">
              <Button
                variant="secondary"
                size="sm"
                :loading="busy === kind"
                @click="downloadKind(kind)"
              >
                {{ t(`settingsData.kind.${kind}`) }}
              </Button>
            </li>
          </ul>
        </section>

        <section class="restricted__group">
          <h3 class="restricted__group-title">
            {{ t("settingsRestricted.groupArchive") }}
          </h3>
          <p
            v-if="exportError && errorArea === 'archive'"
            class="restricted__error"
            role="alert"
          >
            {{ exportError }}
          </p>
          <div class="restricted__actions">
            <Button
              variant="secondary"
              :loading="busy === 'archive'"
              @click="requestArchive"
            >
              {{ t("settingsRestricted.requestArchive") }}
            </Button>
          </div>
          <ul v-if="exportRows.length" class="restricted__list">
            <li v-for="row in exportRows" :key="row.id" class="restricted__row">
              <div class="restricted__main">
                <span>{{
                  t(`settingsData.state.${row.state}`, row.state)
                }}</span>
                <small class="restricted__meta">
                  <span :title="formatAbsoluteDate(row.created_at)">{{
                    formatRelativeDate(row.created_at)
                  }}</span>
                  <template v-if="row.expires_at">
                    ·
                    {{
                      t("settingsData.expires", {
                        when: formatRelativeDate(row.expires_at),
                      })
                    }}
                  </template>
                </small>
              </div>
              <Button
                v-if="row.state === 'done'"
                variant="secondary"
                size="sm"
                :loading="busy === row.id"
                @click="downloadArchive(row)"
              >
                {{ t("settingsRestricted.download") }}
              </Button>
            </li>
          </ul>
        </section>

        <section class="restricted__group">
          <Button variant="danger" :loading="loggingOut" @click="logout">
            {{ t("settingsRestricted.logout") }}
          </Button>
        </section>
      </div>
    </Card>
  </div>
</template>

<style scoped>
.restricted {
  display: flex;
  flex-direction: column;
  gap: 1.25rem;
}
.restricted__skel {
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
}
.restricted__notice {
  font-weight: 600;
  margin: 0;
}
.restricted__hint {
  color: var(--text-secondary);
  font-size: 0.875rem;
  margin: 0;
}
.restricted__error {
  color: var(--danger);
  margin: 0;
}
.restricted__contact {
  margin: 0;
}
.restricted__group {
  display: flex;
  flex-direction: column;
  gap: 0.6rem;
}
.restricted__group-title {
  margin: 0;
}
.restricted__kinds {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
  list-style: none;
  margin: 0;
  padding: 0;
}
.restricted__list {
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
  list-style: none;
  margin: 0;
  padding: 0;
}
.restricted__row {
  align-items: center;
  display: flex;
  gap: 0.75rem;
  justify-content: space-between;
}
.restricted__main {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.restricted__meta {
  color: var(--text-secondary);
}
.restricted__actions {
  display: flex;
  gap: 0.5rem;
}
</style>
