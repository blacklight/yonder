<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import {
  blockAccount,
  follow,
  getAccount,
  getAccountLists,
  getRelationships,
  lookup,
  muteAccount,
  setNote,
  subscribe,
  unblockAccount,
  unmuteAccount,
  unsubscribe,
  accountStatuses,
} from "@/api/accounts";
import { blockDomain, unblockDomain } from "@/api/domains";
import { fileReport } from "@/api/reports";
import { adminAccountAction, setRole } from "@/api/admin";
import {
  addListMembers,
  createList,
  listLists,
  removeListMembers,
} from "@/api/lists";
import { ApiError } from "@/api/errors";
import type { Account, ListEntity, Relationship, Status } from "@/api/entities";
import { sanitizeHtml, safeHttpUrl } from "@/utils/sanitize";
import { useAuthStore } from "@/stores/auth";
import { useComposeStore } from "@/stores/compose";
import Avatar from "@/components/ui/Avatar.vue";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import Tabs from "@/components/ui/Tabs.vue";
import { FontAwesomeIcon } from "@/icons";
import Menu, { type MenuItem } from "@/components/ui/Menu.vue";
import Modal from "@/components/ui/Modal.vue";
import StatusCard from "@/components/status/StatusCard.vue";
import { useStatusEvents } from "@/composables/useStatusEvents";
import { dropFromStatusList, patchStatusList } from "@/utils/statusStream";

const { t } = useI18n();
const route = useRoute();
const router = useRouter();
const auth = useAuthStore();
const compose = useComposeStore();

const account = ref<Account | null>(null);
const rel = ref<Relationship | null>(null);
const state = ref<"loading" | "ok" | "not-found" | "sign-in" | "error">(
  "loading",
);
const actionError = ref<string | null>(null);
const statuses = ref<Status[]>([]);
// The profile's pinned set — fetched once per account (the endpoint
// returns the capped list, no cursors) and refreshed when a card on
// this page pins/unpins. Renders on the posts tab only.
const pinnedStatuses = ref<Status[]>([]);
const nextCursor = ref<string | undefined>();
const tab = ref<"posts" | "media">("posts");
const tabs = computed(
  () =>
    [
      { key: "posts", label: t("profile.posts") },
      { key: "media", label: t("profile.media") },
    ] as const,
);
const showReplies = ref(false);
const showBoosts = ref(true);
const confirmAction = ref<MenuItem | null>(null);
const noteText = ref("");
const noteOpen = ref(false);
const noteSaving = ref(false);
const noteError = ref<string | null>(null);
const visitedIds = new Set<string>();
const MAX_HOPS = 5;

const bio = computed(() => sanitizeHtml(account.value?.note ?? ""));
const headerUrl = computed(() => safeHttpUrl(account.value?.header));

const FIELD_EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
const FIELD_HANDLE_RE = /^@([\w.]+)@((?:[\w-]+\.)+[\w-]+)$/;

/** Scheme-less display text for a URL (`https://x.example/` → `x.example`). */
function displayUrl(url: string): string {
  const s = url.replace(/^https?:\/\//i, "");
  return s.endsWith("/") && s.indexOf("/") === s.length - 1
    ? s.slice(0, -1)
    : s;
}

interface FieldLink {
  href: string;
  text: string;
  external: boolean;
}

/**
 * The single link a field value boils down to — a bare URL, a sole
 * `<a>` anchor (remote fields arrive pre-rendered), or an email
 * address. Anything more complex renders as sanitized HTML.
 */
function fieldLink(value: string): FieldLink | null {
  const trimmed = value.trim();
  const url = safeHttpUrl(trimmed);
  if (url) return { href: url, text: displayUrl(url), external: true };
  if (FIELD_EMAIL_RE.test(trimmed)) {
    return { href: `mailto:${trimmed}`, text: trimmed, external: false };
  }
  const handle = FIELD_HANDLE_RE.exec(trimmed);
  if (handle) {
    return {
      href: `https://${handle[2]}/@${handle[1]}`,
      text: trimmed,
      external: true,
    };
  }
  if (trimmed.startsWith("<")) {
    const doc = new DOMParser().parseFromString(trimmed, "text/html");
    const nodes = [...doc.body.childNodes].filter(
      (n) => !(n.nodeType === Node.TEXT_NODE && !n.textContent?.trim()),
    );
    const sole = nodes.length === 1 ? nodes[0] : null;
    if (sole instanceof HTMLAnchorElement) {
      const href = safeHttpUrl(sole.getAttribute("href"));
      if (href) {
        const text = sole.textContent?.trim() || href;
        return {
          href,
          text: safeHttpUrl(text) ? displayUrl(text) : text,
          external: true,
        };
      }
    }
  }
  return null;
}

const fieldsView = computed(() =>
  (account.value?.fields ?? []).map((f) => ({
    ...f,
    link: fieldLink(f.value),
  })),
);

const avatarUrl = computed(
  () => safeHttpUrl(account.value?.avatar) ?? undefined,
);
const isOwn = computed(() => auth.account?.id === account.value?.id);
const handle = computed(() => {
  const a = account.value;
  return a ? `@${a.acct}` : "";
});
const menuItems = computed<MenuItem[]>(() => {
  const r = rel.value;
  const a = account.value;
  if (!a || r?.blocked_by) return [];
  const domain = a.acct.split("@")[1];
  const items: MenuItem[] = [
    { id: "mention", label: t("profile.mention"), icon: "at" },
    { id: "dm", label: t("profile.dm"), icon: "envelope" },
    { id: "note", label: t("profile.note"), icon: "note-sticky" },
    { id: "lists", label: t("profile.lists"), icon: "list-ul" },
  ];
  if (domain && safeHttpUrl(a.url))
    items.push({
      id: "open-remote",
      label: t("profile.openOnInstance", { domain }),
      icon: "arrow-up-right-from-square",
    });
  if (r?.following)
    items.push({
      id: "hide-boosts",
      label:
        r.showing_reblogs === false
          ? t("profile.showBoosts")
          : t("profile.hideBoosts"),
      icon: "retweet",
    });
  items.push(
    {
      id: "mute",
      label: r?.muting ? t("profile.unmute") : t("profile.mute"),
      icon: "volume-xmark",
      danger: true,
    },
    {
      id: "block",
      label: r?.blocking ? t("profile.unblock") : t("profile.block"),
      icon: "ban",
      danger: true,
    },
  );
  if (domain)
    items.push({
      id: "domain",
      label: t("profile.blockDomain"),
      icon: "globe",
      danger: true,
    });
  items.push({
    id: "report",
    label: t("profile.report"),
    icon: "flag",
    danger: true,
  });
  if (auth.isStaff && !isOwn.value) {
    items.push(
      {
        id: "admin-limit",
        label: t("profile.adminLimit"),
        icon: "comment-slash",
        danger: true,
      },
      {
        id: "admin-suspend",
        label: t("profile.adminSuspend"),
        icon: "user-slash",
        danger: true,
      },
    );
  }
  if (auth.isAdmin && !isOwn.value) {
    items.push({
      id: "admin-delete",
      label: t("profile.adminDelete"),
      icon: "user-xmark",
      danger: true,
    });
    // Roles live on local accounts only — `set_role` 404s on remote
    // rows (domain IS NULL guard), so offering it there can only fail.
    if (!domain)
      items.push({
        id: "admin-mod",
        label: t("profile.adminMakeMod"),
        icon: "user-shield",
      });
  }
  return items;
});

async function loadStatuses(more = false) {
  if (!account.value) return;
  const max_id =
    more && nextCursor.value
      ? (new URL(nextCursor.value, location.origin).searchParams.get(
          "max_id",
        ) ?? undefined)
      : undefined;
  const page = await accountStatuses(account.value.id, {
    max_id,
    only_media: tab.value === "media" || undefined,
    exclude_replies:
      tab.value === "posts" && !showReplies.value ? true : undefined,
    exclude_reblogs:
      tab.value === "posts" && !showBoosts.value ? true : undefined,
  });
  statuses.value = more ? [...statuses.value, ...page.items] : page.items;
  nextCursor.value = page.next;
}

/** `?pinned=true` — local pins and remote featured items alike.
 * Best-effort: a failed read just leaves the section empty rather
 * than blanking the whole profile. */
async function loadPinned() {
  if (!account.value) return;
  try {
    pinnedStatuses.value = (
      await accountStatuses(account.value.id, { pinned: true })
    ).items;
  } catch {
    pinnedStatuses.value = [];
  }
}

async function resolve(): Promise<void> {
  state.value = "loading";
  actionError.value = null;
  try {
    let a: Account;
    if (route.name === "profile-id") {
      a = await getAccount(String(route.params.id));
    } else if (route.name === "profile-acct") {
      a = await lookup(String(route.params.acct));
    } else {
      // `/profile` without a resolved account — nothing to look up.
      state.value = "not-found";
      return;
    }
    // Follow `moved` in-band. Redirecting via router.replace would
    // re-run the route watcher below and reset the visited set, so an
    // A↔B move cycle would ping-pong forever — the hop cap must live
    // inside a single resolve pass.
    let hops = 0;
    while (a.moved && !visitedIds.has(a.id) && hops < MAX_HOPS) {
      visitedIds.add(a.id);
      a = await lookup(a.moved.acct);
      hops++;
    }
    // Point the URL at the canonical handle only when the chain fully
    // resolved; a capped/looping chain renders where it stopped.
    if (
      !a.moved &&
      route.name === "profile-acct" &&
      String(route.params.acct) !== a.acct
    ) {
      void router.replace(`/@${a.acct}`);
    }
    account.value = a;
    state.value = "ok";
    rel.value = null;
    if (auth.isLoggedIn && !isOwn.value) {
      try {
        rel.value = (await getRelationships([a.id]))[0] ?? null;
      } catch {
        /* relationship is best-effort */
      }
    }
    statuses.value = [];
    pinnedStatuses.value = [];
    nextCursor.value = undefined;
    await Promise.all([loadStatuses(), loadPinned()]);
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) state.value = "sign-in";
    else if (e instanceof ApiError && (e.status === 404 || e.status === 422))
      state.value = "not-found";
    else state.value = "error";
  }
}

/** Surface action failures instead of dropping them on the floor. */
async function tryRun(fn: () => Promise<void>) {
  actionError.value = null;
  try {
    await fn();
  } catch (e) {
    actionError.value =
      e instanceof ApiError && e.status === 409
        ? t("profile.lastAdmin")
        : t("common.error");
  }
}

async function refreshRel() {
  if (!account.value || !auth.isLoggedIn) return;
  rel.value = (await getRelationships([account.value.id]))[0] ?? null;
}

async function toggleFollow() {
  const a = account.value;
  if (!a) return;
  await tryRun(async () => {
    if (rel.value?.following || rel.value?.requested) {
      const { unfollow } = await import("@/api/accounts");
      await unfollow(a.id);
    } else {
      await follow(a.id);
    }
    await refreshRel();
  });
}
async function toggleSubscribe() {
  const a = account.value;
  if (!a) return;
  await tryRun(async () => {
    if (rel.value?.subscribing || rel.value?.notifying) await unsubscribe(a.id);
    else await subscribe(a.id);
    await refreshRel();
  });
}

const DESTRUCTIVE = new Set([
  "block",
  "domain",
  "report",
  "admin-limit",
  "admin-suspend",
  "admin-delete",
  "admin-mod",
]);

async function onMenu(item: MenuItem) {
  if (DESTRUCTIVE.has(item.id)) {
    confirmAction.value = item;
    return;
  }
  await run(item.id);
}

async function run(id: string) {
  const a = account.value;
  if (!a) return;
  await tryRun(async () => {
    switch (id) {
      case "mention":
        compose.openMention(a.acct);
        router.push("/compose");
        break;
      case "dm":
        compose.openDirect(a.acct);
        router.push("/compose");
        break;
      case "note":
        openNote();
        break;
      case "lists":
        openListPicker();
        break;
      case "open-remote": {
        const url = safeHttpUrl(a.url);
        if (url) window.open(url, "_blank", "noopener,noreferrer");
        break;
      }
      case "hide-boosts":
        await follow(a.id, {
          reblogs: !(rel.value?.showing_reblogs !== false),
        });
        await refreshRel();
        break;
      case "mute":
        if (rel.value?.muting) await unmuteAccount(a.id);
        else await muteAccount(a.id);
        await refreshRel();
        break;
      case "block":
        if (rel.value?.blocking) await unblockAccount(a.id);
        else await blockAccount(a.id);
        await refreshRel();
        break;
      case "domain": {
        const domain = a.acct.split("@")[1];
        if (rel.value?.domain_blocking) await unblockDomain(domain);
        else await blockDomain(domain);
        await refreshRel();
        break;
      }
      case "report":
        await fileReport({ account_id: Number(a.id) });
        break;
      case "admin-limit":
        await adminAccountAction(a.id, { type: "limit" });
        break;
      case "admin-suspend":
        await adminAccountAction(a.id, { type: "suspend" });
        break;
      case "admin-delete":
        await adminAccountAction(a.id, { type: "delete" });
        break;
      case "admin-mod":
        await setRole(a.id, "moderator");
        break;
    }
  });
}

const listsOpen = ref(false);
const listsLoading = ref(false);
const listsError = ref<string | null>(null);
const myLists = ref<ListEntity[]>([]);
const listMemberIds = ref<Set<string>>(new Set());
const listsBusy = ref<Set<string>>(new Set());
const newListTitle = ref("");
const listCreating = ref(false);

function openListPicker() {
  listsOpen.value = true;
  listsLoading.value = true;
  listsError.value = null;
  newListTitle.value = "";
  const a = account.value;
  if (!a) return;
  Promise.all([listLists(), getAccountLists(a.id)])
    .then(([mine, memberOf]) => {
      myLists.value = mine;
      listMemberIds.value = new Set(memberOf.map((l) => l.id));
    })
    .catch(() => {
      listsError.value = t("common.error");
    })
    .finally(() => {
      listsLoading.value = false;
    });
}

async function toggleListMembership(list: ListEntity) {
  const a = account.value;
  if (!a || listsBusy.value.has(list.id)) return;
  const busy = new Set(listsBusy.value);
  busy.add(list.id);
  listsBusy.value = busy;
  const member = listMemberIds.value.has(list.id);
  try {
    if (member) await removeListMembers(list.id, [a.id]);
    else await addListMembers(list.id, [a.id]);
    const next = new Set(listMemberIds.value);
    if (member) next.delete(list.id);
    else next.add(list.id);
    listMemberIds.value = next;
  } catch {
    listsError.value = t("common.error");
  } finally {
    const done = new Set(listsBusy.value);
    done.delete(list.id);
    listsBusy.value = done;
  }
}

// Creating from the picker adds the account straight onto the new
// list — that's the intent behind opening "Add to list".
async function createListFromPicker() {
  const a = account.value;
  const title = newListTitle.value.trim();
  if (!a || !title || listCreating.value) return;
  listCreating.value = true;
  listsError.value = null;
  try {
    const l = await createList({ title });
    myLists.value = [...myLists.value, l];
    newListTitle.value = "";
    await addListMembers(l.id, [a.id]);
    listMemberIds.value = new Set(listMemberIds.value).add(l.id);
  } catch {
    listsError.value = t("common.error");
  } finally {
    listCreating.value = false;
  }
}

function openNote() {
  noteText.value = rel.value?.note ?? "";
  noteError.value = null;
  noteOpen.value = true;
}

async function saveNote() {
  if (!account.value || noteSaving.value) return;
  noteSaving.value = true;
  noteError.value = null;
  try {
    await setNote(account.value.id, noteText.value);
    noteOpen.value = false;
    await refreshRel();
  } catch {
    noteError.value = t("common.error");
  } finally {
    noteSaving.value = false;
  }
}

async function confirmRun() {
  const item = confirmAction.value;
  confirmAction.value = null;
  if (item) await run(item.id);
}

watch([account, state], () => {
  const app = t("app.name");
  const a = account.value;
  // Mastodon's profile title: `Display Name (@user@domain)`. A blank
  // display name falls back to the username, matching the profile h1.
  if (state.value === "ok" && a) {
    const name = a.display_name || a.username;
    document.title = `${name} (@${a.acct}) — ${app}`;
  } else if (state.value !== "loading") {
    document.title = `${t("route.profile.title")} — ${app}`;
  }
});

// A card's own delete — or a streamed one — drops the status from
// both lists without waiting for a reload.
function dropRemoved(id: string) {
  statuses.value = dropFromStatusList(statuses.value, id);
  pinnedStatuses.value = dropFromStatusList(pinnedStatuses.value, id);
}

// Live edits/deletes land on the displayed statuses — the profile
// timeline is not store-backed, so it patches its own list.
useStatusEvents({
  onStatus: (s) => {
    statuses.value = patchStatusList(statuses.value, s);
    pinnedStatuses.value = patchStatusList(pinnedStatuses.value, s);
  },
  onDelete: dropRemoved,
});

watch(
  () => route.fullPath,
  () => {
    visitedIds.clear();
    void resolve();
  },
  { immediate: true },
);
watch([tab, showReplies, showBoosts], () => {
  statuses.value = [];
  nextCursor.value = undefined;
  void loadStatuses();
});
</script>

<template>
  <main class="page profile">
    <p v-if="state === 'loading'" aria-busy="true">{{ t("common.loading") }}</p>
    <p v-else-if="state === 'not-found'">{{ t("profile.notFound") }}</p>
    <p v-else-if="state === 'error'" role="alert">
      {{ t("profile.loadError") }}
    </p>
    <div v-else-if="state === 'sign-in'" class="profile__signin">
      <p>{{ t("profile.signInToView") }}</p>
      <router-link to="/login">{{ t("nav.signIn") }}</router-link>
    </div>
    <template v-else-if="account">
      <header class="profile__header">
        <img v-if="headerUrl" :src="headerUrl" class="profile__banner" alt="" />
        <div class="profile__top">
          <Avatar
            :src="avatarUrl"
            :name="account.display_name || account.acct"
            size="lg"
            class="profile__avatar"
          />
          <div v-if="!rel?.blocked_by" class="profile__actions">
            <router-link
              v-if="isOwn"
              to="/settings/profile"
              class="profile__edit"
              >{{ t("profile.edit") }}</router-link
            >
            <template v-else-if="auth.isLoggedIn">
              <Button @click="toggleFollow">
                <FontAwesomeIcon
                  :icon="rel?.following ? 'user-check' : 'user-plus'"
                />
                {{
                  rel?.following
                    ? t("profile.unfollow")
                    : rel?.requested
                      ? t("profile.requested")
                      : t("profile.follow")
                }}
              </Button>
              <Button
                v-if="rel?.following"
                variant="secondary"
                @click="toggleSubscribe"
              >
                {{
                  rel.subscribing || rel.notifying
                    ? t("profile.unsubscribe")
                    : t("profile.subscribe")
                }}
              </Button>
            </template>
            <Menu
              v-if="auth.isLoggedIn && !isOwn"
              :items="menuItems"
              :label="t('profile.actions')"
              @select="onMenu"
            />
          </div>
        </div>
        <h1 class="profile__name">
          {{ account.display_name || account.username }}
        </h1>
        <p class="profile__handle">{{ handle }}</p>
        <p v-if="rel?.followed_by" class="profile__badge">
          {{ t("profile.followsYou") }}
        </p>
        <p v-if="rel?.blocked_by" class="profile__badge profile__badge--danger">
          {{ t("profile.blockedYou") }}
        </p>
        <div class="profile__counters">
          <router-link
            :to="`/@${account.acct}/followers`"
            class="profile__counter"
          >
            <i18n-t
              keypath="profile.followers"
              :plural="account.followers_count"
            >
              <template #count>
                <strong class="profile__counter-num">{{
                  account.followers_count
                }}</strong>
              </template>
            </i18n-t>
          </router-link>
          <router-link
            :to="`/@${account.acct}/following`"
            class="profile__counter"
          >
            <i18n-t
              keypath="profile.following"
              :plural="account.following_count"
            >
              <template #count>
                <strong class="profile__counter-num">{{
                  account.following_count
                }}</strong>
              </template>
            </i18n-t>
          </router-link>
          <span class="profile__counter">
            <i18n-t keypath="profile.statuses" :plural="account.statuses_count">
              <template #count>
                <strong class="profile__counter-num">{{
                  account.statuses_count
                }}</strong>
              </template>
            </i18n-t>
          </span>
        </div>
        <p v-if="actionError" class="profile__alert" role="alert">
          {{ actionError }}
        </p>
        <div v-if="bio" class="profile__bio" v-html="bio" />
        <dl v-if="account.fields.length" class="profile__fields">
          <template v-for="f in fieldsView" :key="f.name">
            <dt>{{ f.name }}</dt>
            <dd :class="{ 'profile__field--verified': f.verified_at }">
              <a
                v-if="f.link"
                :href="f.link.href"
                :target="f.link.external ? '_blank' : undefined"
                rel="nofollow noopener noreferrer"
                >{{ f.link.text }}</a
              >
              <FontAwesomeIcon
                v-if="f.verified_at"
                icon="circle-check"
                class="verified"
                :title="t('profile.verified')"
              />
              <span v-else-if="!f.link" v-html="sanitizeHtml(f.value)" />
            </dd>
          </template>
        </dl>
        <div v-if="rel?.note" class="profile__note">
          <div class="profile__note-head">
            <h2 class="profile__note-title">
              <FontAwesomeIcon icon="note-sticky" />
              {{ t("profile.noteTitle") }}
            </h2>
            <Button variant="secondary" size="sm" @click="openNote">
              {{ t("status.edit") }}
            </Button>
          </div>
          <p class="profile__note-text">{{ rel.note }}</p>
        </div>
      </header>
      <Tabs v-model="tab" :items="tabs" :label="t('profile.tabs')" />
      <div v-if="tab === 'posts'" class="profile__toggles">
        <label
          ><input v-model="showReplies" type="checkbox" />
          {{ t("profile.showReplies") }}</label
        >
        <label
          ><input v-model="showBoosts" type="checkbox" />
          {{ t("profile.showBoostsTab") }}</label
        >
      </div>
      <div class="timeline">
        <template v-if="tab === 'posts'">
          <StatusCard
            v-for="s in pinnedStatuses"
            :key="`pinned-${s.id}`"
            :status="s"
            pinned-context
            @removed="dropRemoved"
            @pinned="loadPinned"
          />
        </template>
        <StatusCard
          v-for="s in statuses"
          :key="s.id"
          :status="s"
          @removed="dropRemoved"
          @pinned="loadPinned"
        />
        <div v-if="nextCursor" class="profile__more">
          <Button variant="secondary" @click="loadStatuses(true)">
            {{ t("common.loadMore") }}
          </Button>
        </div>
      </div>
    </template>
    <Modal
      v-if="confirmAction"
      :title="t('profile.confirmTitle')"
      @close="confirmAction = null"
    >
      <p>{{ confirmAction.label }}</p>
      <Button variant="danger" @click="confirmRun">{{
        t("common.confirm")
      }}</Button>
    </Modal>
    <Modal
      v-if="noteOpen"
      :title="t('profile.noteTitle')"
      @close="noteOpen = false"
    >
      <textarea
        v-model="noteText"
        class="profile__note-input"
        rows="4"
        maxlength="2000"
        :placeholder="t('profile.notePlaceholder')"
        :aria-label="t('profile.noteTitle')"
      />
      <p v-if="noteError" class="profile__note-error" role="alert">
        {{ noteError }}
      </p>
      <template #footer>
        <Button variant="secondary" @click="noteOpen = false">
          {{ t("ui.cancel") }}
        </Button>
        <Button :loading="noteSaving" @click="saveNote">
          {{ t("common.save") }}
        </Button>
      </template>
    </Modal>
    <Modal
      v-if="listsOpen"
      :title="t('profile.listsTitle')"
      @close="listsOpen = false"
    >
      <p v-if="listsLoading" aria-busy="true">{{ t("common.loading") }}</p>
      <template v-else>
        <form class="profile__lists-new" @submit.prevent="createListFromPicker">
          <Input
            v-model="newListTitle"
            :label="t('lists.titleLabel')"
            :placeholder="t('profile.listsNewPlaceholder')"
          />
          <Button
            type="submit"
            size="sm"
            :loading="listCreating"
            :disabled="!newListTitle.trim()"
          >
            {{ t("lists.create") }}
          </Button>
        </form>
        <ul v-if="myLists.length" class="profile__lists">
          <li v-for="l in myLists" :key="l.id">
            <label class="profile__list-row">
              <input
                type="checkbox"
                :checked="listMemberIds.has(l.id)"
                :disabled="listsBusy.has(l.id)"
                @change="toggleListMembership(l)"
              />
              <span class="profile__list-title">{{ l.title }}</span>
              <FontAwesomeIcon
                v-if="listMemberIds.has(l.id)"
                icon="check"
                class="profile__list-check"
              />
            </label>
          </li>
        </ul>
        <div v-else class="profile__lists-empty">
          <p>{{ t("profile.listsEmpty") }}</p>
          <router-link to="/lists" @click="listsOpen = false">{{
            t("profile.listsManage")
          }}</router-link>
        </div>
        <p v-if="listsError" class="profile__lists-error" role="alert">
          {{ listsError }}
        </p>
      </template>
    </Modal>
  </main>
</template>

<style scoped>
.profile__banner {
  display: block;
  width: 100%;
  aspect-ratio: 3 / 1;
  max-height: 12rem;
  object-fit: cover;
  border-radius: var(--radius-lg);
  background: var(--bg-sunken);
}
.profile__top {
  display: flex;
  align-items: flex-end;
  justify-content: space-between;
  flex-wrap: wrap;
  gap: var(--space-3);
  margin-top: var(--space-4);
}
.profile__banner + .profile__top {
  margin-top: calc(-1 * var(--space-7));
}
.profile__avatar {
  flex: none;
  margin-left: var(--space-2);
  border: 4px solid var(--bg);
}
.profile__actions {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: flex-end;
  gap: var(--space-2);
}
.profile__edit {
  display: inline-flex;
  align-items: center;
  padding: var(--space-2) var(--space-4);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  color: var(--text);
  font-weight: 600;
  text-decoration: none;
}
.profile__name {
  margin: var(--space-3) 0 0;
  font-size: var(--text-2xl);
  line-height: var(--leading-tight);
  overflow-wrap: anywhere;
}
.profile__handle {
  margin: var(--space-1) 0 0;
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow-wrap: anywhere;
}
.profile__badge {
  display: inline-block;
  margin: var(--space-2) 0 0;
  padding: 0 var(--space-2);
  border-radius: var(--radius-sm);
  background: var(--bg-sunken);
  color: var(--text-muted);
  font-size: var(--text-xs);
  font-weight: 600;
  text-transform: uppercase;
}
.profile__badge--danger {
  color: var(--danger);
}
.profile__counters {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2) var(--space-4);
  margin-top: var(--space-3);
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.profile__counter {
  color: inherit;
  text-decoration: none;
  text-transform: capitalize;
}
.profile__counter-num {
  color: var(--text);
  font-weight: 600;
}
.profile__counter:hover {
  color: var(--text);
  text-decoration: underline;
}
.profile__alert {
  margin: var(--space-3) 0 0;
  color: var(--danger);
}
.profile__bio {
  font-size: var(--text-sm);
  margin-top: var(--space-4);
  overflow-wrap: anywhere;
}
.profile__fields {
  background: var(--bg-raised);
  display: grid;
  grid-template-columns: minmax(7rem, auto) minmax(0, 1fr);
  gap: var(--space-2) var(--space-4);
  margin: var(--space-4) 0 0;
  padding: var(--space-4) var(--space-3);
  border: 1px solid var(--border);
}
.profile__fields dt {
  font-weight: 600;
  color: var(--text-muted);
}
.profile__fields dd {
  margin: 0;
  min-width: 0;
  overflow-wrap: anywhere;
}
.profile__toggles {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2) var(--space-4);
  margin-bottom: var(--space-4);
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.profile__toggles label {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
}
.timeline {
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  overflow: hidden;
}
.profile__more {
  margin-top: var(--space-4);
  text-align: center;
}
.profile__signin {
  padding: var(--space-6) 0;
  text-align: center;
}
.verified {
  color: var(--success);
}
.profile__fields a {
  text-decoration: none;
}
.profile__fields a:hover {
  text-decoration: underline;
}
.profile__field--verified a {
  font-weight: 600;
  margin-right: var(--space-2);
}
.profile__note {
  margin-top: var(--space-4);
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-raised);
}
.profile__note-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-2);
}
.profile__note-title {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  margin: 0;
  color: var(--text-muted);
  font-size: var(--text-sm);
  font-weight: 600;
  text-transform: uppercase;
}
.profile__note-text {
  margin: var(--space-2) 0 0;
  font-size: var(--text-sm);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.profile__note-input {
  display: block;
  width: 100%;
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg);
  color: var(--text);
  resize: vertical;
}
.profile__note-error {
  margin: var(--space-2) 0 0;
  color: var(--danger);
  font-size: var(--text-sm);
}
.profile__lists-new {
  display: flex;
  align-items: flex-end;
  gap: var(--space-2);
  margin-bottom: var(--space-3);
}
.profile__lists-new :deep(.field) {
  flex: 1;
  min-width: 0;
}
.profile__lists {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.profile__list-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-2) var(--space-3);
  border-radius: var(--radius-md);
  cursor: pointer;
}
.profile__list-row:hover {
  background: var(--bg-sunken);
}
.profile__list-title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.profile__list-check {
  color: var(--accent);
}
.profile__lists-empty {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.profile__lists-empty p {
  margin: 0;
  color: var(--text-muted);
}
.profile__lists-error {
  margin: var(--space-2) 0 0;
  color: var(--danger);
  font-size: var(--text-sm);
}
</style>
