<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import Avatar from "@/components/ui/Avatar.vue";
import Menu, { type MenuItem } from "@/components/ui/Menu.vue";
import Modal from "@/components/ui/Modal.vue";
import Tooltip from "@/components/ui/Tooltip.vue";
import CwBlock from "./CwBlock.vue";
import StatusMedia from "./StatusMedia.vue";
import StatusQuote from "./StatusQuote.vue";
import StatusPoll from "./StatusPoll.vue";
import StatusFooter from "./StatusFooter.vue";
import ReactionPicker from "./ReactionPicker.vue";
import ComposeBox from "@/components/compose/ComposeBox.vue";
import { sanitizeHtml } from "@/utils/sanitize";
import { formatAbsoluteDate, formatRelativeDate } from "@/utils/relativeDate";
import { ApiError } from "@/api/errors";
import { FontAwesomeIcon } from "@/icons";
import { deleteStatus, react, unreact } from "@/api/statuses";
import { blockAccount, muteAccount } from "@/api/accounts";
import { blockDomain } from "@/api/domains";
import { fileReport } from "@/api/reports";
import { adminAccountAction, adminStatusAction } from "@/api/admin";
import { useAuthStore } from "@/stores/auth";
import { useComposeStore } from "@/stores/compose";
import type { Status } from "@/api/entities";

const props = defineProps<{
  status: Status;
  /** DM threads reply through the page's own composer as a direct
   * message — no inline form and never a public reply to a DM. */
  directReply?: boolean;
}>();
const emit = defineEmits<{ removed: [id: string] }>();
const { t } = useI18n();
const router = useRouter();
const auth = useAuthStore();
const compose = useComposeStore();

// Boosts wrap the inner status — display it, keep the booster line.
const shown = computed(() => props.status.reblog ?? props.status);
// The federated `RE:` paragraph stays in the HTML for Pleroma-family
// renderers that don't read the quote field — when our embedded card
// renders the target it would only duplicate it.
const shownContent = computed(() =>
  shown.value.quote?.quoted_status
    ? shown.value.content.replace(
        /<p>\s*RE:\s*<a\b[^>]*>[\s\S]*?<\/a>\s*<\/p>\s*$/,
        "",
      )
    : shown.value.content,
);

// The whole card is the permalink's click target, Mastodon-style —
// except interactive elements (links, buttons, the composer) and an
// in-progress text selection.
function onCardClick(event: MouseEvent) {
  const target = event.target as HTMLElement | null;
  if (
    target?.closest(
      "a, button, input, textarea, select, label, audio, video, .status-card__reply, .modal__backdrop, .menu",
    )
  ) {
    return;
  }
  if (window.getSelection()?.toString()) return;
  void router.push(`/statuses/${shown.value.id}`);
}
const booster = computed(() =>
  props.status.reblog ? props.status.account : null,
);

const account = computed(() => shown.value.account);
const remoteDomain = computed(() => {
  const acct = account.value.acct;
  return acct.includes("@") ? acct.split("@").slice(1).join("@") : null;
});
const faviconFailed = ref(false);
const pickerOpen = ref(false);
const reportOpen = ref(false);
const reportComment = ref("");
const actionError = ref("");

// Card URLs are scheme-checked upstream (`preview.rs` only writes
// `preview_cards` rows for `valid_url_schemes` links) — re-checked
// at the sink so a bad or future card payload can't bind an unsafe
// scheme to `href`.
const card = computed(() => {
  const c = shown.value.card;
  return c && /^https?:\/\//i.test(c.url) ? c : null;
});

// Destructive menu entries are routed through a confirm dialog first.
const CONFIRM_KEYS: Record<string, string> = {
  delete: "delete",
  "admin-delete": "adminDeleteInstance",
  "admin-delete-profile": "adminDeleteProfile",
  "admin-suspend": "adminSuspend",
  "admin-delete-user": "adminDeleteUser",
  "admin-limit": "adminLimit",
};
const pendingAction = ref<{ text: string; run: () => void } | null>(null);

function confirmThen(text: string, run: () => void) {
  pendingAction.value = { text, run };
}

async function runPending() {
  const action = pendingAction.value;
  pendingAction.value = null;
  action?.run();
}

/** Reply/quote/mention/DM replace the compose draft — ask first when
 * the user has an unsaved draft. */
function guardDraft(action: () => void) {
  if (compose.isDirty) confirmThen(t("compose.discardText"), action);
  else action();
}

function fail(e: unknown) {
  actionError.value = e instanceof ApiError ? e.message : String(e);
}

const menuItems = computed<MenuItem[]>(() => {
  const items: MenuItem[] = [
    { id: "copy", label: t("status.copyLink"), icon: "link" },
    {
      id: "open",
      label: t("status.openTab"),
      icon: "arrow-up-right-from-square",
    },
  ];
  if (auth.isLoggedIn) {
    items.push(
      { id: "mention", label: t("status.mention"), icon: "at" },
      { id: "dm", label: t("status.dm"), icon: "envelope" },
      {
        id: "mute",
        label: t("status.mute", { user: `@${account.value.acct}` }),
        icon: "volume-xmark",
      },
      {
        id: "block",
        label: t("status.block", { user: `@${account.value.acct}` }),
        icon: "ban",
      },
      {
        id: "report",
        label: t("status.report", { user: `@${account.value.acct}` }),
        icon: "flag",
      },
    );
    if (remoteDomain.value)
      items.push({
        id: "block-domain",
        label: t("status.blockDomain", { domain: remoteDomain.value }),
        icon: "globe",
      });
    if (auth.account?.id === account.value.id) {
      items.push(
        { id: "edit", label: t("status.edit"), icon: "pen" },
        { id: "delete", label: t("status.delete"), icon: "trash-can" },
      );
    }
    if (auth.isStaff) {
      items.push(
        {
          id: "mark-sensitive",
          label: t("status.adminMarkSensitive"),
          icon: "eye-slash",
        },
        {
          id: "admin-delete",
          label: t("status.adminDeleteInstance"),
          icon: "trash-can",
        },
      );
      if (!remoteDomain.value)
        items.push({
          id: "admin-delete-profile",
          label: t("status.adminDeleteProfile"),
          icon: "user-xmark",
        });
      items.push(
        {
          id: "admin-limit",
          label: t("status.adminLimit"),
          icon: "comment-slash",
        },
        {
          id: "admin-suspend",
          label: t("status.adminSuspend"),
          icon: "user-slash",
        },
      );
      if (auth.isAdmin)
        items.push({
          id: "admin-delete-user",
          label: t("status.adminDeleteUser"),
          icon: "user-minus",
        });
    }
  }
  return items;
});

function onMenu(item: MenuItem) {
  actionError.value = "";
  const confirmKey = CONFIRM_KEYS[item.id];
  if (confirmKey) {
    confirmThen(
      t(`status.confirm.${confirmKey}`, { user: `@${account.value.acct}` }),
      () => void runAction(item),
    );
    return;
  }
  if (item.id === "mention" || item.id === "dm") {
    guardDraft(() => void runAction(item));
    return;
  }
  void runAction(item);
}

async function runAction(item: MenuItem) {
  const s = shown.value;
  const id = s.account.id;
  try {
    switch (item.id) {
      case "copy":
        await navigator.clipboard.writeText(s.url ?? s.uri);
        break;
      case "open":
        window.open(s.url ?? s.uri, "_blank", "noopener");
        break;
      case "mention":
        compose.openMention(s.account.acct);
        await router.push("/compose");
        break;
      case "dm":
        compose.openDirect(s.account.acct);
        await router.push("/compose");
        break;
      case "mute":
        await muteAccount(id);
        break;
      case "block":
        await blockAccount(id);
        break;
      case "block-domain":
        if (remoteDomain.value) await blockDomain(remoteDomain.value);
        break;
      case "report":
        reportComment.value = "";
        reportOpen.value = true;
        break;
      case "edit":
        await router.push({ path: "/compose", query: { edit: s.id } });
        break;
      case "delete":
        await deleteStatus(s.id);
        emit("removed", s.id);
        break;
      case "mark-sensitive":
        await adminStatusAction(s.id, "sensitive");
        // `shown` aliases the timeline store's reactive entity — the
        // write-through updates every card sharing it (see the same
        // pattern in StatusFooter).
        s.sensitive = true;
        break;
      case "admin-delete":
      case "admin-delete-profile":
        await adminStatusAction(s.id, "delete");
        emit("removed", s.id);
        break;
      case "admin-limit":
        await adminAccountAction(id, { type: "limit" });
        break;
      case "admin-suspend":
        await adminAccountAction(id, { type: "suspend" });
        break;
      case "admin-delete-user":
        await adminAccountAction(id, { type: "delete" });
        break;
    }
  } catch (e) {
    fail(e);
  }
}

async function submitReport() {
  try {
    await fileReport({
      account_id: Number(shown.value.account.id),
      status_ids: [Number(shown.value.id)],
      comment: reportComment.value || undefined,
    });
    reportOpen.value = false;
  } catch (e) {
    fail(e);
  }
}

async function onReact(emoji: string) {
  pickerOpen.value = false;
  const reactions = shown.value.pleroma?.emoji_reactions ?? [];
  const existing = reactions.find(
    (r) => r.name === emoji || r.name === `:${emoji}:`,
  );
  try {
    if (existing?.me) await unreact(shown.value.id, emoji);
    else await react(shown.value.id, emoji);
  } catch (e) {
    fail(e);
  }
}
</script>

<template>
  <article class="status-card" :data-status-id="status.id" @click="onCardClick">
    <p v-if="booster" class="status-card__boost">
      <FontAwesomeIcon icon="retweet" />
      {{
        t("status.boosted", { name: booster.display_name || booster.username })
      }}
    </p>

    <header class="status-card__header">
      <router-link :to="`/@${account.acct}`" class="status-card__author">
        <Avatar
          :src="account.avatar"
          :name="account.display_name || account.username"
          size="md"
        />
        <div class="status-card__names">
          <span class="status-card__display">{{
            account.display_name || account.username
          }}</span>
          <span class="status-card__acct">@{{ account.acct }}</span>
        </div>
      </router-link>
      <img
        v-if="remoteDomain && !faviconFailed"
        :src="`https://${remoteDomain}/favicon.ico`"
        class="status-card__favicon"
        :alt="remoteDomain"
        loading="lazy"
        @error="faviconFailed = true"
      />
      <Tooltip :text="formatAbsoluteDate(shown.created_at)">
        <time class="status-card__date" :datetime="shown.created_at">{{
          formatRelativeDate(shown.created_at)
        }}</time>
      </Tooltip>
      <Menu :items="menuItems" :label="t('status.more')" @select="onMenu" />
    </header>

    <p v-if="actionError" class="status-card__error">{{ actionError }}</p>

    <CwBlock :spoiler-text="shown.spoiler_text">
      <!-- eslint-disable-next-line vue/no-v-html -->
      <div class="status-card__content" v-html="sanitizeHtml(shownContent)" />

      <StatusMedia
        :media="shown.media_attachments"
        :sensitive="shown.sensitive && !shown.spoiler_text"
      />
      <StatusQuote
        :status="shown"
        @reply="
          (s) =>
            guardDraft(() =>
              props.directReply
                ? compose.openDirectReply(s)
                : compose.openReply(s),
            )
        "
        @quote="(s) => guardDraft(() => compose.openQuote(s))"
      />
      <StatusPoll :status="shown" />
    </CwBlock>

    <a
      v-if="card"
      :href="card.url"
      class="status-card__preview"
      target="_blank"
      rel="noopener"
    >
      <img
        v-if="card.image"
        :src="card.image"
        alt=""
        class="status-card__preview-img"
      />
      <span class="status-card__preview-body">
        <strong>{{ card.title }}</strong>
        <span class="status-card__preview-desc">{{ card.description }}</span>
        <span class="status-card__preview-provider">{{
          card.provider_name
        }}</span>
      </span>
    </a>

    <div
      v-if="shown.pleroma?.emoji_reactions?.length"
      class="status-card__reactions"
    >
      <button
        v-for="r in shown.pleroma.emoji_reactions"
        :key="r.name"
        type="button"
        class="status-card__reaction"
        :class="{ 'status-card__reaction--me': r.me }"
        :disabled="!auth.isLoggedIn"
        @click="onReact(r.name)"
      >
        <img
          v-if="r.url"
          :src="r.url"
          :alt="r.name"
          class="status-card__reaction-img"
        />
        <template v-else>{{ r.name }}</template>
        {{ r.count }}
      </button>
    </div>

    <div class="status-card__footer-wrap">
      <StatusFooter
        :status="shown"
        @reply="
          (s) =>
            guardDraft(() =>
              props.directReply
                ? compose.openDirectReply(s)
                : compose.openReply(s),
            )
        "
        @quote="(s) => guardDraft(() => compose.openQuote(s))"
        @react="pickerOpen = !pickerOpen"
      />
      <ReactionPicker v-if="pickerOpen" @select="onReact" />
    </div>

    <ComposeBox
      v-if="!directReply && compose.replyTargetId === shown.id"
      class="status-card__reply"
    />

    <Modal
      v-if="pendingAction"
      :title="t('status.confirm.title')"
      @close="pendingAction = null"
    >
      <p class="status-card__confirm-text">{{ pendingAction.text }}</p>
      <div class="status-card__confirm-btns">
        <button
          type="button"
          class="status-card__report-submit"
          @click="pendingAction = null"
        >
          {{ t("ui.cancel") }}
        </button>
        <button
          type="button"
          class="status-card__report-submit status-card__confirm-yes"
          data-testid="confirm-yes"
          @click="runPending"
        >
          {{ t("ui.confirm") }}
        </button>
      </div>
    </Modal>

    <Modal
      v-if="reportOpen"
      :title="t('status.reportTitle')"
      @close="reportOpen = false"
    >
      <form @submit.prevent="submitReport">
        <textarea
          v-model="reportComment"
          class="status-card__report-text"
          :placeholder="t('status.reportPlaceholder')"
          rows="3"
        />
        <button type="submit" class="status-card__report-submit">
          {{ t("status.reportSubmit") }}
        </button>
      </form>
    </Modal>
  </article>
</template>

<style scoped>
.status-card {
  background: var(--bg-raised);
  padding: var(--space-4);
  border-bottom: 1px solid var(--border-dim);
  cursor: pointer;
}
.status-card a,
.status-card button,
.status-card .status-card__reply {
  cursor: auto;
}
.status-card__boost {
  color: var(--text-muted);
  font-size: var(--text-sm);
  margin: 0 0 var(--space-2);
}
.status-card__header {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin-bottom: var(--space-2);
}
.status-card__author {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  min-width: 0;
  flex: 1;
  color: var(--text);
  text-decoration: none;
}
.status-card__author:hover .status-card__display {
  text-decoration: underline;
}
.status-card__author .avatar {
  cursor: pointer;
}
.status-card__names {
  display: flex;
  flex-direction: column;
  min-width: 0;
  cursor: pointer;
}
.status-card__display {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.status-card__acct {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.status-card__favicon {
  width: 14px;
  height: 14px;
}
.status-card__date {
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.status-card__content {
  overflow-wrap: break-word;
}
.status-card__preview {
  display: flex;
  gap: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-3);
  margin: var(--space-2) 0;
  text-decoration: none;
  color: var(--text);
}
.status-card__preview-img {
  width: 4rem;
  height: 4rem;
  object-fit: cover;
  border-radius: var(--radius-sm);
}
.status-card__preview-body {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.status-card__preview-desc {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
}
.status-card__preview-provider {
  color: var(--text-muted);
  font-size: var(--text-xs);
}
.status-card__reactions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-1);
  margin-top: var(--space-2);
}
.status-card__reaction {
  border: 1px solid var(--border);
  background: var(--bg-raised);
  border-radius: var(--radius-full);
  padding: 2px var(--space-2);
  cursor: pointer;
  font: inherit;
  color: var(--text);
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
}
.status-card__reaction--me {
  border-color: var(--accent);
  background: color-mix(in srgb, var(--accent) 12%, transparent);
}
.status-card__reaction-img {
  width: 1rem;
  height: 1rem;
}
.status-card__footer-wrap {
  position: relative;
}
.status-card__reply {
  margin-top: var(--space-3);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border);
}
.status-card__error {
  color: #c0392b;
  margin: var(--space-1) 0;
}
.status-card__report-text {
  width: 100%;
  font: inherit;
  margin-bottom: var(--space-2);
}
.status-card__confirm-text {
  margin: 0 0 var(--space-3);
}
.status-card__confirm-btns {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-2);
}
.status-card__confirm-yes {
  border-color: var(--accent);
  color: var(--accent-link);
  font-weight: 600;
}
.status-card__report-submit {
  border: 1px solid var(--border);
  background: var(--bg-raised);
  border-radius: var(--radius-md);
  padding: var(--space-1) var(--space-3);
  color: var(--text);
  cursor: pointer;
}
</style>
