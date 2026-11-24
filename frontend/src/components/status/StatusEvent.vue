<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { FontAwesomeIcon } from "@/icons";
import Button from "@/components/ui/Button.vue";
import AccountChip from "@/components/ui/AccountChip.vue";
import Avatar from "@/components/ui/Avatar.vue";
import Modal from "@/components/ui/Modal.vue";
import {
  eventDecideRequest,
  eventInvitees,
  eventParticipations,
  eventRemoveParticipant,
  eventRsvp,
  type EventParticipationRow,
} from "@/api/events";
import { requestBlob } from "@/api/client";
import { saveBlob } from "@/composables/useExports";
import { useToastsStore } from "@/stores/toasts";
import { useAuthStore } from "@/stores/auth";
import { useInstanceStore } from "@/stores/instance";
import { search } from "@/api/search";
import type { Account, EventLocation, Status } from "@/api/entities";
import { browserTimeZone } from "@/utils/timezone";

const props = defineProps<{ status: Status }>();
const { t, locale } = useI18n();
const toasts = useToastsStore();
const auth = useAuthStore();
const instance = useInstanceStore();
onMounted(() => void instance.ensure());

const ev = computed(() => props.status.yonder?.event ?? null);

// --- viewer participation (local override after an RSVP call) -------
interface Participation {
  state?: string;
  role?: string;
  can_join?: boolean;
  reason?: string | null;
}
const participation = ref<Participation | null>(null);
const synced = computed<Participation | null>(
  () =>
    participation.value ??
    (ev.value?.participation as Participation | null | undefined) ??
    null,
);
const isOrganizer = computed(() => synced.value?.reason === "own_event");
const rsvpProtocol = computed(() => ev.value?.rsvp_protocol ?? "join");

// --- date chip -------------------------------------------------------
// Start/End render on separate lines as `<date> <time> (<zone>)` —
// the IANA name (e.g. `Europe/Amsterdam`), not a `GMT+1` offset.
const eventZone = computed(() => ev.value?.timezone || browserTimeZone());
const fmt = computed(() => {
  return new Intl.DateTimeFormat(locale.value, {
    timeZone: eventZone.value,
    weekday: "short",
    month: "short",
    day: "numeric",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
});
const whenText = (iso: string) =>
  `${fmt.value.format(new Date(iso))} (${eventZone.value})`;
const viewerFmt = computed(
  () =>
    new Intl.DateTimeFormat(locale.value, {
      timeZone: browserTimeZone(),
      hour: "2-digit",
      minute: "2-digit",
    }),
);
const startLabel = computed(() => {
  const iso = ev.value?.start_time;
  if (!iso) return t("events.postponedNoDate");
  return whenText(iso);
});
const endLabel = computed(() => {
  const e = ev.value;
  if (!e?.end_time || e.end_time_inferred || e.display_end_time === false)
    return null;
  return whenText(e.end_time);
});
/** Viewer-zone clock time, only when it differs from the event zone. */
const viewerTimeLabel = computed(() => {
  const zone = ev.value?.timezone;
  const iso = ev.value?.start_time;
  if (!zone || !iso || zone === browserTimeZone()) return null;
  return viewerFmt.value.format(new Date(iso));
});

// --- locations / links ----------------------------------------------
function osmUrl(lat?: number | null, lon?: number | null): string | null {
  if (lat == null || lon == null) return null;
  return `https://www.openstreetmap.org/?mlat=${lat}&mlon=${lon}#map=15/${lat}/${lon}`;
}
/** Structured address parts (§9.2 `address` object) in display order. */
function addressParts(l: EventLocation): string[] {
  const a = l.address;
  if (!a || typeof a !== "object") return [];
  return [a.street, a.locality, a.region, a.postal_code, a.country].filter(
    (s): s is string => typeof s === "string" && s.length > 0,
  );
}
/** The one-line address: structured parts joined, else the freeform
 * `address.text` (remote `address_text`), else a bare-string address. */
function locationText(l: EventLocation): string {
  const parts = addressParts(l);
  if (parts.length) return parts.join(", ");
  const a = l.address;
  if (typeof a === "string") return a;
  return a?.text ?? "";
}

// Map links follow `[events] map_provider` (`osm` default, `google`
// configurable) — coordinates win, the address text is the fallback
// query for places without lat/lon.
function googleMapsUrl(l: EventLocation): string | null {
  const q =
    l.latitude != null && l.longitude != null
      ? `${l.latitude},${l.longitude}`
      : locationText(l);
  if (!q) return null;
  return `https://www.google.com/maps/search/?api=1&query=${encodeURIComponent(q)}`;
}
function placeMapUrl(l: EventLocation): string | null {
  if (instance.eventsMapProvider === "google") return googleMapsUrl(l);
  if (l.latitude != null && l.longitude != null)
    return osmUrl(l.latitude, l.longitude);
  const text = locationText(l);
  return text
    ? `https://www.openstreetmap.org/search?query=${encodeURIComponent(text)}`
    : null;
}

// Embedded OSM preview — iframe embed needs no API key.
const mapLocation = ref<EventLocation | null>(null);
function osmEmbedUrl(lat: number, lon: number): string {
  const d = 0.01;
  const bbox = `${lon - d},${lat - d},${lon + d},${lat + d}`;
  return `https://www.openstreetmap.org/export/embed.html?bbox=${bbox}&layer=mapnik&marker=${lat},${lon}`;
}

// --- RSVP ------------------------------------------------------------
const rsvpBusy = ref(false);
async function rsvp(state: "accepted" | "tentative" | "declined" | "none") {
  if (rsvpBusy.value) return;
  rsvpBusy.value = true;
  try {
    const res = (await eventRsvp(String(props.status.id), state)) as {
      participation?: { state: string; role: string } | null;
    };
    participation.value = res.participation
      ? {
          state: res.participation.state,
          role: res.participation.role,
          can_join: false,
          reason: null,
        }
      : { state: undefined, can_join: true, reason: null };
  } catch {
    toasts.push({ text: t("common.error"), kind: "error" });
  } finally {
    rsvpBusy.value = false;
  }
}

/** Which RSVP buttons to show — the §14 state × protocol matrix. */
const rsvpActions = computed(() => {
  const e = ev.value;
  const p = synced.value;
  if (!e || !auth.isLoggedIn || !p) return [] as string[];
  if (isOrganizer.value) return [];
  const protocol = rsvpProtocol.value;
  if (protocol === "external" || protocol === "none") return [];
  const state = p.state;
  const canJoin = p.can_join !== false;
  const inviteResponse = protocol === "invite_response";
  const actions: string[] = [];
  if (
    !state ||
    state === "declined" ||
    state === "rejected" ||
    state === "ignored"
  ) {
    if (canJoin || !p.reason) {
      actions.push(inviteResponse ? "request" : "join");
    }
  }
  if (state === "invited") {
    // invite_response: Accept / Maybe / Decline.
    actions.push("accept", ...(inviteResponse ? ["tentative"] : []), "decline");
  } else if (state === "accepted" || state === "tentative") {
    actions.push("decline");
    if (state === "accepted") actions.unshift("accept_done");
    else actions.unshift("tentative_done");
  } else if (state === "pending") {
    actions.push("pending_done", "decline");
  }
  return actions;
});
const rsvpDisabledReason = computed(() => {
  const e = ev.value;
  const p = synced.value;
  if (!e || !auth.isLoggedIn || isOrganizer.value) return null;
  if (rsvpProtocol.value === "external") return null; // Register link instead
  if (rsvpProtocol.value === "none") return "none";
  if (!p?.can_join && !p?.state) return p?.reason ?? "unavailable";
  return null;
});

function onRsvp(action: string) {
  switch (action) {
    case "join":
    case "request":
      void rsvp("accepted");
      break;
    case "accept":
      void rsvp("accepted");
      break;
    case "tentative":
      void rsvp("tentative");
      break;
    case "decline":
      void rsvp("declined");
      break;
    case "cancel":
      void rsvp("none");
      break;
  }
}

// --- ICS actions -----------------------------------------------------
/** `ics_url` is absolute; `requestBlob` wants an API-relative path. */
function icsPath(): string | null {
  const url = ev.value?.ics_url;
  if (!url) return null;
  try {
    const u = new URL(url, location.origin);
    return `${u.pathname}${u.search}`;
  } catch {
    return url.startsWith("/") ? url : `/${url}`;
  }
}
/** Absolute ICS URL — `ics_url` may arrive relative on older servers. */
function icsAbsolute(): string | null {
  const url = ev.value?.ics_url;
  if (!url) return null;
  return url.startsWith("http") ? url : `${location.origin}${url}`;
}
/** Public/unlisted ICS serves anonymously — render a real link so
 * the browser (or "copy link address") handles it; narrower
 * visibilities keep the authenticated blob download. */
const icsIsPublic = computed(() =>
  ["public", "unlisted"].includes(props.status.visibility),
);
/** "Copy link" copies the event permalink, not the feed URL. */
const statusUrl = computed(
  () => props.status.url ?? `${location.origin}/statuses/${props.status.id}`,
);
const copied = ref(false);
async function copyLink() {
  try {
    await navigator.clipboard.writeText(statusUrl.value);
    copied.value = true;
    setTimeout(() => (copied.value = false), 2000);
  } catch {
    toasts.push({ text: t("common.error"), kind: "error" });
  }
}
async function downloadIcs() {
  const path = icsPath();
  if (!path) return;
  try {
    const { blob, filename } = await requestBlob({ path });
    saveBlob(blob, filename || `${props.status.id}.ics`);
  } catch {
    toasts.push({ text: t("common.error"), kind: "error" });
  }
}

// --- organizer panel --------------------------------------------------
type ParticipationRow = EventParticipationRow;
const panelOpen = ref(false);
const rows = ref<ParticipationRow[]>([]);
const panelLoading = ref(false);
const panelError = ref(false);

async function togglePanel() {
  panelOpen.value = !panelOpen.value;
  if (panelOpen.value && !rows.value.length) await loadRows();
}
async function loadRows() {
  panelLoading.value = true;
  panelError.value = false;
  try {
    const res = await eventParticipations(String(props.status.id));
    rows.value = res.participations;
  } catch {
    panelError.value = true;
  } finally {
    panelLoading.value = false;
  }
}
async function decide(accountId: string, decision: "authorize" | "reject") {
  try {
    await eventDecideRequest(String(props.status.id), accountId, decision);
    await loadRows();
  } catch {
    toasts.push({ text: t("common.error"), kind: "error" });
  }
}
async function removeParticipant(accountId: string) {
  try {
    await eventRemoveParticipant(String(props.status.id), accountId);
    await loadRows();
  } catch {
    toasts.push({ text: t("common.error"), kind: "error" });
  }
}
const pendingRows = computed(() =>
  rows.value.filter((r) => r.state === "pending"),
);
const otherRows = computed(() =>
  rows.value.filter((r) => r.state !== "pending"),
);

// invite-more
const inviteQuery = ref("");
const inviteResults = ref<Account[]>([]);
const inviteBusy = ref(false);
let inviteTimer: ReturnType<typeof setTimeout> | undefined;
async function searchInvitees() {
  const q = inviteQuery.value.trim();
  if (q.length < 2) {
    inviteResults.value = [];
    return;
  }
  try {
    const res = await search({ q, type: "accounts", limit: 5, resolve: true });
    inviteResults.value = res.accounts;
  } catch {
    inviteResults.value = [];
  }
}
function onInviteInput() {
  clearTimeout(inviteTimer);
  inviteTimer = setTimeout(searchInvitees, 300);
}
/** Enter picks the top suggestion, or runs the search immediately
 * when the debounced list hasn't populated yet. */
function onInviteEnter() {
  if (inviteResults.value.length) void invite(inviteResults.value[0]);
  else void searchInvitees();
}
async function invite(a: Account) {
  inviteBusy.value = true;
  try {
    await eventInvitees(String(props.status.id), [a.id]);
    inviteQuery.value = "";
    inviteResults.value = [];
    await loadRows();
  } catch {
    toasts.push({ text: t("common.error"), kind: "error" });
  } finally {
    inviteBusy.value = false;
  }
}

const capacityLabel = computed(() => {
  const e = ev.value;
  if (!e) return null;
  const parts: string[] = [];
  if (e.participants_count != null) parts.push(String(e.participants_count));
  if (e.maximum_attendee_capacity != null)
    parts.push(`/ ${e.maximum_attendee_capacity}`);
  return parts.length ? parts.join("") : null;
});
const cancelled = computed(() => ev.value?.status === "cancelled");
const postponed = computed(() => ev.value?.status === "postponed");
const statusBadge = computed(() => {
  const s = ev.value?.status;
  return s && s !== "scheduled" ? s : null;
});
</script>

<template>
  <div v-if="ev" class="status-event" data-testid="status-event">
    <img
      v-if="ev.banner"
      :src="ev.banner.preview_url ?? ev.banner.url"
      :alt="ev.banner.description ?? ''"
      class="status-event__banner"
      loading="lazy"
    />

    <div class="status-event__head">
      <div class="status-event__when">
        <span class="status-event__when-line">
          <FontAwesomeIcon icon="calendar-days" />
          <span class="status-event__when-label">{{ t("events.start") }}</span>
          {{ startLabel }}
        </span>
        <span v-if="endLabel" class="status-event__when-line">
          <FontAwesomeIcon icon="calendar-days" />
          <span class="status-event__when-label">{{ t("events.end") }}</span>
          {{ endLabel }}
        </span>
      </div>
      <span v-if="viewerTimeLabel" class="status-event__viewer-tz">
        {{ t("events.viewerTime", { time: viewerTimeLabel }) }}
      </span>
      <span
        v-if="statusBadge"
        class="status-event__badge"
        :class="{ 'status-event__badge--cancelled': cancelled }"
      >
        {{ t(`events.status_${statusBadge}`) }}
      </span>
    </div>

    <h3
      class="status-event__name"
      :class="{ 'status-event__name--cancelled': cancelled }"
    >
      {{ ev.name }}
    </h3>
    <p v-if="postponed && ev.previous_start_time" class="status-event__was">
      {{
        t("events.was", { time: fmt.format(new Date(ev.previous_start_time)) })
      }}
    </p>

    <ul v-if="ev.locations?.length" class="status-event__locs">
      <li v-for="(l, i) in ev.locations" :key="i">
        <template v-if="l.type === 'virtual'">
          <FontAwesomeIcon icon="video" />
          <a
            v-if="l.url"
            :href="l.url"
            target="_blank"
            rel="noopener noreferrer"
          >
            {{ l.name || l.url }}
          </a>
          <span v-else>{{ l.name }}</span>
        </template>
        <template v-else>
          <div class="status-event__place--name">
            <FontAwesomeIcon icon="location-dot" />
            <a
              v-if="placeMapUrl(l)"
              :href="placeMapUrl(l)!"
              target="_blank"
              rel="noopener noreferrer"
            >
              {{ l.name || locationText(l) || t("events.place") }}
            </a>
            <span v-else>{{
              l.name || locationText(l) || t("events.place")
            }}</span>
          </div>
          <div class="status-event__addr" v-if="l.latitude != null && l.longitude != null">
            <button
              type="button"
              class="status-event__map-btn"
              :aria-label="t('events.showMap')"
              :title="t('events.showMap')"
              data-testid="event-show-map"
              @click="mapLocation = l"
            >
              <FontAwesomeIcon icon="map-location-dot" />
            </button>
            <span v-if="l.name && locationText(l)">
              {{ locationText(l) }}
            </span>
          </div>
        </template>
      </li>
    </ul>

    <p
      v-if="ev.external_participation_url && rsvpProtocol === 'external'"
      class="status-event__ext"
    >
      <a
        :href="ev.external_participation_url"
        target="_blank"
        rel="noopener noreferrer"
      >
        <FontAwesomeIcon icon="arrow-up-right-from-square" />
        {{ t("events.register") }}
      </a>
    </p>

    <div
      v-if="ev.categories?.length || capacityLabel"
      class="status-event__meta"
    >
      <span v-for="c in ev.categories" :key="c" class="status-event__cat">
        {{
          c
            .toLowerCase()
            .split("_")
            .map((w) => w[0].toUpperCase() + w.slice(1))
            .join(" ")
        }}
      </span>
      <span v-if="capacityLabel" class="status-event__count">
        <FontAwesomeIcon icon="user-group" /> {{ capacityLabel }}
      </span>
    </div>

    <p v-if="ev.links?.length" class="status-event__links">
      <a
        v-for="(l, i) in ev.links"
        :key="i"
        :href="l.href"
        target="_blank"
        rel="noopener noreferrer"
      >
        {{ l.name || l.href }}
      </a>
    </p>

    <!-- RSVP controls -->
    <div
      v-if="rsvpActions.length || rsvpDisabledReason"
      class="status-event__rsvp"
    >
      <template v-if="rsvpActions.length">
        <Button
          v-for="a in rsvpActions"
          :key="a"
          size="sm"
          :variant="a.endsWith('_done') ? 'secondary' : 'primary'"
          :loading="
            rsvpBusy && (a === 'accept' || a === 'join' || a === 'request')
          "
          :disabled="rsvpBusy"
          :data-testid="`rsvp-${a}`"
          @click="
            onRsvp(
              a === 'accept_done' ||
                a === 'tentative_done' ||
                a === 'pending_done'
                ? 'cancel'
                : a,
            )
          "
        >
          <template v-if="a === 'accept_done'">
            <FontAwesomeIcon icon="check" /> {{ t("events.going") }}
          </template>
          <template v-else-if="a === 'tentative_done'">
            <FontAwesomeIcon icon="check" /> {{ t("events.maybe") }}
          </template>
          <template v-else-if="a === 'pending_done'">
            {{ t("events.pending") }}
          </template>
          <template v-else-if="a === 'join'">{{ t("events.join") }}</template>
          <template v-else-if="a === 'request'">{{
            t("events.requestJoin")
          }}</template>
          <template v-else-if="a === 'accept'">{{
            t("events.accept")
          }}</template>
          <template v-else-if="a === 'tentative'">{{
            t("events.maybe")
          }}</template>
          <template v-else-if="a === 'decline'">{{
            t("events.decline")
          }}</template>
        </Button>
      </template>
      <span v-else-if="rsvpDisabledReason" class="status-event__rsvp-reason">
        {{ t(`events.reason_${rsvpDisabledReason}`) }}
      </span>
    </div>

    <!-- ICS actions -->
    <div class="status-event__ics">
      <button type="button" class="status-event__ics-btn" @click="copyLink">
        <FontAwesomeIcon :icon="copied ? 'check' : 'link'" />
        {{ copied ? t("events.copied") : t("events.copyLink") }}
      </button>
      <a
        v-if="icsIsPublic && icsAbsolute()"
        :href="icsAbsolute()!"
        class="status-event__ics-btn"
        data-testid="download-calendar-link"
      >
        <FontAwesomeIcon icon="download" />
        {{ t("events.downloadCalendar") }}
      </a>
      <button
        v-else
        type="button"
        class="status-event__ics-btn"
        @click="downloadIcs"
      >
        <FontAwesomeIcon icon="download" />
        {{ t("events.downloadCalendar") }}
      </button>
    </div>

    <!-- Organizer panel -->
    <div v-if="isOrganizer" class="status-event__organizer">
      <button
        type="button"
        class="status-event__organizer-toggle"
        :aria-expanded="panelOpen"
        data-testid="organizer-panel-toggle"
        @click="togglePanel"
      >
        <FontAwesomeIcon icon="user-gear" />
        {{ t("events.organizerPanel") }}
        <span v-if="pendingRows.length" class="status-event__pending-count">
          {{ pendingRows.length }}
        </span>
      </button>
      <div v-if="panelOpen" class="status-event__panel">
        <p v-if="panelLoading" class="status-event__hint">
          {{ t("common.loading") }}
        </p>
        <p v-else-if="panelError" class="status-event__hint">
          {{ t("common.error") }}
        </p>
        <template v-else>
          <div v-if="pendingRows.length" class="status-event__requests">
            <h4>{{ t("events.pendingRequests") }}</h4>
            <div
              v-for="r in pendingRows"
              :key="r.account_id"
              class="status-event__row"
            >
              <AccountChip
                :account="r.account"
                :fallback="`@${r.account_id}`"
              />
              <span v-if="r.participation_message" class="status-event__msg">
                {{ r.participation_message }}
              </span>
              <span class="status-event__row-actions">
                <Button size="sm" @click="decide(r.account_id, 'authorize')">
                  {{ t("events.authorize") }}
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  @click="decide(r.account_id, 'reject')"
                >
                  {{ t("events.reject") }}
                </Button>
              </span>
            </div>
          </div>
          <div v-if="otherRows.length" class="status-event__participants">
            <h4>{{ t("events.participants") }}</h4>
            <div
              v-for="r in otherRows"
              :key="r.account_id"
              class="status-event__row"
            >
              <AccountChip
                :account="r.account"
                :fallback="`@${r.account_id}`"
              />
              <span class="status-event__state">{{
                t(`events.state_${r.state}`)
              }}</span>
              <Button
                v-if="r.state !== 'accepted'"
                size="sm"
                variant="secondary"
                @click="removeParticipant(r.account_id)"
              >
                {{ t("events.remove") }}
              </Button>
            </div>
          </div>
          <!-- Invitees are a direct-visibility mechanism — the
               endpoint 422s anything wider. -->
          <div
            v-if="props.status.visibility === 'direct'"
            class="status-event__invite"
          >
            <input
              v-model="inviteQuery"
              class="status-event__invite-input"
              :placeholder="t('events.inviteMore')"
              data-testid="invite-search"
              @input="onInviteInput"
              @keydown.enter.prevent="onInviteEnter"
            />
            <ul
              v-if="inviteResults.length"
              class="status-event__invite-results"
            >
              <li v-for="a in inviteResults" :key="a.id">
                <button type="button" :disabled="inviteBusy" @click="invite(a)">
                  <Avatar
                    :src="a.avatar"
                    :name="a.display_name || a.username"
                    size="sm"
                  />
                  <span>{{ a.display_name || a.username }}</span>
                  <span class="status-event__acct">@{{ a.acct }}</span>
                </button>
              </li>
            </ul>
          </div>
        </template>
      </div>
    </div>

    <Modal
      v-if="mapLocation"
      :title="
        mapLocation.name || locationText(mapLocation) || t('events.place')
      "
      @close="mapLocation = null"
    >
      <iframe
        v-if="mapLocation.latitude != null && mapLocation.longitude != null"
        :src="osmEmbedUrl(mapLocation.latitude, mapLocation.longitude)"
        class="status-event__map"
        title="OpenStreetMap"
        loading="lazy"
        referrerpolicy="no-referrer-when-downgrade"
      />
      <a
        v-if="placeMapUrl(mapLocation)"
        :href="placeMapUrl(mapLocation)!"
        target="_blank"
        rel="noopener noreferrer"
        class="status-event__map-link"
      >
        <FontAwesomeIcon icon="arrow-up-right-from-square" />
        {{ t("events.openInMaps") }}
      </a>
    </Modal>
  </div>
</template>

<style scoped>
.status-event {
  border: 1px solid var(--border, #ddd);
  border-radius: var(--radius-md, 8px);
  overflow: hidden;
  margin: 0.5rem 0;
}
.status-event__banner {
  width: 100%;
  max-height: 12rem;
  object-fit: cover;
  display: block;
}
.status-event__head {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  flex-wrap: wrap;
  padding: 0.5rem 0.75rem 0;
  font-size: 0.85rem;
  color: var(--text-muted, #666);
}
.status-event__when {
  display: flex;
  flex-direction: column;
  gap: 0.15rem;
}
.status-event__when-line {
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
  font-weight: 600;
  color: var(--text, #222);
}
.status-event__when-label {
  font-weight: 400;
  color: var(--text-muted, #666);
  min-width: 2.5rem;
}
.status-event__viewer-tz {
  color: var(--text-muted, #666);
}
.status-event__badge {
  border-radius: 999px;
  padding: 0.1rem 0.5rem;
  font-size: 0.75rem;
  background: var(--bg-hover, #eee);
  color: var(--text, #222);
}
.status-event__badge--cancelled {
  background: #b3261e;
  color: #fff;
}
.status-event__name {
  margin: 0.75rem 0;
  padding: 0 0.75rem;
  font-size: 1.5rem;
}
.status-event__name--cancelled {
  text-decoration: line-through;
  color: var(--text-muted, #999);
}
.status-event__was {
  margin: 0;
  padding: 0 0.75rem;
  font-size: 0.8rem;
  color: var(--text-muted, #666);
}
.status-event__locs {
  list-style: none;
  margin: 0.4rem 0 0;
  padding: 0 0.75rem;
  display: flex;
  flex-direction: column;
  gap: 0.2rem;
  font-size: 0.9rem;
}
.status-event__locs a {
  text-decoration: none;
}
.status-event__map-btn {
  border: 0;
  background: none;
  color: var(--text-muted, #666);
  cursor: pointer;
  padding: 0 0.2rem;
  font-size: 0.85rem;
}
.status-event__map-btn:hover {
  color: var(--accent, #4a9eff);
}
.status-event__map {
  width: 100%;
  height: 20rem;
  border: 0;
  border-radius: var(--radius-sm, 6px);
}
.status-event__map-link {
  display: inline-flex;
  align-items: center;
  gap: 0.35rem;
  margin-top: var(--space-2, 0.5rem);
  color: var(--accent, #4a9eff);
  font-size: 0.85rem;
}
.status-event__addr {
  color: var(--text-faint, #777);
  font-size: 0.8rem;
  font-weight: 300;
}
.status-event__ext {
  margin: 0.4rem 0 0;
  padding: 0 0.75rem;
}
.status-event__meta {
  display: flex;
  gap: 0.4rem;
  flex-wrap: wrap;
  padding: 0.4rem 0.75rem 0;
  align-items: center;
}
.status-event__cat {
  border: 1px solid var(--border, #ddd);
  border-radius: 999px;
  padding: 0.05rem 0.5rem;
  font-size: 0.75rem;
  color: var(--text-muted, #666);
}
.status-event__count {
  font-size: 0.8rem;
  color: var(--text-muted, #666);
}
.status-event__links {
  margin: 0.3rem 0 0;
  padding: 0 0.75rem;
  display: flex;
  gap: 0.75rem;
  flex-wrap: wrap;
}
.status-event__links a {
  color: var(--accent, #4a9eff);
  font-size: 0.85rem;
}
.status-event__rsvp {
  display: flex;
  gap: 0.4rem;
  flex-wrap: wrap;
  padding: 0.6rem 0.75rem 0;
}
.status-event__rsvp-reason {
  font-size: 0.8rem;
  color: var(--text-muted, #666);
  align-self: center;
}
.status-event__ics {
  display: flex;
  gap: 0.4rem;
  padding: 0.5rem 0.75rem;
}
.status-event__ics-btn {
  border: 1px solid var(--border, #ddd);
  background: none;
  border-radius: var(--radius-sm, 6px);
  padding: 0.3rem 0.6rem;
  font-size: 0.8rem;
  cursor: pointer;
  color: var(--text, #222);
  display: inline-flex;
  align-items: center;
  gap: 0.35rem;
  text-decoration: none;
}
.status-event__ics-btn:hover {
  background: var(--bg-hover, #f0f0f0);
}
.status-event__organizer {
  border-top: 1px solid var(--border, #ddd);
  padding: 0.5rem 0.75rem;
}
.status-event__organizer-toggle {
  border: 0;
  background: none;
  font: inherit;
  font-weight: 600;
  cursor: pointer;
  color: var(--text, #222);
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
}
.status-event__pending-count {
  background: var(--accent, #4a9eff);
  color: #fff;
  border-radius: 999px;
  font-size: 0.7rem;
  padding: 0.05rem 0.4rem;
}
.status-event__panel {
  margin-top: 0.5rem;
  display: flex;
  flex-direction: column;
  gap: 0.6rem;
}
.status-event__panel h4 {
  margin: 0 0 0.3rem;
  font-size: 0.85rem;
}
.status-event__row {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  flex-wrap: wrap;
  position: relative;
  padding: 0.25rem 0;
  font-size: 0.85rem;
}
.status-event__state,
.status-event__msg {
  color: var(--text-faint, #666);
  font-size: 0.8rem;
}
.status-event__state {
  position: absolute;
  top: 0;
  right: 0;
}
.status-event__row-actions {
  margin-left: auto;
  display: flex;
  gap: 0.3rem;
}
.status-event__hint {
  color: var(--text-muted, #666);
  font-size: 0.85rem;
}
.status-event__invite-input {
  width: 100%;
  padding: 0.4rem 0.6rem;
  border: 1px solid var(--border, #ddd);
  border-radius: var(--radius-sm, 6px);
  font: inherit;
  background: var(--bg, #fff);
  color: var(--text, #222);
}
.status-event__invite-results {
  list-style: none;
  margin: 0.3rem 0 0;
  padding: 0;
  border: 1px solid var(--border, #ddd);
  border-radius: var(--radius-sm, 6px);
}
.status-event__invite-results button {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  width: 100%;
  border: 0;
  background: none;
  padding: 0.4rem 0.6rem;
  cursor: pointer;
  font: inherit;
  color: var(--text, #222);
  text-align: start;
}
.status-event__acct {
  color: var(--text-muted, #666);
  font-size: 0.8rem;
}
</style>
