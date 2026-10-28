<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import { createToken, listTokens, revokeToken } from "@/api/settings";
import type { CreatedToken, Token } from "@/api/entities";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";
import { useAuthStore } from "@/stores/auth";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";
import SettingsNav from "@/components/settings/SettingsNav.vue";

const { t } = useI18n();
const auth = useAuthStore();

// The scopes the form offers — the backend additionally rejects any
// scope outside the caller's own grant. `admin` is offered to staff
// only: non-staff roles can never exercise it anyway.
const scopeOptions = computed(() => {
  const base = ["read", "write", "follow", "push"];
  return auth.isStaff ? [...base, "admin"] : base;
});

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
  name: "",
  scopes: ["read"] as string[],
  expiry: "" as "" | number,
});
const creating = ref(false);
const createError = ref("");

// The plaintext is only returned by POST — it stays on screen (with a
// copy affordance) until the user dismisses it.
const revealed = ref<CreatedToken | null>(null);
const copied = ref(false);

async function copyRevealed() {
  if (!revealed.value) return;
  await navigator.clipboard.writeText(revealed.value.token);
  copied.value = true;
}

async function submit() {
  createError.value = "";
  if (!form.name.trim()) {
    createError.value = t("settingsTokens.nameRequired");
    return;
  }
  if (!form.scopes.length) {
    createError.value = t("settingsTokens.scopeRequired");
    return;
  }
  creating.value = true;
  try {
    const body: { name: string; scopes: string; expires_in?: number } = {
      name: form.name.trim(),
      scopes: form.scopes.join(" "),
    };
    if (form.expiry) body.expires_in = Number(form.expiry);
    const created = await createToken(body);
    revealed.value = created;
    copied.value = false;
    form.name = "";
    await refresh();
  } catch (e) {
    createError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    creating.value = false;
  }
}

// ---------------------------------------------------------------------------
// List + revoke
// ---------------------------------------------------------------------------

const tokens = ref<Token[]>([]);
const loading = ref(true);
const listError = ref("");

// Two-click revoke: the first click arms the row, the second (danger)
// click calls the API.
const confirming = ref<string | null>(null);
const revoking = ref(false);

async function refresh() {
  listError.value = "";
  try {
    tokens.value = await listTokens();
  } catch (e) {
    listError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

function isExpired(token: Token) {
  return token.expires_at
    ? new Date(token.expires_at).getTime() <= Date.now()
    : false;
}

function scopeList(token: Token) {
  return token.scopes?.join(" ") ?? "";
}

async function revoke(id: string) {
  revoking.value = true;
  listError.value = "";
  try {
    await revokeToken(id);
    tokens.value = tokens.value.filter((tk) => tk.id !== id);
  } catch (e) {
    listError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    revoking.value = false;
    confirming.value = null;
  }
}

onMounted(refresh);
</script>

<template>
  <div>
    <SettingsNav />
    <Card>
      <template #header>{{ t("settingsTokens.title") }}</template>
      <div class="tokens">
        <p class="tokens__hint">{{ t("settingsTokens.intro") }}</p>

        <section class="tokens__group">
          <h3 class="tokens__group-title">
            {{ t("settingsTokens.groupCreate") }}
          </h3>
          <form class="tokens__form" @submit.prevent="submit">
            <Input
              v-model="form.name"
              :label="t('settingsTokens.nameLabel')"
              :placeholder="t('settingsTokens.namePlaceholder')"
              required
            />
            <fieldset class="tokens__scopes">
              <legend>{{ t("settingsTokens.scopesLabel") }}</legend>
              <label
                v-for="scope in scopeOptions"
                :key="scope"
                class="tokens__check"
              >
                <input v-model="form.scopes" type="checkbox" :value="scope" />
                <span>{{ t(`settingsTokens.scope.${scope}`) }}</span>
              </label>
              <p class="tokens__hint">{{ t("settingsTokens.scopesHint") }}</p>
            </fieldset>
            <label class="tokens__field">
              <span>{{ t("settingsTokens.expiryLabel") }}</span>
              <select v-model="form.expiry">
                <option
                  v-for="opt in expiryOptions"
                  :key="opt.value"
                  :value="opt.value"
                >
                  {{ t(`settingsTokens.${opt.label}`) }}
                </option>
              </select>
            </label>
            <p v-if="createError" class="tokens__error" role="alert">
              {{ createError }}
            </p>
            <div>
              <Button type="submit" :loading="creating" :disabled="creating">
                {{ t("settingsTokens.create") }}
              </Button>
            </div>
          </form>

          <div v-if="revealed" class="tokens__reveal">
            <p class="tokens__reveal-label">
              {{ t("settingsTokens.revealHint") }}
            </p>
            <code class="tokens__secret">{{ revealed.token }}</code>
            <div class="tokens__inline-actions">
              <Button variant="secondary" size="sm" @click="copyRevealed">
                {{
                  copied ? t("settingsTokens.copied") : t("settingsTokens.copy")
                }}
              </Button>
              <Button variant="secondary" size="sm" @click="revealed = null">
                {{ t("settingsTokens.saved") }}
              </Button>
            </div>
          </div>
        </section>

        <section class="tokens__group">
          <h3 class="tokens__group-title">
            {{ t("settingsTokens.groupList") }}
          </h3>
          <p v-if="listError" class="tokens__error" role="alert">
            {{ listError }}
          </p>
          <ul v-if="tokens.length" class="tokens__list">
            <li v-for="token in tokens" :key="token.id" class="tokens__row">
              <div class="tokens__main">
                <span class="tokens__name">
                  {{ token.name }}
                  <span v-if="isExpired(token)" class="tokens__badge">{{
                    t("settingsTokens.expired")
                  }}</span>
                </span>
                <small class="tokens__meta">
                  <span v-if="scopeList(token)">{{ scopeList(token) }}</span>
                  <span v-if="token.created_at">
                    ·
                    <span :title="formatAbsoluteDate(token.created_at)">{{
                      t("settingsTokens.created", {
                        when: formatRelativeDate(token.created_at),
                      })
                    }}</span>
                  </span>
                  <span v-if="token.expires_at">
                    ·
                    <span :title="formatAbsoluteDate(token.expires_at)">{{
                      t("settingsTokens.expires", {
                        when: formatRelativeDate(token.expires_at),
                      })
                    }}</span>
                  </span>
                  <span v-else> · {{ t("settingsTokens.neverExpires") }}</span>
                </small>
              </div>
              <div class="tokens__actions">
                <template v-if="confirming === token.id">
                  <Button
                    variant="danger"
                    size="sm"
                    :loading="revoking"
                    @click="revoke(token.id)"
                    >{{ t("settingsTokens.revokeConfirm") }}</Button
                  >
                  <Button
                    variant="secondary"
                    size="sm"
                    @click="confirming = null"
                    >{{ t("settingsTokens.cancel") }}</Button
                  >
                </template>
                <Button
                  v-else
                  variant="secondary"
                  size="sm"
                  @click="confirming = token.id"
                  >{{ t("settingsTokens.revoke") }}</Button
                >
              </div>
            </li>
          </ul>
          <p v-else-if="!loading" class="tokens__hint">
            {{ t("settingsTokens.empty") }}
          </p>
        </section>
      </div>
    </Card>
  </div>
</template>

<style scoped>
.tokens {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
}
.tokens__group {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.tokens__group-title {
  margin: 0;
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-sm);
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--text-muted);
}
.tokens__form {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  max-width: 26rem;
}
.tokens__scopes {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  margin: 0;
  padding: 0;
  border: 0;
}
.tokens__scopes legend {
  padding: 0;
  font-size: var(--text-sm);
  font-weight: 500;
}
.tokens__check {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.tokens__field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  font-size: var(--text-sm);
  max-width: 14rem;
}
.tokens__hint {
  margin: 0;
  font-size: var(--text-sm);
  color: var(--text-muted);
}
.tokens__error {
  margin: 0;
  color: var(--danger);
}
.tokens__reveal {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-sunken);
  max-width: 26rem;
}
.tokens__reveal-label {
  margin: 0;
  font-size: var(--text-sm);
}
.tokens__secret {
  word-break: break-all;
}
.tokens__inline-actions {
  display: flex;
  gap: var(--space-2);
}
.tokens__list {
  display: flex;
  flex-direction: column;
  margin: 0;
  padding: 0;
  list-style: none;
}
.tokens__row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  padding: var(--space-2) 0;
  border-top: 1px solid var(--border);
  flex-wrap: wrap;
}
.tokens__main {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  min-width: 0;
}
.tokens__name {
  font-weight: 600;
}
.tokens__badge {
  padding: 0 var(--space-2);
  border-radius: var(--radius-full);
  background: var(--accent-soft);
  color: var(--accent);
  font-size: var(--text-xs);
  font-weight: 700;
  text-transform: uppercase;
}
.tokens__meta {
  color: var(--text-muted);
}
.tokens__actions {
  display: flex;
  gap: var(--space-2);
}
@media (max-width: 768px) {
  .tokens__row {
    align-items: flex-start;
    flex-direction: column;
  }
}
</style>
