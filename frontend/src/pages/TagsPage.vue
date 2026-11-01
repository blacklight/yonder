<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import Button from "@/components/ui/Button.vue";
import Spinner from "@/components/ui/Spinner.vue";
import TagCard from "@/components/tags/TagCard.vue";
import { FontAwesomeIcon } from "@/icons";
import { useAuthStore } from "@/stores/auth";
import { followedTags, trendingTags, type TrendingTagsQuery } from "@/api/tags";
import type { Tag } from "@/api/entities";

const { t } = useI18n();
const auth = useAuthStore();

/** Preset windows all anchor at the most recent hashtag occurrence
 *  (`anchor=latest`) so a quiet instance still ranks its last active
 *  tags; "custom" switches to explicit `from`/`to` dates. */
type RangeKey = "24h" | "7d" | "30d" | "custom";
const RANGE_PERIOD: Record<
  Exclude<RangeKey, "custom">,
  "day" | "week" | "month"
> = {
  "24h": "day",
  "7d": "week",
  "30d": "month",
};
/** Page sizes stay within the stock `trends_max_limit` (20) so the
 *  server never clamps a request mid-page. */
const PAGE_SIZES = [5, 10, 20] as const;

const scope = ref<"global" | "local">("global");
const range = ref<RangeKey>("24h");
const customFrom = ref("");
const customTo = ref("");
const pageSize = ref<number>(20);
const page = ref(0);

const tags = ref<Tag[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);
// Lookahead row: one extra result past the page tells whether a next
// page exists without a count endpoint.
const hasMore = ref(false);

function query(): TrendingTagsQuery {
  const q: TrendingTagsQuery = {
    limit: pageSize.value + 1,
    offset: page.value * pageSize.value,
    local: scope.value === "local",
  };
  if (range.value === "custom") {
    if (customFrom.value) q.from = customFrom.value;
    if (customTo.value) q.to = customTo.value;
  } else {
    q.period = RANGE_PERIOD[range.value];
    q.anchor = "latest";
  }
  return q;
}

async function load() {
  loading.value = true;
  error.value = null;
  try {
    const rows = await trendingTags(query());
    hasMore.value = rows.length > pageSize.value;
    tags.value = rows.slice(0, pageSize.value);
  } catch (e) {
    error.value = e instanceof Error ? e.message : t("common.error");
    tags.value = [];
    hasMore.value = false;
  } finally {
    loading.value = false;
  }
}

function refresh() {
  page.value = 0;
  void load();
}

// Switching *to* a preset reloads; entering "custom" waits for the
// Apply click so half-typed dates don't fire a query per keystroke.
watch(range, (v) => {
  if (v !== "custom") refresh();
});
watch([scope, pageSize], refresh);

function applyCustom() {
  refresh();
}

function prevPage() {
  if (page.value > 0) {
    page.value -= 1;
    void load();
  }
}
function nextPage() {
  if (hasMore.value) {
    page.value += 1;
    void load();
  }
}

// --- Followed hashtags -------------------------------------------------

const followed = ref<Tag[]>([]);
const followedNext = ref<string | undefined>();
const followedLoading = ref(false);
const followedLoaded = ref(false);

function cursor(url?: string) {
  if (!url) return undefined;
  return new URL(url, location.origin).searchParams.get("max_id") ?? undefined;
}

async function loadFollowed() {
  if (!auth.isLoggedIn || followedLoading.value) return;
  followedLoading.value = true;
  try {
    const page = await followedTags({ limit: 20 });
    followed.value = page.items;
    followedNext.value = page.next;
  } catch {
    followed.value = [];
  } finally {
    followedLoading.value = false;
    followedLoaded.value = true;
  }
}

async function moreFollowed() {
  if (!followedNext.value || followedLoading.value) return;
  followedLoading.value = true;
  try {
    const page = await followedTags({
      limit: 20,
      max_id: cursor(followedNext.value),
    });
    followed.value = [...followed.value, ...page.items];
    followedNext.value = page.next;
  } finally {
    followedLoading.value = false;
  }
}

/** A card's follow toggle can drop a tag out of the followed list. */
function onTagChange(updated: Tag) {
  if (!updated.following) {
    followed.value = followed.value.filter((tg) => tg.id !== updated.id);
  }
}

const showPager = computed(() => page.value > 0 || hasMore.value);

onMounted(() => {
  void load();
  void loadFollowed();
});
</script>

<template>
  <div class="tags-page">
    <h1 class="tags-page__title">{{ t("nav.hashtags") }}</h1>

    <section class="tags-page__section" data-testid="trending">
      <header class="tags-page__section-head">
        <h2>{{ t("tags.trending") }}</h2>
      </header>

      <div class="tags-page__controls">
        <label class="tags-page__control">
          <span>{{ t("tags.scope") }}</span>
          <select v-model="scope" data-testid="scope">
            <option value="global">{{ t("tags.scopeGlobal") }}</option>
            <option value="local">{{ t("tags.scopeLocal") }}</option>
          </select>
        </label>
        <label class="tags-page__control">
          <span>{{ t("tags.range") }}</span>
          <select v-model="range" data-testid="range">
            <option value="24h">{{ t("tags.range24h") }}</option>
            <option value="7d">{{ t("tags.range7d") }}</option>
            <option value="30d">{{ t("tags.range30d") }}</option>
            <option value="custom">{{ t("tags.rangeCustom") }}</option>
          </select>
        </label>
        <template v-if="range === 'custom'">
          <label class="tags-page__control">
            <span>{{ t("tags.from") }}</span>
            <input v-model="customFrom" type="date" data-testid="from" />
          </label>
          <label class="tags-page__control">
            <span>{{ t("tags.to") }}</span>
            <input v-model="customTo" type="date" data-testid="to" />
          </label>
          <Button size="sm" data-testid="apply" @click="applyCustom">
            {{ t("tags.apply") }}
          </Button>
        </template>
        <label class="tags-page__control">
          <span>{{ t("tags.perPage") }}</span>
          <select v-model.number="pageSize" data-testid="per-page">
            <option v-for="n in PAGE_SIZES" :key="n" :value="n">
              {{ n }}
            </option>
          </select>
        </label>
      </div>

      <div v-if="loading" class="tags-page__state" role="status">
        <Spinner />
      </div>
      <div v-else-if="error" class="tags-page__state tags-page__error">
        <p>{{ error }}</p>
        <Button size="sm" @click="load">{{ t("timeline.retry") }}</Button>
      </div>
      <p v-else-if="!tags.length" class="tags-page__state">
        {{ t("tags.empty") }}
      </p>
      <div v-else class="tags-page__list">
        <TagCard
          v-for="tag in tags"
          :key="tag.id"
          :tag="tag"
          @change="onTagChange"
        />
      </div>

      <nav v-if="showPager" class="tags-page__pager" aria-label="pagination">
        <Button
          size="sm"
          variant="secondary"
          :disabled="page === 0 || loading"
          data-testid="prev-page"
          @click="prevPage"
        >
          <FontAwesomeIcon icon="chevron-left" />
          {{ t("tags.prev") }}
        </Button>
        <span class="tags-page__page">{{
          t("tags.page", { n: page + 1 })
        }}</span>
        <Button
          size="sm"
          variant="secondary"
          :disabled="!hasMore || loading"
          data-testid="next-page"
          @click="nextPage"
        >
          {{ t("tags.next") }}
          <FontAwesomeIcon icon="chevron-right" />
        </Button>
      </nav>
    </section>

    <section
      v-if="auth.isLoggedIn"
      class="tags-page__section"
      data-testid="followed"
    >
      <header class="tags-page__section-head">
        <h2>{{ t("tags.followed") }}</h2>
      </header>
      <div v-if="followedLoading && !followedLoaded" class="tags-page__state">
        <Spinner />
      </div>
      <p v-else-if="!followed.length" class="tags-page__state">
        {{ t("tags.emptyFollowed") }}
      </p>
      <div v-else class="tags-page__list">
        <TagCard
          v-for="tag in followed"
          :key="tag.id"
          :tag="tag"
          @change="onTagChange"
        />
      </div>
      <Button
        v-if="followedNext"
        size="sm"
        variant="secondary"
        :disabled="followedLoading"
        data-testid="more-followed"
        @click="moreFollowed"
      >
        {{ t("common.loadMore") }}
      </Button>
    </section>
  </div>
</template>

<style scoped>
.tags-page__title {
  margin-bottom: var(--space-4);
}
.tags-page__section {
  margin-bottom: var(--space-6);
}
.tags-page__section-head {
  margin-bottom: var(--space-3);
}
.tags-page__section-head h2 {
  font-size: var(--text-lg);
  font-weight: 600;
}
.tags-page__controls {
  display: flex;
  flex-wrap: wrap;
  align-items: end;
  gap: var(--space-3);
  margin-bottom: var(--space-4);
}
.tags-page__control {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  font-size: var(--text-sm);
  color: var(--text-muted);
}
.tags-page__control select,
.tags-page__control input {
  color: var(--text);
  background: var(--bg-raised);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-1) var(--space-2);
}
.tags-page__list {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(16rem, 1fr));
  gap: var(--space-3);
}
.tags-page__state {
  padding: var(--space-4) 0;
  color: var(--text-muted);
}
.tags-page__error p {
  color: var(--danger);
  margin-bottom: var(--space-2);
}
.tags-page__pager {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin-top: var(--space-4);
}
.tags-page__page {
  font-size: var(--text-sm);
  color: var(--text-muted);
}
</style>
