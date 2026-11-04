<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import { ApiError } from "@/api/errors";
import {
  addAlias,
  beginImportConnection,
  cancelMove,
  listAliases,
  listImports,
  moveAccount,
  moveState,
  removeAlias,
} from "@/api/settings";
import type { MoveState, TransferRow } from "@/api/entities";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";
import { importErrorKey } from "@/utils/importError";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";
import Skeleton from "@/components/ui/Skeleton.vue";
import SettingsNav from "@/components/settings/SettingsNav.vue";

const { t } = useI18n();
const route = useRoute();
const router = useRouter();

// ---------------------------------------------------------------------------
// Aliases (`alsoKnownAs`) — required before an outbound Move verifies.
// ---------------------------------------------------------------------------

const aliases = ref<string[]>([]);
const aliasesLoading = ref(true);
const aliasError = ref("");
const aliasForm = reactive({ acct: "" });
const addingAlias = ref(false);
const removingAlias = ref<string | null>(null);

async function refreshAliases() {
  try {
    aliases.value = await listAliases();
  } catch (e) {
    aliasError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    aliasesLoading.value = false;
  }
}

async function submitAlias() {
  const acct = aliasForm.acct.trim();
  if (!acct) return;
  aliasError.value = "";
  addingAlias.value = true;
  try {
    await addAlias(acct);
    aliasForm.acct = "";
    await refreshAliases();
  } catch (e) {
    aliasError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    addingAlias.value = false;
  }
}

async function removeAliasEntry(acct: string) {
  aliasError.value = "";
  removingAlias.value = acct;
  try {
    await removeAlias(acct);
    await refreshAliases();
  } catch (e) {
    aliasError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    removingAlias.value = null;
  }
}

// ---------------------------------------------------------------------------
// Move-out — password-confirmed redirect, cooldown + progress state.
// ---------------------------------------------------------------------------

const state = ref<MoveState | null>(null);
const stateError = ref("");
const moveForm = reactive({ acct: "", password: "" });
const moving = ref(false);
const moveError = ref("");
const moveNotice = ref("");
const cancelPassword = ref("");
const cancelling = ref(false);

const moveStatus = computed(
  () => state.value?.event?.state ?? (state.value?.moved ? "done" : "idle"),
);
const followers = computed(() => {
  const f = state.value?.followers;
  if (!f) return null;
  return { ...f, total: f.migrated + f.skipped + f.failed + f.pending };
});

async function refreshState() {
  try {
    state.value = await moveState();
  } catch (e) {
    stateError.value = e instanceof ApiError ? e.message : String(e);
  }
}

async function submitMove() {
  moveError.value = "";
  moveNotice.value = "";
  moving.value = true;
  try {
    await moveAccount(moveForm.acct.trim(), moveForm.password);
    moveForm.acct = "";
    moveForm.password = "";
    moveNotice.value = t("settingsMigration.moveStarted");
    await refreshState();
  } catch (e) {
    moveError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    moving.value = false;
  }
}

async function submitCancel() {
  moveError.value = "";
  cancelling.value = true;
  try {
    await cancelMove(cancelPassword.value);
    cancelPassword.value = "";
    moveNotice.value = t("settingsMigration.moveCancelled");
    await refreshState();
  } catch (e) {
    moveError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    cancelling.value = false;
  }
}

// ---------------------------------------------------------------------------
// Move-in wizard — authorize/import on the source *before* its Move.
// ---------------------------------------------------------------------------

const wizard = reactive({ step: 0, origin: "", error: "" });
const connecting = ref(false);
const recentImports = ref<TransferRow[]>([]);
const importsError = ref("");

// A move needs only the old account's follows. Activity import asks for
// its own consent on the data page, so it is not granted here.
const WIZARD_CATEGORIES = ["following"];

async function connectSource() {
  wizard.error = "";
  connecting.value = true;
  try {
    const { authorization_url } = await beginImportConnection(
      wizard.origin.trim(),
      WIZARD_CATEGORIES,
      "/settings/migration",
    );
    window.location.assign(authorization_url);
  } catch (e) {
    wizard.error = e instanceof ApiError ? e.message : String(e);
    connecting.value = false;
  }
}

onMounted(() => {
  // `return_to` brings the OAuth callback back here, not to the data
  // page — resume the wizard on the import step.
  if (route.query.connected === "1") {
    wizard.step = 1;
  } else if (typeof route.query.import_error === "string") {
    wizard.error = t(importErrorKey(route.query.import_error));
  }
  if (route.query.connected || route.query.import_error) {
    void router.replace({ query: {} });
  }
  void refreshAliases();
  void refreshState();
  void listImports()
    .then((rows) => (recentImports.value = rows.slice(0, 3)))
    .catch((e) => {
      importsError.value = e instanceof ApiError ? e.message : String(e);
    });
});
</script>

<template>
  <div>
    <SettingsNav />
    <Card>
      <template #header>{{ t("settingsMigration.title") }}</template>
      <div class="mig">
        <p class="mig__hint">{{ t("settingsMigration.intro") }}</p>

        <section class="mig__group">
          <h3 class="mig__group-title">
            {{ t("settingsMigration.groupMoveIn") }}
          </h3>
          <ol class="mig__steps">
            <li :class="{ 'mig__step--active': wizard.step === 0 }">
              <h4>{{ t("settingsMigration.stepConnectTitle") }}</h4>
              <p class="mig__hint">
                {{ t("settingsMigration.stepConnectHint") }}
              </p>
              <form class="mig__form" @submit.prevent="connectSource">
                <Input
                  v-model="wizard.origin"
                  :label="t('settingsMigration.originLabel')"
                  :placeholder="t('settingsMigration.originPlaceholder')"
                />
                <Button
                  type="submit"
                  :loading="connecting"
                  :disabled="connecting || !wizard.origin.trim()"
                >
                  {{ t("settingsMigration.stepConnect") }}
                </Button>
              </form>
              <p v-if="wizard.error" class="mig__error" role="alert">
                {{ wizard.error }}
              </p>
              <Button
                v-if="wizard.step === 0"
                variant="secondary"
                size="sm"
                @click="wizard.step = 1"
              >
                {{ t("settingsMigration.stepSkip") }}
              </Button>
            </li>
            <li :class="{ 'mig__step--active': wizard.step === 1 }">
              <h4>{{ t("settingsMigration.stepImportTitle") }}</h4>
              <p class="mig__hint">
                {{ t("settingsMigration.stepImportHint") }}
              </p>
              <router-link :to="{ name: 'settings-data' }">
                {{ t("settingsMigration.stepImportLink") }}
              </router-link>
              <div v-if="wizard.step === 1">
                <Button variant="secondary" size="sm" @click="wizard.step = 2">
                  {{ t("settingsMigration.stepNext") }}
                </Button>
              </div>
            </li>
            <li :class="{ 'mig__step--active': wizard.step === 2 }">
              <h4>{{ t("settingsMigration.stepReportTitle") }}</h4>
              <p class="mig__hint">
                {{ t("settingsMigration.stepReportHint") }}
              </p>
              <ul v-if="recentImports.length" class="mig__list">
                <li v-for="row in recentImports" :key="row.id" class="mig__row">
                  <span>
                    {{ t(`settingsData.kind.${row.kind}`, row.kind) }}
                  </span>
                  <small class="mig__hint">
                    {{ t(`settingsData.state.${row.state}`, row.state) }}
                    <template v-if="row.total != null">
                      ·
                      {{
                        t("settingsData.progress", {
                          done: row.processed,
                          total: row.total,
                          failed: row.failed,
                        })
                      }}
                    </template>
                  </small>
                </li>
              </ul>
              <p v-else class="mig__hint">
                {{ t("settingsMigration.reportEmpty") }}
              </p>
              <p v-if="importsError" class="mig__error" role="alert">
                {{ importsError }}
              </p>
              <div v-if="wizard.step === 2">
                <Button variant="secondary" size="sm" @click="wizard.step = 3">
                  {{ t("settingsMigration.stepNext") }}
                </Button>
              </div>
            </li>
            <li :class="{ 'mig__step--active': wizard.step === 3 }">
              <h4>{{ t("settingsMigration.stepAliasTitle") }}</h4>
              <p class="mig__hint">
                {{ t("settingsMigration.stepAliasHint") }}
              </p>
              <div v-if="wizard.step === 3">
                <Button variant="secondary" size="sm" @click="wizard.step = 4">
                  {{ t("settingsMigration.stepNext") }}
                </Button>
              </div>
            </li>
            <li :class="{ 'mig__step--active': wizard.step === 4 }">
              <h4>{{ t("settingsMigration.stepMoveTitle") }}</h4>
              <p class="mig__hint">{{ t("settingsMigration.stepMoveHint") }}</p>
            </li>
          </ol>
        </section>

        <section class="mig__group">
          <h3 class="mig__group-title">
            {{ t("settingsMigration.groupAliases") }}
          </h3>
          <p class="mig__hint">{{ t("settingsMigration.aliasesHint") }}</p>
          <p v-if="aliasError" class="mig__error" role="alert">
            {{ aliasError }}
          </p>
          <ul v-if="aliasesLoading" class="mig__list">
            <li class="mig__row"><Skeleton width="40%" /></li>
          </ul>
          <ul v-else-if="aliases.length" class="mig__list">
            <li v-for="acct in aliases" :key="acct" class="mig__row">
              <code class="mig__alias">{{ acct }}</code>
              <Button
                variant="secondary"
                size="sm"
                :loading="removingAlias === acct"
                @click="removeAliasEntry(acct)"
                >{{ t("settingsMigration.aliasRemove") }}</Button
              >
            </li>
          </ul>
          <p v-else class="mig__hint">{{ t("settingsMigration.noAliases") }}</p>
          <form class="mig__form" @submit.prevent="submitAlias">
            <Input
              v-model="aliasForm.acct"
              :label="t('settingsMigration.aliasLabel')"
              :placeholder="t('settingsMigration.aliasPlaceholder')"
            />
            <Button
              type="submit"
              variant="secondary"
              :loading="addingAlias"
              :disabled="addingAlias || !aliasForm.acct.trim()"
            >
              {{ t("settingsMigration.aliasAdd") }}
            </Button>
          </form>
        </section>

        <section class="mig__group">
          <h3 class="mig__group-title">
            {{ t("settingsMigration.groupMoveOut") }}
          </h3>
          <p v-if="stateError" class="mig__error" role="alert">
            {{ stateError }}
          </p>
          <div v-if="state" class="mig__state">
            <p v-if="moveStatus === 'idle'" class="mig__hint">
              {{ t("settingsMigration.stateIdle") }}
            </p>
            <template v-else>
              <p class="mig__state-line">
                {{ t(`settingsMigration.state.${moveStatus}`, moveStatus) }}
                <template v-if="state.target">→ {{ state.target }}</template>
              </p>
              <p v-if="followers?.total" class="mig__state-line">
                {{
                  t("settingsMigration.followerProgress", {
                    done: followers.migrated,
                    total: followers.total,
                  })
                }}
              </p>
              <p v-if="followers?.failed" class="mig__error" role="alert">
                {{
                  t("settingsMigration.followerFailed", { n: followers.failed })
                }}
              </p>
            </template>
            <p v-if="state.cooldown_until" class="mig__warn" role="note">
              {{
                t("settingsMigration.cooldown", {
                  when: formatRelativeDate(state.cooldown_until),
                  abs: formatAbsoluteDate(state.cooldown_until),
                })
              }}
            </p>
          </div>
          <p class="mig__hint">{{ t("settingsMigration.moveHint") }}</p>
          <p v-if="moveNotice" class="mig__notice" role="status">
            {{ moveNotice }}
          </p>
          <p v-if="moveError" class="mig__error" role="alert">
            {{ moveError }}
          </p>
          <form
            v-if="!state?.moved"
            class="mig__form"
            @submit.prevent="submitMove"
          >
            <Input
              v-model="moveForm.acct"
              :label="t('settingsMigration.targetLabel')"
              :placeholder="t('settingsMigration.targetPlaceholder')"
            />
            <Input
              v-model="moveForm.password"
              type="password"
              :label="t('settingsMigration.passwordLabel')"
            />
            <Button
              type="submit"
              variant="danger"
              :loading="moving"
              :disabled="moving || !moveForm.acct.trim() || !moveForm.password"
            >
              {{ t("settingsMigration.moveSubmit") }}
            </Button>
          </form>
          <form v-else class="mig__form" @submit.prevent="submitCancel">
            <Input
              v-model="cancelPassword"
              type="password"
              :label="t('settingsMigration.passwordLabel')"
            />
            <Button
              type="submit"
              variant="secondary"
              :loading="cancelling"
              :disabled="cancelling || !cancelPassword"
            >
              {{ t("settingsMigration.cancelSubmit") }}
            </Button>
          </form>
        </section>
      </div>
    </Card>
  </div>
</template>

<style scoped>
.mig {
  display: flex;
  flex-direction: column;
  gap: 1.25rem;
}
.mig__group {
  display: flex;
  flex-direction: column;
  gap: 0.6rem;
}
.mig__group-title {
  margin: 0;
}
.mig__hint {
  color: var(--text-secondary);
  font-size: 0.875rem;
  margin: 0;
}
.mig__error {
  color: var(--danger);
  margin: 0;
}
.mig__warn {
  color: var(--warning, var(--danger));
  font-size: 0.875rem;
  margin: 0;
}
.mig__notice {
  color: var(--success, var(--accent));
  margin: 0;
}
.mig__steps {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
  list-style: none;
  margin: 0;
  padding-inline-start: 1.25rem;
}
.mig__steps h4 {
  margin: 0 0 0.25rem;
}
.mig__step--active h4 {
  color: var(--accent);
}
.mig__form {
  display: flex;
  flex-direction: column;
  gap: 0.6rem;
  margin: 0.5rem 0;
}
.mig__list {
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
  list-style: none;
  margin: 0;
  padding: 0;
}
.mig__row {
  align-items: center;
  display: flex;
  gap: 0.75rem;
  justify-content: space-between;
}
.mig__state-line {
  margin: 0;
}
</style>
