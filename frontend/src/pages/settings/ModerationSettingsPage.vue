<script setup lang="ts">
import { onMounted, reactive, ref, shallowRef } from "vue";
import { useI18n } from "vue-i18n";
import { ApiError } from "@/api/errors";
import {
  blockedAccounts,
  mutedAccounts,
  unblockAccount,
  unmuteAccount,
} from "@/api/accounts";
import { blockDomain, domainBlocksPage, unblockDomain } from "@/api/domains";
import type { Page } from "@/api/pagination";
import type { Account } from "@/api/entities";
import Avatar from "@/components/ui/Avatar.vue";
import AccountRowSkeleton from "@/components/ui/AccountRowSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import EmojiText from "@/components/ui/EmojiText.vue";
import Input from "@/components/ui/Input.vue";
import SettingsNav from "@/components/settings/SettingsNav.vue";

const { t } = useI18n();

/**
 * Shared state for the three Link-header-paginated edge lists (mutes,
 * blocks, domain blocks). `key` identifies a row for removal;
 * `remove` performs the API call.
 */
function useEdgeList<T>(
  fetchPage: (query?: { max_id?: string }) => Promise<Page<T>>,
  opts: {
    key: (item: T) => string;
    remove: (item: T) => Promise<unknown>;
  },
) {
  const list = reactive({
    // Shallow — rows are never mutated in place, the page swaps the
    // array wholesale on fetch/removal. (Also keeps the generic `T`
    // out of reactive's deep-unwrap machinery.)
    items: shallowRef<T[]>([]),
    nextCursor: undefined as string | undefined,
    loading: true,
    error: "",
    busyKey: null as string | null,

    async load(more = false) {
      list.error = "";
      list.loading = true;
      try {
        const page = await fetchPage(
          more && list.nextCursor
            ? {
                max_id:
                  new URL(list.nextCursor, location.origin).searchParams.get(
                    "max_id",
                  ) ?? undefined,
              }
            : undefined,
        );
        list.items = more ? [...list.items, ...page.items] : page.items;
        list.nextCursor = page.next;
      } catch (e) {
        list.error = e instanceof ApiError ? e.message : String(e);
      } finally {
        list.loading = false;
      }
    },

    async remove(item: T) {
      const key = opts.key(item);
      list.busyKey = key;
      list.error = "";
      try {
        await opts.remove(item);
        list.items = list.items.filter((i) => opts.key(i) !== key);
      } catch (e) {
        list.error = e instanceof ApiError ? e.message : String(e);
      } finally {
        list.busyKey = null;
      }
    },
  });
  return list;
}

const muted = useEdgeList<Account>(mutedAccounts, {
  key: (a) => a.id,
  remove: (a) => unmuteAccount(a.id),
});
const blocked = useEdgeList<Account>(blockedAccounts, {
  key: (a) => a.id,
  remove: (a) => unblockAccount(a.id),
});
const domains = useEdgeList<string>(domainBlocksPage, {
  key: (d) => d,
  remove: (d) => unblockDomain(d),
});

const accountSections = [
  {
    key: "muted",
    titleKey: "settingsModeration.groupMuted",
    emptyKey: "settingsModeration.mutedEmpty",
    actionKey: "settingsModeration.unmute",
    list: muted,
  },
  {
    key: "blocked",
    titleKey: "settingsModeration.groupBlocked",
    emptyKey: "settingsModeration.blockedEmpty",
    actionKey: "settingsModeration.unblock",
    list: blocked,
  },
];

// ---------------------------------------------------------------------------
// Add a domain block
// ---------------------------------------------------------------------------

const newDomain = ref("");
const adding = ref(false);
const addError = ref("");

/** Tolerate pasted URLs — keep just the hostname. */
function normalizeDomain(input: string): string {
  const raw = input.trim().toLowerCase();
  if (!raw) return "";
  if (raw.includes("://")) {
    try {
      return new URL(raw).hostname;
    } catch {
      return "";
    }
  }
  return raw.split(/[/?#]/)[0];
}

async function addDomain() {
  addError.value = "";
  const domain = normalizeDomain(newDomain.value);
  if (!/^[^\s]+\.[^\s]+$/.test(domain)) {
    addError.value = t("settingsModeration.addDomainInvalid");
    return;
  }
  adding.value = true;
  try {
    await blockDomain(domain);
    if (!domains.items.includes(domain)) {
      domains.items = [domain, ...domains.items];
    }
    newDomain.value = "";
  } catch (e) {
    addError.value = e instanceof ApiError ? e.message : String(e);
  } finally {
    adding.value = false;
  }
}

onMounted(() => {
  void muted.load();
  void blocked.load();
  void domains.load();
});
</script>

<template>
  <div>
    <SettingsNav />
    <Card>
      <template #header>{{ t("settingsModeration.title") }}</template>
      <div class="mod">
        <p class="mod__hint">{{ t("settingsModeration.intro") }}</p>

        <section
          v-for="sec in accountSections"
          :key="sec.key"
          class="mod__group"
        >
          <h3 class="mod__group-title">{{ t(sec.titleKey) }}</h3>
          <p v-if="sec.list.error" class="mod__error" role="alert">
            {{ sec.list.error }}
          </p>
          <ul
            v-if="sec.list.loading && !sec.list.items.length"
            class="mod__list"
            role="status"
            :aria-label="t('common.loading')"
          >
            <li v-for="n in 3" :key="n"><AccountRowSkeleton /></li>
          </ul>
          <template v-else-if="sec.list.items.length">
            <ul class="mod__list" :aria-busy="sec.list.loading || undefined">
              <li v-for="a in sec.list.items" :key="a.id" class="mod__row">
                <router-link :to="`/@${a.acct}`" class="mod__acct">
                  <Avatar
                    :src="a.avatar"
                    :name="a.display_name || a.acct"
                    size="sm"
                  />
                  <span class="mod__names">
                    <span class="mod__display"
                      ><EmojiText
                        :text="a.display_name || a.username"
                        :emojis="a.emojis"
                    /></span>
                    <span class="mod__handle">@{{ a.acct }}</span>
                  </span>
                </router-link>
                <Button
                  variant="secondary"
                  size="sm"
                  :loading="sec.list.busyKey === a.id"
                  :disabled="sec.list.busyKey !== null"
                  @click="sec.list.remove(a)"
                >
                  {{ t(sec.actionKey) }}
                </Button>
              </li>
              <li v-for="n in sec.list.loading ? 2 : 0" :key="`skel-${n}`">
                <AccountRowSkeleton />
              </li>
            </ul>
            <div v-if="sec.list.nextCursor" class="mod__more">
              <Button
                variant="secondary"
                size="sm"
                :loading="sec.list.loading"
                @click="sec.list.load(true)"
              >
                {{ t("common.loadMore") }}
              </Button>
            </div>
          </template>
          <p v-else class="mod__hint">{{ t(sec.emptyKey) }}</p>
        </section>

        <section class="mod__group">
          <h3 class="mod__group-title">
            {{ t("settingsModeration.groupDomains") }}
          </h3>
          <form class="mod__form" @submit.prevent="addDomain">
            <Input
              v-model="newDomain"
              :label="t('settingsModeration.addDomainLabel')"
              :placeholder="t('settingsModeration.addDomainPlaceholder')"
              :error="addError"
              inputmode="url"
              autocomplete="off"
            />
            <div>
              <Button
                type="submit"
                :loading="adding"
                :disabled="adding || !newDomain.trim()"
              >
                {{ t("settingsModeration.addDomain") }}
              </Button>
            </div>
          </form>
          <p v-if="domains.error" class="mod__error" role="alert">
            {{ domains.error }}
          </p>
          <ul
            v-if="domains.loading && !domains.items.length"
            class="mod__list"
            role="status"
            :aria-label="t('common.loading')"
          >
            <li v-for="n in 2" :key="n" class="mod__row">
              <AccountRowSkeleton :avatar="false" />
            </li>
          </ul>
          <template v-else-if="domains.items.length">
            <ul class="mod__list" :aria-busy="domains.loading || undefined">
              <li v-for="d in domains.items" :key="d" class="mod__row">
                <span class="mod__domain">{{ d }}</span>
                <Button
                  variant="secondary"
                  size="sm"
                  :loading="domains.busyKey === d"
                  :disabled="domains.busyKey !== null"
                  @click="domains.remove(d)"
                >
                  {{ t("settingsModeration.unblock") }}
                </Button>
              </li>
            </ul>
            <div v-if="domains.nextCursor" class="mod__more">
              <Button
                variant="secondary"
                size="sm"
                :loading="domains.loading"
                @click="domains.load(true)"
              >
                {{ t("common.loadMore") }}
              </Button>
            </div>
          </template>
          <p v-else class="mod__hint">
            {{ t("settingsModeration.domainsEmpty") }}
          </p>
        </section>
      </div>
    </Card>
  </div>
</template>

<style scoped>
.mod {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
}
.mod__group {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.mod__group-title {
  margin: 0;
  padding-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-size: var(--text-sm);
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--text-muted);
}
.mod__hint {
  margin: 0;
  font-size: var(--text-sm);
  color: var(--text-muted);
}
.mod__error {
  margin: 0;
  color: var(--danger);
}
.mod__list {
  display: flex;
  flex-direction: column;
  margin: 0;
  padding: 0;
  list-style: none;
}
.mod__row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  padding: var(--space-2) 0;
  border-top: 1px solid var(--border);
}
.mod__acct {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  min-width: 0;
  color: var(--text);
  text-decoration: none;
}
.mod__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.mod__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.mod__handle {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.mod__domain {
  font-family: var(--font-mono, monospace);
  font-size: var(--text-sm);
  overflow-wrap: anywhere;
}
.mod__more {
  text-align: center;
}
.mod__form {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  max-width: 26rem;
}
@media (max-width: 768px) {
  .mod__row {
    align-items: flex-start;
    flex-direction: column;
  }
}
</style>
