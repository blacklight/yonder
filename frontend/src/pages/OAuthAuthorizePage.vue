<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute } from "vue-router";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import { FontAwesomeIcon } from "@/icons";
import { ApiError } from "@/api/errors";
import {
  authorizeApp,
  authorizeInfo,
  navigateTo,
  type AuthorizeInfo,
  type AuthorizeQuery,
} from "@/api/oauth";
import { useAuthStore } from "@/stores/auth";

// OAuth consent screen for client apps (Moshidon, Tusky, …). The
// route is `requiresAuth` — anonymous visitors sign in through the
// normal flow and return here. "Authorize" POSTs `/oauth/authorize`
// and navigates to the returned `redirect_to`; for a custom-scheme
// redirect URI (`moshidon-android-auth://…`) the platform hands the
// authorization code back to the app.
const { t } = useI18n();
const route = useRoute();
const auth = useAuthStore();

function q(name: string): string | undefined {
  const v = route.query[name];
  return typeof v === "string" && v ? v : undefined;
}

const query = ref<AuthorizeQuery | null>(null);
const info = ref<AuthorizeInfo | null>(null);
const loading = ref(true);
const loadError = ref("");
const pending = ref(false);
const error = ref("");
// Set by "deny" when the redirect URI can't carry the error back to
// the client (the urn:* out-of-band placeholders go nowhere).
const denied = ref(false);

const requestedScopes = computed(() => {
  const raw = query.value?.scope || (info.value?.scopes ?? []).join(" ");
  return raw.split(/\s+/).filter(Boolean);
});
const isOob = computed(
  () => query.value?.redirect_uri.startsWith("urn:") ?? false,
);
// `website` arrives as free text — often without a scheme (Moshidon
// sends "github.com/…"). Only http(s) is ever linked; anything else
// gains an https:// prefix so it can't smuggle a live scheme in.
const websiteHref = computed(() => {
  const w = info.value?.website;
  if (!w) return "";
  return w.startsWith("https://") || w.startsWith("http://")
    ? w
    : `https://${w}`;
});

async function load() {
  // The watcher can re-run after a bad query — reset everything.
  loading.value = true;
  loadError.value = "";
  error.value = "";
  query.value = null;
  info.value = null;
  const client_id = q("client_id");
  const redirect_uri = q("redirect_uri");
  const responseType = q("response_type") ?? "code";
  if (!client_id || !redirect_uri || responseType !== "code") {
    loadError.value = t("oauth.invalidRequest");
    loading.value = false;
    return;
  }
  query.value = {
    client_id,
    redirect_uri,
    response_type: responseType,
    scope: q("scope"),
    state: q("state"),
    code_challenge: q("code_challenge"),
    code_challenge_method: q("code_challenge_method"),
  };
  try {
    info.value = await authorizeInfo(query.value);
  } catch (e) {
    loadError.value =
      e instanceof ApiError && e.status === 404
        ? t("oauth.unknownApp")
        : t("oauth.loadFailed");
  } finally {
    loading.value = false;
  }
}

// The route's query IS the authorization request — re-evaluate if it
// changes (account switch, a second client hop) rather than once.
watch(
  () => route.query,
  () => void load(),
  { immediate: true },
);
void auth.restore();

async function authorize() {
  if (!query.value) return;
  error.value = "";
  pending.value = true;
  try {
    const res = await authorizeApp(query.value);
    navigateTo(res.redirect_to);
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : t("oauth.failed");
    pending.value = false;
  }
}

// RFC 6749 §4.1.2.1 — the denial rides back to the client on its
// redirect URI, exactly like the code would have.
function deny() {
  const query_ = query.value;
  if (!query_) return;
  if (isOob.value) {
    denied.value = true;
    return;
  }
  try {
    const target = new URL(query_.redirect_uri);
    target.searchParams.set("error", "access_denied");
    target.searchParams.set("error_description", "The user denied access.");
    if (query_.state) target.searchParams.set("state", query_.state);
    navigateTo(target.toString());
  } catch {
    // A redirect_uri that doesn't parse is rejected server-side
    // anyway — fall back to the plain "close this window" state.
    denied.value = true;
  }
}
</script>

<template>
  <Card class="authorize">
    <template #header>{{ t("oauth.title") }}</template>
    <p v-if="loadError" class="authorize__error" role="alert">
      {{ loadError }}
    </p>
    <p v-else-if="loading" class="authorize__muted">…</p>
    <p v-else-if="denied" class="authorize__muted" role="status">
      {{ t("oauth.denied") }}
    </p>
    <div v-else class="authorize__body">
      <p class="authorize__prompt">
        <strong>{{ info?.client_name }}</strong> {{ t("oauth.wantsAccess") }}
      </p>
      <p v-if="info?.website" class="authorize__muted">
        <a :href="websiteHref" target="_blank" rel="noopener">{{
          info.website
        }}</a>
      </p>
      <template v-if="requestedScopes.length">
        <p class="authorize__muted">{{ t("oauth.scopesTitle") }}</p>
        <ul class="authorize__scopes">
          <li v-for="scope in requestedScopes" :key="scope">{{ scope }}</li>
        </ul>
      </template>
      <p v-if="auth.account" class="authorize__muted">
        {{ t("oauth.signedInAs", { acct: auth.account.acct }) }}
      </p>
      <p v-if="error" class="authorize__error" role="alert">{{ error }}</p>
      <div class="authorize__actions">
        <Button :loading="pending" :disabled="pending" @click="authorize">
          <FontAwesomeIcon icon="check" />
          {{ t("oauth.authorize") }}
        </Button>
        <Button variant="secondary" :disabled="pending" @click="deny">
          <FontAwesomeIcon icon="xmark" />
          {{ t("oauth.deny") }}
        </Button>
      </div>
    </div>
  </Card>
</template>

<style scoped>
.authorize {
  max-width: 28rem;
  margin: 0 auto;
}
.authorize__body {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.authorize__prompt {
  margin: 0;
}
.authorize__muted {
  color: var(--text-muted);
  margin: 0;
}
.authorize__scopes {
  margin: 0;
  padding-inline-start: var(--space-6, 1.5rem);
}
.authorize__actions {
  display: flex;
  gap: var(--space-3);
}
.authorize__error {
  color: var(--danger);
  margin: 0;
}
</style>
