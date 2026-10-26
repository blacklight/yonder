<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import {
  changePassword,
  deleteAccount,
  disableTwoFactor,
  listAuthorizedApps,
  listSessions,
  regenerateBackupCodes,
  revokeAuthorizedApp,
  revokeSession,
  totpConfirm,
  totpDelete,
  totpSetup,
  twoFactorStatus,
  webauthnDelete,
  webauthnRegister,
  webauthnRegisterBegin,
  webauthnRename,
} from "@/api/settings";
import type {
  AuthorizedApp,
  SessionEntity,
  TwoFactorStatus,
} from "@/api/entities";
import { createSecurityKey, webauthnSupported } from "@/utils/webauthn";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";
import { useAuthStore } from "@/stores/auth";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";
import SettingsNav from "@/components/settings/SettingsNav.vue";
import { FontAwesomeIcon } from "@/icons";

const { t } = useI18n();
const router = useRouter();
const auth = useAuthStore();

const MIN_PASSWORD = 8;

// ---------------------------------------------------------------------------
// Password change
// ---------------------------------------------------------------------------

const pwCurrent = ref("");
const pwNew = ref("");
const pwConfirm = ref("");
const pwPending = ref(false);
const pwError = ref("");
const pwSaved = ref(false);

async function savePassword() {
  pwError.value = "";
  pwSaved.value = false;
  if (pwNew.value.length < MIN_PASSWORD) {
    pwError.value = t("settingsSecurity.passwordTooShort");
    return;
  }
  if (pwNew.value !== pwConfirm.value) {
    pwError.value = t("settingsSecurity.passwordMismatch");
    return;
  }
  pwPending.value = true;
  try {
    await changePassword(pwCurrent.value, pwNew.value);
    pwCurrent.value = "";
    pwNew.value = "";
    pwConfirm.value = "";
    pwSaved.value = true;
  } catch (e) {
    pwError.value = e instanceof Error ? e.message : String(e);
  } finally {
    pwPending.value = false;
  }
}

// ---------------------------------------------------------------------------
// Two-factor
// ---------------------------------------------------------------------------

const twofa = ref<TwoFactorStatus | null>(null);

// TOTP enrollment — the secret/QR are shown until the user confirms or
// cancels; a confirmed enrollment lands on the backup-code reveal.
const totpWizard = ref<{ secret: string; qr_data_uri: string } | null>(null);
const totpCode = ref("");
const totpPending = ref(false);
const totpError = ref("");

// One-time backup-code reveal (enrollment + regeneration).
const revealedCodes = ref<string[] | null>(null);

// The shared password gate for destructive actions — exactly one is
// open at a time.
const gate = ref<string | null>(null);
const gatePassword = ref("");
const gatePending = ref(false);
const gateError = ref("");

function openGate(which: string) {
  gate.value = which;
  gatePassword.value = "";
  gateError.value = "";
}

function closeGate() {
  gate.value = null;
  gatePassword.value = "";
  gateError.value = "";
}

async function refreshTwoFactor() {
  twofa.value = await twoFactorStatus();
}

async function startTotp() {
  totpPending.value = true;
  totpError.value = "";
  try {
    const data = await totpSetup();
    totpWizard.value = { secret: data.secret, qr_data_uri: data.qr_data_uri };
    totpCode.value = "";
  } catch (e) {
    totpError.value = e instanceof Error ? e.message : String(e);
  } finally {
    totpPending.value = false;
  }
}

async function confirmTotp() {
  totpPending.value = true;
  totpError.value = "";
  try {
    const data = await totpConfirm(totpCode.value.trim());
    totpWizard.value = null;
    totpCode.value = "";
    revealedCodes.value = data.backup_codes;
    await refreshTwoFactor();
  } catch (e) {
    totpError.value = e instanceof Error ? e.message : String(e);
  } finally {
    totpPending.value = false;
  }
}

// WebAuthn — the ceremony is split in two steps because
// `navigator.credentials.create()` requires fresh transient user
// activation: awaiting the `register/begin` fetch inside the same
// click consumes it (Firefox/LibreWolf then instantly reject the
// prompt with NotAllowedError). First click fetches options and arms
// the dialog; the armed click calls `create()` synchronously.
const newKeyName = ref("");
const keyStage = ref<"idle" | "arming" | "armed" | "creating">("idle");
const keyError = ref("");
let pendingOptions: unknown = null;
const renaming = ref<string | null>(null);
const renameValue = ref("");

async function addSecurityKey() {
  keyError.value = "";
  if (keyStage.value === "armed" && pendingOptions) {
    keyStage.value = "creating";
    try {
      const credential = await createSecurityKey(pendingOptions);
      await webauthnRegister(credential, newKeyName.value.trim() || undefined);
      newKeyName.value = "";
      pendingOptions = null;
      keyStage.value = "idle";
      await refreshTwoFactor();
    } catch (e) {
      // A failed finish consumes the staged ceremony — re-arm is
      // required; a browser-side cancel leaves it valid for retry.
      keyStage.value = "armed";
      keyError.value = e instanceof Error ? e.message : String(e);
    }
    return;
  }
  keyStage.value = "arming";
  try {
    pendingOptions = await webauthnRegisterBegin();
    keyStage.value = "armed";
  } catch (e) {
    keyStage.value = "idle";
    keyError.value = e instanceof Error ? e.message : String(e);
  }
}

function cancelKey() {
  pendingOptions = null;
  keyStage.value = "idle";
  keyError.value = "";
}

function startRename(id: string, name: string) {
  renaming.value = id;
  renameValue.value = name;
}

async function saveRename(id: string) {
  try {
    await webauthnRename(id, renameValue.value.trim());
    renaming.value = null;
    await refreshTwoFactor();
  } catch (e) {
    keyError.value = e instanceof Error ? e.message : String(e);
  }
}

/** Dispatch for the shared password gate. */
async function runGate() {
  const which = gate.value;
  if (!which) return;
  gatePending.value = true;
  gateError.value = "";
  const password = gatePassword.value;
  try {
    if (which === "totp-remove") {
      await totpDelete(password);
    } else if (which === "backup") {
      const data = await regenerateBackupCodes(password);
      revealedCodes.value = data.backup_codes;
    } else if (which === "disable-2fa") {
      await disableTwoFactor(password);
    } else if (which.startsWith("key-")) {
      await webauthnDelete(which.slice(4), password);
    } else if (which === "delete-account") {
      await deleteAccount(password);
      auth.signOut();
      void router.push("/");
      return;
    }
    closeGate();
    await refreshTwoFactor();
  } catch (e) {
    gateError.value = e instanceof Error ? e.message : String(e);
  } finally {
    gatePending.value = false;
  }
}

// ---------------------------------------------------------------------------
// Sessions + authorized apps
// ---------------------------------------------------------------------------

const sessions = ref<SessionEntity[]>([]);
const apps = ref<AuthorizedApp[]>([]);
const listError = ref("");

async function revokeSessionById(id: string) {
  await revokeSession(id);
  sessions.value = sessions.value.filter((s) => s.id !== id);
}

async function revokeAppById(id: string) {
  await revokeAuthorizedApp(id);
  apps.value = apps.value.filter((a) => a.id !== id);
}

// ---------------------------------------------------------------------------

const delConfirm = ref(false);

onMounted(async () => {
  try {
    await refreshTwoFactor();
    sessions.value = await listSessions();
    apps.value = await listAuthorizedApps();
  } catch (e) {
    listError.value = e instanceof Error ? e.message : String(e);
  }
});
</script>

<template>
  <div>
    <SettingsNav />
    <Card>
      <template #header>{{ t("settingsSecurity.title") }}</template>
      <div class="security">
        <section class="security__group">
          <h3 class="security__group-title">
            {{ t("settingsSecurity.groupPassword") }}
          </h3>
          <form class="security__form" @submit.prevent="savePassword">
            <Input
              v-model="pwCurrent"
              type="password"
              :label="t('settingsSecurity.currentPassword')"
              autocomplete="current-password"
              required
            />
            <Input
              v-model="pwNew"
              type="password"
              :label="t('settingsSecurity.newPassword')"
              autocomplete="new-password"
              required
            />
            <Input
              v-model="pwConfirm"
              type="password"
              :label="t('settingsSecurity.confirmPassword')"
              autocomplete="new-password"
              required
            />
            <p class="security__hint">
              {{ t("settingsSecurity.passwordHint") }}
            </p>
            <p v-if="pwError" class="security__error" role="alert">
              {{ pwError }}
            </p>
            <div class="security__footer">
              <Button type="submit" :loading="pwPending" :disabled="pwPending">
                {{ t("settingsSecurity.changePassword") }}
              </Button>
              <p v-if="pwSaved" class="security__saved">
                {{ t("settingsSecurity.passwordSaved") }}
              </p>
            </div>
          </form>
        </section>

        <section v-if="twofa" class="security__group">
          <h3 class="security__group-title">
            {{ t("settingsSecurity.group2fa") }}
          </h3>
          <p class="security__status">
            <FontAwesomeIcon
              :icon="twofa.enabled ? 'shield-halved' : 'circle-info'"
              aria-hidden="true"
            />
            {{
              twofa.enabled
                ? t("settingsSecurity.twoFaEnabled", {
                    count: twofa.backup_codes_remaining,
                  })
                : t("settingsSecurity.twoFaDisabled")
            }}
          </p>

          <div class="security__block">
            <div class="security__block-head">
              <strong>{{ t("settingsSecurity.totp") }}</strong>
              <span v-if="twofa.totp.enabled" class="security__badge">{{
                t("settingsSecurity.enabled")
              }}</span>
            </div>

            <div v-if="totpWizard" class="security__wizard">
              <img
                :src="totpWizard.qr_data_uri"
                class="security__qr"
                :alt="t('settingsSecurity.totpQrAlt')"
              />
              <p class="security__secret">
                <code>{{ totpWizard.secret }}</code>
              </p>
              <form class="security__inline" @submit.prevent="confirmTotp">
                <Input
                  v-model="totpCode"
                  :label="t('settingsSecurity.totpCode')"
                  inputmode="numeric"
                  autocomplete="one-time-code"
                  required
                />
                <div class="security__inline-actions">
                  <Button
                    type="submit"
                    size="sm"
                    :loading="totpPending"
                    :disabled="totpPending"
                    >{{ t("settingsSecurity.totpConfirm") }}</Button
                  >
                  <Button
                    variant="secondary"
                    size="sm"
                    @click="totpWizard = null"
                    >{{ t("settingsSecurity.cancel") }}</Button
                  >
                </div>
              </form>
            </div>
            <p v-if="totpError" class="security__error" role="alert">
              {{ totpError }}
            </p>

            <div class="security__row-actions">
              <Button
                v-if="!twofa.totp.enabled && !totpWizard"
                variant="secondary"
                size="sm"
                :disabled="totpPending"
                @click="startTotp"
              >
                {{ t("settingsSecurity.totpSetup") }}
              </Button>
              <template v-if="twofa.totp.enabled">
                <Button
                  v-if="gate !== 'totp-remove'"
                  variant="secondary"
                  size="sm"
                  @click="openGate('totp-remove')"
                >
                  {{ t("settingsSecurity.totpRemove") }}
                </Button>
              </template>
            </div>
            <form
              v-if="gate === 'totp-remove'"
              class="security__gate"
              @submit.prevent="runGate"
            >
              <Input
                v-model="gatePassword"
                type="password"
                :label="t('settingsSecurity.passwordConfirm')"
                autocomplete="current-password"
                required
              />
              <div class="security__inline-actions">
                <Button
                  type="submit"
                  variant="danger"
                  size="sm"
                  :loading="gatePending"
                  >{{ t("settingsSecurity.confirm") }}</Button
                >
                <Button variant="secondary" size="sm" @click="closeGate">{{
                  t("settingsSecurity.cancel")
                }}</Button>
              </div>
            </form>
          </div>

          <div class="security__block">
            <div class="security__block-head">
              <strong>{{ t("settingsSecurity.backupCodes") }}</strong>
              <span class="security__hint-inline">{{
                t("settingsSecurity.backupCodesRemaining", {
                  count: twofa.backup_codes_remaining,
                })
              }}</span>
            </div>
            <div v-if="revealedCodes" class="security__codes">
              <ul class="security__code-list">
                <li v-for="code in revealedCodes" :key="code">
                  <code>{{ code }}</code>
                </li>
              </ul>
              <p class="security__hint">
                {{ t("settingsSecurity.backupCodesHint") }}
              </p>
              <Button
                variant="secondary"
                size="sm"
                @click="revealedCodes = null"
                >{{ t("settingsSecurity.codesSaved") }}</Button
              >
            </div>
            <Button
              v-if="twofa.enabled && gate !== 'backup'"
              variant="secondary"
              size="sm"
              @click="openGate('backup')"
            >
              {{ t("settingsSecurity.backupCodesRegenerate") }}
            </Button>
            <form
              v-if="gate === 'backup'"
              class="security__gate"
              @submit.prevent="runGate"
            >
              <Input
                v-model="gatePassword"
                type="password"
                :label="t('settingsSecurity.passwordConfirm')"
                autocomplete="current-password"
                required
              />
              <div class="security__inline-actions">
                <Button type="submit" size="sm" :loading="gatePending">{{
                  t("settingsSecurity.confirm")
                }}</Button>
                <Button variant="secondary" size="sm" @click="closeGate">{{
                  t("settingsSecurity.cancel")
                }}</Button>
              </div>
            </form>
          </div>

          <div class="security__block">
            <div class="security__block-head">
              <strong>{{ t("settingsSecurity.securityKeys") }}</strong>
            </div>
            <ul v-if="twofa.webauthn.length" class="security__list">
              <li
                v-for="key in twofa.webauthn"
                :key="key.id"
                class="security__list-row"
              >
                <div class="security__list-main">
                  <template v-if="renaming === key.id">
                    <Input
                      v-model="renameValue"
                      :label="t('settingsSecurity.keyName')"
                    />
                    <div class="security__inline-actions">
                      <Button size="sm" @click="saveRename(key.id)">{{
                        t("settingsSecurity.save")
                      }}</Button>
                      <Button
                        variant="secondary"
                        size="sm"
                        @click="renaming = null"
                        >{{ t("settingsSecurity.cancel") }}</Button
                      >
                    </div>
                  </template>
                  <template v-else>
                    <span class="security__list-title">{{
                      key.name || t("settingsSecurity.keyUnnamed")
                    }}</span>
                    <small class="security__list-meta">
                      {{
                        key.last_used_at
                          ? t("settingsSecurity.lastUsed", {
                              when: formatRelativeDate(key.last_used_at),
                            })
                          : key.created_at
                            ? t("settingsSecurity.added", {
                                when: formatRelativeDate(key.created_at),
                              })
                            : ""
                      }}
                    </small>
                  </template>
                </div>
                <div v-if="renaming !== key.id" class="security__list-actions">
                  <Button
                    variant="secondary"
                    size="sm"
                    @click="startRename(key.id, key.name)"
                    >{{ t("settingsSecurity.rename") }}</Button
                  >
                  <Button
                    v-if="gate !== `key-${key.id}`"
                    variant="secondary"
                    size="sm"
                    @click="openGate(`key-${key.id}`)"
                    >{{ t("settingsSecurity.remove") }}</Button
                  >
                </div>
                <form
                  v-if="gate === `key-${key.id}`"
                  class="security__gate"
                  @submit.prevent="runGate"
                >
                  <Input
                    v-model="gatePassword"
                    type="password"
                    :label="t('settingsSecurity.passwordConfirm')"
                    autocomplete="current-password"
                    required
                  />
                  <div class="security__inline-actions">
                    <Button
                      type="submit"
                      variant="danger"
                      size="sm"
                      :loading="gatePending"
                      >{{ t("settingsSecurity.confirm") }}</Button
                    >
                    <Button variant="secondary" size="sm" @click="closeGate">{{
                      t("settingsSecurity.cancel")
                    }}</Button>
                  </div>
                </form>
              </li>
            </ul>
            <p v-else class="security__hint">
              {{ t("settingsSecurity.noSecurityKeys") }}
            </p>
            <form
              v-if="webauthnSupported()"
              class="security__inline"
              @submit.prevent="addSecurityKey"
            >
              <Input
                v-model="newKeyName"
                :label="t('settingsSecurity.keyName')"
                :placeholder="t('settingsSecurity.keyNamePlaceholder')"
              />
              <Button
                type="submit"
                :variant="keyStage === 'armed' ? 'primary' : 'secondary'"
                size="sm"
                :loading="keyStage === 'arming' || keyStage === 'creating'"
                :disabled="keyStage === 'arming' || keyStage === 'creating'"
                >{{
                  keyStage === "armed"
                    ? t("settingsSecurity.keyArmed")
                    : t("settingsSecurity.addKey")
                }}</Button
              >
              <Button
                v-if="keyStage === 'armed'"
                variant="secondary"
                size="sm"
                @click="cancelKey"
                >{{ t("settingsSecurity.cancel") }}</Button
              >
            </form>
            <p v-if="keyStage === 'armed'" class="security__hint">
              {{ t("settingsSecurity.keyArmedHint") }}
            </p>
            <p v-else-if="!webauthnSupported()" class="security__hint">
              {{ t("settingsSecurity.webauthnUnsupported") }}
            </p>
            <p v-if="keyError" class="security__error" role="alert">
              {{ keyError }}
            </p>
          </div>

          <div v-if="twofa.enabled" class="security__block">
            <Button
              v-if="gate !== 'disable-2fa'"
              variant="danger"
              size="sm"
              @click="openGate('disable-2fa')"
            >
              {{ t("settingsSecurity.disableAll") }}
            </Button>
            <form
              v-if="gate === 'disable-2fa'"
              class="security__gate"
              @submit.prevent="runGate"
            >
              <Input
                v-model="gatePassword"
                type="password"
                :label="t('settingsSecurity.passwordConfirm')"
                autocomplete="current-password"
                required
              />
              <div class="security__inline-actions">
                <Button
                  type="submit"
                  variant="danger"
                  size="sm"
                  :loading="gatePending"
                  >{{ t("settingsSecurity.disableAll") }}</Button
                >
                <Button variant="secondary" size="sm" @click="closeGate">{{
                  t("settingsSecurity.cancel")
                }}</Button>
              </div>
            </form>
          </div>
          <p v-if="gateError" class="security__error" role="alert">
            {{ gateError }}
          </p>
        </section>
        <p v-else-if="listError" class="security__error">
          {{ listError }}
        </p>

        <section class="security__group">
          <h3 class="security__group-title">
            {{ t("settingsSecurity.groupSessions") }}
          </h3>
          <ul v-if="sessions.length" class="security__list">
            <li v-for="s in sessions" :key="s.id" class="security__list-row">
              <div class="security__list-main">
                <span class="security__list-title">
                  {{ s.browser || t("settingsSecurity.session") }}
                  <span v-if="s.current" class="security__badge">{{
                    t("settingsSecurity.currentSession")
                  }}</span>
                </span>
                <small class="security__list-meta">
                  <span v-if="s.ip">{{ s.ip }} · </span>
                  <span
                    v-if="s.last_activity_at"
                    :title="formatAbsoluteDate(s.last_activity_at)"
                    >{{ formatRelativeDate(s.last_activity_at) }}</span
                  >
                </small>
              </div>
              <div class="security__list-actions">
                <Button
                  v-if="!s.current"
                  variant="secondary"
                  size="sm"
                  @click="revokeSessionById(s.id)"
                  >{{ t("settingsSecurity.revoke") }}</Button
                >
              </div>
            </li>
          </ul>
          <p v-else class="security__hint">
            {{ t("settingsSecurity.noSessions") }}
          </p>
        </section>

        <section class="security__group">
          <h3 class="security__group-title">
            {{ t("settingsSecurity.groupApps") }}
          </h3>
          <ul v-if="apps.length" class="security__list">
            <li v-for="app in apps" :key="app.id" class="security__list-row">
              <div class="security__list-main">
                <span class="security__list-title">{{ app.name }}</span>
                <small class="security__list-meta">
                  <span v-if="app.scopes">{{ app.scopes }}</span>
                  <span v-if="app.website"> · {{ app.website }}</span>
                </small>
              </div>
              <div class="security__list-actions">
                <Button
                  variant="secondary"
                  size="sm"
                  @click="revokeAppById(app.id)"
                  >{{ t("settingsSecurity.revoke") }}</Button
                >
              </div>
            </li>
          </ul>
          <p v-else class="security__hint">
            {{ t("settingsSecurity.noApps") }}
          </p>
        </section>

        <section class="security__group security__group--danger">
          <h3 class="security__group-title">
            {{ t("settingsSecurity.groupDanger") }}
          </h3>
          <p class="security__hint">{{ t("settingsSecurity.deleteHint") }}</p>
          <label class="security__check">
            <input v-model="delConfirm" type="checkbox" />
            <span>{{ t("settingsSecurity.deleteAcknowledge") }}</span>
          </label>
          <Button
            v-if="delConfirm && gate !== 'delete-account'"
            variant="danger"
            size="sm"
            @click="openGate('delete-account')"
          >
            {{ t("settingsSecurity.deleteAccount") }}
          </Button>
          <form
            v-if="gate === 'delete-account'"
            class="security__gate"
            @submit.prevent="runGate"
          >
            <Input
              v-model="gatePassword"
              type="password"
              :label="t('settingsSecurity.passwordConfirm')"
              autocomplete="current-password"
              required
            />
            <div class="security__inline-actions">
              <Button
                type="submit"
                variant="danger"
                size="sm"
                :loading="gatePending"
                >{{ t("settingsSecurity.deleteAccount") }}</Button
              >
              <Button variant="secondary" size="sm" @click="closeGate">{{
                t("settingsSecurity.cancel")
              }}</Button>
            </div>
          </form>
        </section>
      </div>
    </Card>
  </div>
</template>

<style scoped>
.security {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
}
.security__group {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.security__group--danger {
  border-top: 1px solid var(--border);
  padding-top: var(--space-3);
}
.security__group-title {
  margin: 0;
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-sm);
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--text-muted);
}
.security__form {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  max-width: 22rem;
}
.security__footer {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.security__hint {
  margin: 0;
  font-size: var(--text-sm);
  color: var(--text-muted);
}
.security__hint-inline {
  font-size: var(--text-sm);
  color: var(--text-muted);
}
.security__error {
  margin: 0;
  color: var(--danger);
}
.security__saved {
  margin: 0;
  color: var(--success);
}
.security__status {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  margin: 0;
}
.security__block {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  padding: var(--space-2) 0;
}
.security__block-head {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.security__badge {
  padding: 0 var(--space-2);
  border-radius: var(--radius-full);
  background: var(--accent-soft);
  color: var(--accent);
  font-size: var(--text-xs);
  font-weight: 700;
  text-transform: uppercase;
}
.security__wizard {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-sunken);
  max-width: 26rem;
}
.security__qr {
  width: 10rem;
  height: 10rem;
  background: #fff;
  border-radius: var(--radius-sm);
}
.security__secret {
  margin: 0;
  word-break: break-all;
}
.security__codes {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  align-items: flex-start;
}
.security__code-list {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(9rem, 1fr));
  gap: var(--space-1) var(--space-3);
  margin: 0;
  padding: var(--space-3);
  list-style: none;
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-sunken);
  width: 100%;
  max-width: 26rem;
}
.security__inline {
  display: flex;
  align-items: flex-end;
  gap: var(--space-2);
  flex-wrap: wrap;
}
.security__inline .field {
  flex: 1;
  min-width: 10rem;
  max-width: 14rem;
}
.security__inline-actions {
  display: flex;
  gap: var(--space-2);
}
.security__gate {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  max-width: 22rem;
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-sunken);
}
.security__list {
  display: flex;
  flex-direction: column;
  margin: 0;
  padding: 0;
  list-style: none;
}
.security__list-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  padding: var(--space-2) 0;
  border-top: 1px solid var(--border);
  flex-wrap: wrap;
}
.security__list-main {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  min-width: 0;
}
.security__list-title {
  font-weight: 600;
}
.security__list-meta {
  color: var(--text-muted);
}
.security__list-actions {
  display: flex;
  gap: var(--space-2);
}
.security__check {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.security__row-actions {
  display: flex;
  gap: var(--space-2);
}
@media (max-width: 768px) {
  .security__list-row {
    align-items: flex-start;
    flex-direction: column;
  }
}
</style>
