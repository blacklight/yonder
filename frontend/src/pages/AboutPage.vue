<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import Card from "@/components/ui/Card.vue";
import Avatar from "@/components/ui/Avatar.vue";
import Spinner from "@/components/ui/Spinner.vue";
import {
  getInstance,
  getInstanceV1,
  getExtendedDescription,
  getInstanceDomainBlocks,
  type ExtendedDescription,
  type InstanceV1Entity,
  type PublicDomainBlock,
} from "@/api/instance";
import type { InstanceEntity } from "@/api/entities";
import { sanitizeHtml, safeHttpUrl } from "@/utils/sanitize";
import { useAuthStore } from "@/stores/auth";

// Mastodon's /about layout: a hero (banner + name + tagline + CTA),
// the stats band, the long-form "about this server" text, the
// numbered server rules, the moderated-servers listing and the
// "administered by" contact card — all anonymous-eligible.
const { t } = useI18n();
const auth = useAuthStore();

const instance = ref<InstanceEntity | null>(null);
const v1 = ref<InstanceV1Entity | null>(null);
const extended = ref<ExtendedDescription | null>(null);
const blocks = ref<PublicDomainBlock[] | null>(null);
const loading = ref(true);
const loadError = ref("");

onMounted(async () => {
  loading.value = true;
  loadError.value = "";
  try {
    // The entity is the page's backbone — a failure there is fatal;
    // the optional sections self-resolve (404 → hide) in parallel.
    const [entity, v1Entity, ext, mod] = await Promise.all([
      getInstance(),
      getInstanceV1().catch(() => null),
      getExtendedDescription(),
      getInstanceDomainBlocks().catch(() => null),
    ]);
    instance.value = entity;
    v1.value = v1Entity;
    extended.value = ext;
    blocks.value = mod;
  } catch {
    loadError.value = t("about.loadError");
  } finally {
    loading.value = false;
  }
});

const title = computed(
  () => instance.value?.title || instance.value?.domain || "",
);
const domain = computed(() => instance.value?.domain ?? "");
const bannerUrl = computed(() =>
  safeHttpUrl(instance.value?.thumbnail?.url ?? null),
);
const logoUrl = computed(() =>
  safeHttpUrl(instance.value?.icon?.[0]?.src ?? null),
);
const description = computed(() => {
  const text =
    instance.value?.short_description?.trim() ||
    instance.value?.description?.trim() ||
    "";
  return sanitizeHtml(text);
});
const extendedHtml = computed(() =>
  extended.value?.content ? sanitizeHtml(extended.value.content) : "",
);
const aboutHtml = computed(() => extendedHtml.value || description.value);
const rules = computed(() => instance.value?.rules ?? []);
const registrations = computed(() => instance.value?.registrations);
const canRegister = computed(() => registrations.value?.enabled === true);
const contactAccount = computed(
  () => instance.value?.contact?.account ?? v1.value?.contact_account ?? null,
);
const contactName = computed(
  () =>
    instance.value?.contact?.name || contactAccount.value?.display_name || "",
);
const contactUrl = computed(
  () =>
    safeHttpUrl(contactAccount.value?.url ?? null) ??
    safeHttpUrl(instance.value?.contact?.url ?? null),
);
const contactEmail = computed(
  () => instance.value?.contact?.email ?? v1.value?.email ?? null,
);
const stats = computed(() => {
  const s = v1.value?.stats;
  const items: { key: string; value: number }[] = [];
  const active = instance.value?.usage?.users?.active_month;
  if (typeof active === "number") {
    items.push({ key: "about.stats.activeUsers", value: active });
  }
  if (typeof s?.user_count === "number")
    items.push({ key: "about.stats.users", value: s.user_count });
  if (typeof s?.status_count === "number")
    items.push({ key: "about.stats.statuses", value: s.status_count });
  if (typeof s?.domain_count === "number")
    items.push({ key: "about.stats.domains", value: s.domain_count });
  return items;
});
const languages = computed(() => instance.value?.languages ?? []);
const sourceUrl = computed(() =>
  safeHttpUrl(instance.value?.source_url ?? null),
);
const acctLink = computed(() =>
  contactAccount.value ? `/@${contactAccount.value.acct}` : null,
);
const SEVERITIES = new Set(["noop", "silence", "suspend"]);
function severityLabel(severity: string) {
  return SEVERITIES.has(severity) ? t(`about.severity.${severity}`) : severity;
}
</script>

<template>
  <div class="about">
    <Spinner v-if="loading" />
    <p v-else-if="loadError" class="about__error" role="alert">
      {{ loadError }}
    </p>
    <template v-else-if="instance">
      <header class="about__hero">
        <img
          v-if="bannerUrl"
          :src="bannerUrl"
          :alt="t('about.bannerAlt', { title })"
          class="about__banner"
        />
        <div class="about__hero-body">
          <img
            v-if="logoUrl"
            :src="logoUrl"
            :alt="t('about.logoAlt', { title })"
            class="about__logo"
          />
          <div class="about__hero-text">
            <h1 class="about__title">{{ title }}</h1>
            <p class="about__domain">{{ domain }}</p>
            <p v-if="description" class="about__tagline" v-html="description" />
          </div>
          <div v-if="!auth.isLoggedIn" class="about__cta">
            <router-link
              v-if="canRegister"
              to="/register"
              class="about__cta-link about__cta-link--primary"
              >{{ t("nav.signUp") }}</router-link
            >
            <router-link to="/login" class="about__cta-link">{{
              t("nav.signIn")
            }}</router-link>
          </div>
        </div>
      </header>

      <dl v-if="stats.length" class="about__stats">
        <div v-for="s in stats" :key="s.key" class="about__stat">
          <dt>{{ t(s.key) }}</dt>
          <dd>{{ s.value.toLocaleString() }}</dd>
        </div>
      </dl>

      <Card v-if="aboutHtml">
        <template #header>{{ t("about.aboutThisServer") }}</template>
        <div class="about__description" v-html="aboutHtml" />
      </Card>

      <Card v-if="rules.length">
        <template #header>{{ t("about.serverRules") }}</template>
        <ol class="about__rules">
          <li v-for="rule in rules" :key="rule.id">{{ rule.text }}</li>
        </ol>
      </Card>

      <Card v-if="contactAccount || contactName || contactEmail">
        <template #header>{{ t("about.administeredBy") }}</template>
        <div class="about__contact">
          <router-link
            v-if="contactAccount && acctLink"
            :to="acctLink"
            class="about__contact-account"
          >
            <Avatar
              :src="contactAccount.avatar"
              :name="contactAccount.display_name || contactAccount.acct"
            />
            <span class="about__contact-names">
              <strong>{{
                contactAccount.display_name || contactAccount.acct
              }}</strong>
              <span class="about__contact-acct"
                >@{{ contactAccount.acct }}</span
              >
            </span>
          </router-link>
          <template v-else>
            <a
              v-if="contactUrl"
              :href="contactUrl"
              target="_blank"
              rel="noopener"
              >{{ contactName || contactUrl }}</a
            >
            <strong v-else-if="contactName">{{ contactName }}</strong>
          </template>
          <a
            v-if="contactEmail"
            :href="`mailto:${contactEmail}`"
            class="about__contact-email"
            >{{ contactEmail }}</a
          >
        </div>
      </Card>

      <Card v-if="blocks?.length">
        <template #header>{{ t("about.moderatedServers") }}</template>
        <ul class="about__blocks">
          <li v-for="b in blocks" :key="b.domain">
            <strong>{{ b.domain }}</strong>
            <span class="about__block-severity">{{
              severityLabel(b.severity)
            }}</span>
            <p v-if="b.comment" class="about__block-comment">
              {{ b.comment }}
            </p>
          </li>
        </ul>
      </Card>

      <footer class="about__footer">
        <span v-if="instance.version">{{ instance.version }}</span>
        <span v-if="languages.length">
          {{ t("about.languages") }}: {{ languages.join(", ") }}
        </span>
        <a v-if="sourceUrl" :href="sourceUrl" target="_blank" rel="noopener">{{
          t("footer.source")
        }}</a>
      </footer>
    </template>
  </div>
</template>

<style scoped>
.about {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.about__error {
  color: var(--danger);
}
.about__hero {
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  overflow: hidden;
}
.about__banner {
  display: block;
  width: 100%;
  max-height: 14rem;
  object-fit: cover;
}
.about__hero-body {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-end;
  gap: var(--space-4);
  padding: var(--space-4);
}
.about__logo {
  width: 4.5rem;
  height: 4.5rem;
  border-radius: var(--radius-md);
  object-fit: contain;
  background: var(--bg-sunken);
  margin-top: -3rem;
  border: 3px solid var(--bg-raised);
}
.about__banner + .about__hero-body .about__logo {
  margin-top: -3rem;
}
.about__hero-body > .about__logo:first-child {
  margin-top: 0;
}
.about__hero-text {
  flex: 1 1 auto;
  min-width: 12rem;
}
.about__title {
  margin: 0;
  font-size: var(--text-xl);
}
.about__domain {
  margin: 0;
  color: var(--text-faint, var(--text-muted));
}
.about__tagline {
  margin: var(--space-2) 0 0;
  color: var(--text-muted);
}
.about__cta {
  display: flex;
  gap: var(--space-3);
  align-items: center;
}
.about__cta-link {
  display: inline-flex;
  align-items: center;
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-2) var(--space-4);
  font-weight: 600;
  text-decoration: none;
  color: var(--text);
}
.about__cta-link--primary {
  background: var(--accent);
  border-color: transparent;
  color: var(--accent-fg);
}
.about__stats {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-4);
  margin: 0;
  padding: var(--space-4);
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
}
.about__stat dt {
  color: var(--text-faint, var(--text-muted));
  font-size: var(--text-sm);
}
.about__stat dd {
  margin: 0;
  font-size: var(--text-xl);
  font-weight: 700;
}
.about__description :deep(p:first-child) {
  margin-top: 0;
}
.about__description :deep(p:last-child) {
  margin-bottom: 0;
}
.about__rules {
  margin: 0;
  padding-inline-start: var(--space-5, 1.5rem);
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.about__contact {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.about__contact-account {
  display: inline-flex;
  align-items: center;
  gap: var(--space-3);
  text-decoration: none;
  color: inherit;
}
.about__contact-names {
  display: flex;
  flex-direction: column;
}
.about__contact-acct {
  color: var(--text-faint, var(--text-muted));
  font-size: var(--text-sm);
}
.about__blocks {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.about__block-severity {
  margin-inline-start: var(--space-2);
  color: var(--text-faint, var(--text-muted));
  font-size: var(--text-sm);
}
.about__block-comment {
  margin: var(--space-1) 0 0;
  color: var(--text-muted);
}
.about__footer {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-3);
  color: var(--text-faint, var(--text-muted));
  font-size: var(--text-sm);
}
</style>
