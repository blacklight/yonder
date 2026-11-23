<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { FontAwesomeIcon } from "@/icons";
import { search } from "@/api/search";
import type { Account, MediaAttachment } from "@/api/entities";
import type {
  EventDraft,
  EventLocationDraft,
} from "@/stores/compose";
import { timeZones } from "@/utils/timezone";

const props = defineProps<{
  event: EventDraft;
  /** The event name — shared with the draft's `title` field. */
  title: string;
  /** Draft visibility — the invitee picker only applies to `direct`. */
  visibility: string;
  /** Uploaded attachments eligible for banner/flyer picks. */
  media: MediaAttachment[];
  /** Edit mode — exposes the status transitions (cancel/postpone/…). */
  editing?: boolean;
}>();
const emit = defineEmits<{
  update: [event: EventDraft];
  updateTitle: [title: string];
  remove: [];
}>();
const { t } = useI18n();

const JOIN_MODES = ["free", "restricted", "invite", "external"];
const ATTENDEE_VISIBILITIES = ["organizers", "participants", "public"];
// Mirrors the backend FEP-8a8e category list (`events::validate::
// FEP_CATEGORIES`) — labels are humanized codes, not translated.
const CATEGORIES = [
  "ARTS",
  "AUTO_BOAT_AIR",
  "BOOK_CLUBS",
  "BUSINESS",
  "CAUSES",
  "CLIMATE_ENVIRONMENT",
  "COMMUNITY",
  "COMEDY",
  "CRAFTS",
  "CREATIVE_JAM",
  "DIY_MAKER_SPACES",
  "FAMILY_EDUCATION",
  "FASHION_BEAUTY",
  "FESTIVALS",
  "FILM_MEDIA",
  "FOOD_DRINK",
  "GAMES",
  "INCLUSIVE_SPACES",
  "LANGUAGE_CULTURE",
  "LEARNING",
  "LGBTQ",
  "MEETING",
  "MEDITATION_WELLBEING",
  "MOVEMENTS_POLITICS",
  "MUSIC",
  "NETWORKING",
  "OUTDOORS_ADVENTURE",
  "PARTY",
  "PERFORMING_VISUAL_ARTS",
  "PETS",
  "PHOTOGRAPHY",
  "SCIENCE_TECH",
  "SPIRITUALITY_RELIGION_BELIEFS",
  "SPORTS",
  "THEATRE",
  "WORKSHOPS_SKILL_SHARING",
];
// Edit-mode status transitions the API accepts (§9.1).
const EDIT_STATUSES = [
  "scheduled",
  "tentative",
  "rescheduled",
  "moved_online",
  "postponed",
  "cancelled",
];

const zones = timeZones();
const images = computed(() =>
  props.media.filter((m) => m.type === "image" || m.type === "gifv"),
);

function patch(p: Partial<EventDraft>) {
  emit("update", { ...props.event, ...p });
}
function patchLocation(i: number, p: Partial<EventLocationDraft>) {
  const locations = props.event.locations.map((l, j) =>
    j === i ? { ...l, ...p } : l,
  );
  patch({ locations });
}
function categoryLabel(code: string): string {
  return code
    .toLowerCase()
    .split("_")
    .map((w) => w[0].toUpperCase() + w.slice(1))
    .join(" ");
}
function toggleCategory(code: string) {
  const categories = props.event.categories.includes(code)
    ? props.event.categories.filter((c) => c !== code)
    : [...props.event.categories, code];
  patch({ categories });
}

const hasEnd = computed(() => props.event.endTime !== "");

/** Edit-mode start-time change: moving the start of a live event is
 * a reschedule — confirm once, then mark the status explicitly. */
function onStartInput(e: Event) {
  const input = e.target as HTMLInputElement;
  const v = input.value;
  const movable = ["scheduled", "tentative"].includes(props.event.status);
  if (
    props.editing &&
    movable &&
    props.event.startTime &&
    v &&
    v !== props.event.startTime
  ) {
    if (window.confirm(t("compose.eventRescheduleConfirm"))) {
      patch({ startTime: v, status: "rescheduled" });
    } else {
      input.value = props.event.startTime;
    }
    return;
  }
  patch({ startTime: v });
}
function toggleEnd(checked: boolean) {
  if (checked) {
    // Default to one hour after start (or two hours out).
    const base = props.event.startTime
      ? new Date(`${props.event.startTime}:00`)
      : new Date(Date.now() + 2 * 3600_000);
    const end = new Date(
      (Number.isNaN(base.getTime()) ? Date.now() + 2 * 3600_000 : base.getTime()) +
        3600_000,
    );
    const p = (n: number) => String(n).padStart(2, "0");
    patch({
      endTime: `${end.getFullYear()}-${p(end.getMonth() + 1)}-${p(end.getDate())}T${p(end.getHours())}:${p(end.getMinutes())}`,
    });
  } else {
    patch({ endTime: "" });
  }
}

function addLocation(kind: "place" | "virtual") {
  patch({
    locations: [
      ...props.event.locations,
      {
        kind,
        name: "",
        url: "",
        streetAddress: "",
        locality: "",
        region: "",
        postalCode: "",
        country: "",
        addressText: "",
        latitude: "",
        longitude: "",
      },
    ],
  });
}
function removeLocation(i: number) {
  patch({ locations: props.event.locations.filter((_, j) => j !== i) });
}

// --- invitee picker (direct visibility only) -------------------------
const inviteQuery = ref("");
const inviteResults = ref<Account[]>([]);
const invitees = ref<Account[]>([]);
const inviteLoading = ref(false);
let inviteTimer: ReturnType<typeof setTimeout> | undefined;

async function searchInvitees() {
  const q = inviteQuery.value.trim();
  if (q.length < 2) {
    inviteResults.value = [];
    return;
  }
  inviteLoading.value = true;
  try {
    const res = await search({ q, type: "accounts", limit: 5 });
    const chosen = new Set(props.event.invitees);
    inviteResults.value = res.accounts.filter((a) => !chosen.has(a.id));
  } catch {
    inviteResults.value = [];
  } finally {
    inviteLoading.value = false;
  }
}
function onInviteInput() {
  clearTimeout(inviteTimer);
  inviteTimer = setTimeout(searchInvitees, 300);
}
function addInvitee(a: Account) {
  if (props.event.invitees.includes(a.id)) return;
  patch({ invitees: [...props.event.invitees, a.id] });
  invitees.value = [...invitees.value, a];
  inviteQuery.value = "";
  inviteResults.value = [];
}
function removeInvitee(id: string) {
  patch({ invitees: props.event.invitees.filter((i) => i !== id) });
  invitees.value = invitees.value.filter((a) => a.id !== id);
}
</script>

<template>
  <fieldset class="event" data-testid="event-builder">
    <legend class="event__legend">
      {{ t("compose.event") }}
      <button type="button" class="event__remove" @click="emit('remove')">
        {{ t("compose.eventRemove") }}
      </button>
    </legend>

    <input
      :value="title"
      class="event__name"
      :placeholder="t('compose.eventName')"
      maxlength="500"
      :aria-label="t('compose.eventName')"
      data-testid="event-name"
      @input="emit('updateTitle', ($event.target as HTMLInputElement).value)"
    />

    <div class="event__row">
      <label class="event__field">
        <span>{{ t("compose.eventStart") }}</span>
        <input
          :value="event.startTime"
          type="datetime-local"
          data-testid="event-start"
          @input="onStartInput"
        />
      </label>
      <label class="event__field">
        <span>{{ t("compose.eventEnd") }}</span>
        <span class="event__end">
          <input
            type="checkbox"
            :checked="hasEnd"
            data-testid="event-has-end"
            @change="toggleEnd(($event.target as HTMLInputElement).checked)"
          />
          <input
            :value="event.endTime"
            type="datetime-local"
            :disabled="!hasEnd"
            data-testid="event-end"
            @input="patch({ endTime: ($event.target as HTMLInputElement).value })"
          />
        </span>
      </label>
      <label class="event__field">
        <span>{{ t("compose.eventTimezone") }}</span>
        <select
          :value="event.timezone"
          data-testid="event-timezone"
          @change="patch({ timezone: ($event.target as HTMLSelectElement).value })"
        >
          <option v-for="z in zones" :key="z" :value="z">{{ z }}</option>
        </select>
      </label>
    </div>

    <div v-if="editing" class="event__row">
      <label class="event__field">
        <span>{{ t("compose.eventStatus") }}</span>
        <select
          :value="event.status"
          data-testid="event-status"
          @change="patch({ status: ($event.target as HTMLSelectElement).value })"
        >
          <option v-for="s in EDIT_STATUSES" :key="s" :value="s">
            {{ t(`compose.eventStatus_${s}`) }}
          </option>
        </select>
      </label>
    </div>

    <div class="event__row">
      <label class="event__field">
        <span>{{ t("compose.eventJoinMode") }}</span>
        <select
          :value="event.joinMode"
          data-testid="event-join-mode"
          @change="patch({ joinMode: ($event.target as HTMLSelectElement).value })"
        >
          <option v-for="m in JOIN_MODES" :key="m" :value="m">
            {{ t(`compose.eventJoin_${m}`) }}
          </option>
        </select>
      </label>
      <label v-if="event.joinMode === 'external'" class="event__field event__field--grow">
        <span>{{ t("compose.eventExternalUrl") }}</span>
        <input
          :value="event.externalParticipationUrl"
          type="url"
          :placeholder="t('compose.eventExternalUrlPh')"
          data-testid="event-external-url"
          @input="
            patch({
              externalParticipationUrl: ($event.target as HTMLInputElement).value,
            })
          "
        />
      </label>
      <label class="event__field">
        <span>{{ t("compose.eventCapacity") }}</span>
        <input
          :value="event.maximumAttendeeCapacity"
          type="number"
          min="1"
          data-testid="event-capacity"
          @input="
            patch({
              maximumAttendeeCapacity: ($event.target as HTMLInputElement).value,
            })
          "
        />
      </label>
      <label class="event__field">
        <span>{{ t("compose.eventAttendeesVisibility") }}</span>
        <select
          :value="event.attendeesVisibility"
          data-testid="event-attendees-visibility"
          @change="
            patch({ attendeesVisibility: ($event.target as HTMLSelectElement).value })
          "
        >
          <option v-for="v in ATTENDEE_VISIBILITIES" :key="v" :value="v">
            {{ t(`compose.eventAttendees_${v}`) }}
          </option>
        </select>
      </label>
    </div>

    <div class="event__cats" data-testid="event-categories">
      <button
        v-for="c in CATEGORIES"
        :key="c"
        type="button"
        class="event__cat"
        :class="{ 'event__cat--on': event.categories.includes(c) }"
        :aria-pressed="event.categories.includes(c)"
        @click="toggleCategory(c)"
      >
        {{ categoryLabel(c) }}
      </button>
    </div>

    <div class="event__locs">
      <div
        v-for="(loc, i) in event.locations"
        :key="i"
        class="event__loc"
        data-testid="event-location"
      >
        <div class="event__loc-head">
          <select
            :value="loc.kind"
            @change="
              patchLocation(i, {
                kind: ($event.target as HTMLSelectElement).value as 'place' | 'virtual',
              })
            "
          >
            <option value="place">{{ t("compose.eventLocPlace") }}</option>
            <option value="virtual">{{ t("compose.eventLocVirtual") }}</option>
          </select>
          <button
            type="button"
            class="event__loc-x"
            :aria-label="t('compose.eventLocRemove')"
            @click="removeLocation(i)"
          >
            <FontAwesomeIcon icon="xmark" />
          </button>
        </div>
        <input
          :value="loc.name"
          :placeholder="t('compose.eventLocName')"
          @input="patchLocation(i, { name: ($event.target as HTMLInputElement).value })"
        />
        <input
          v-if="loc.kind === 'virtual'"
          :value="loc.url"
          type="url"
          :placeholder="t('compose.eventLocUrl')"
          @input="patchLocation(i, { url: ($event.target as HTMLInputElement).value })"
        />
        <template v-else>
          <input
            :value="loc.streetAddress"
            :placeholder="t('compose.eventLocStreet')"
            @input="
              patchLocation(i, {
                streetAddress: ($event.target as HTMLInputElement).value,
              })
            "
          />
          <div class="event__loc-grid">
            <input
              :value="loc.locality"
              :placeholder="t('compose.eventLocLocality')"
              @input="
                patchLocation(i, {
                  locality: ($event.target as HTMLInputElement).value,
                })
              "
            />
            <input
              :value="loc.region"
              :placeholder="t('compose.eventLocRegion')"
              @input="
                patchLocation(i, { region: ($event.target as HTMLInputElement).value })
              "
            />
            <input
              :value="loc.postalCode"
              :placeholder="t('compose.eventLocPostal')"
              @input="
                patchLocation(i, {
                  postalCode: ($event.target as HTMLInputElement).value,
                })
              "
            />
            <input
              :value="loc.country"
              :placeholder="t('compose.eventLocCountry')"
              @input="
                patchLocation(i, { country: ($event.target as HTMLInputElement).value })
              "
            />
          </div>
          <div class="event__loc-grid">
            <input
              :value="loc.latitude"
              type="number"
              step="any"
              :placeholder="t('compose.eventLocLat')"
              @input="
                patchLocation(i, {
                  latitude: ($event.target as HTMLInputElement).value,
                })
              "
            />
            <input
              :value="loc.longitude"
              type="number"
              step="any"
              :placeholder="t('compose.eventLocLon')"
              @input="
                patchLocation(i, {
                  longitude: ($event.target as HTMLInputElement).value,
                })
              "
            />
          </div>
        </template>
      </div>
      <div class="event__loc-adds">
        <button type="button" class="event__add" @click="addLocation('place')">
          <FontAwesomeIcon icon="plus" /> {{ t("compose.eventLocAddPlace") }}
        </button>
        <button type="button" class="event__add" @click="addLocation('virtual')">
          <FontAwesomeIcon icon="plus" /> {{ t("compose.eventLocAddVirtual") }}
        </button>
      </div>
    </div>

    <div v-if="images.length" class="event__media">
      <span class="event__media-label">{{ t("compose.eventBanner") }}</span>
      <div class="event__media-row">
        <button
          v-for="m in images"
          :key="m.id"
          type="button"
          class="event__media-pick"
          :class="{
            'event__media-pick--banner': event.bannerMediaId === m.id,
            'event__media-pick--flyer': event.flyerMediaId === m.id,
          }"
          @click="
            patch({
              bannerMediaId: event.bannerMediaId === m.id ? null : m.id,
            })
          "
        >
          <img :src="m.preview_url ?? m.url" :alt="m.description ?? ''" />
          <span v-if="event.bannerMediaId === m.id" class="event__media-tag">
            {{ t("compose.eventBanner") }}
          </span>
        </button>
      </div>
      <span class="event__media-label">{{ t("compose.eventFlyer") }}</span>
      <div class="event__media-row">
        <button
          v-for="m in images"
          :key="m.id"
          type="button"
          class="event__media-pick"
          :class="{ 'event__media-pick--flyer': event.flyerMediaId === m.id }"
          @click="
            patch({ flyerMediaId: event.flyerMediaId === m.id ? null : m.id })
          "
        >
          <img :src="m.preview_url ?? m.url" :alt="m.description ?? ''" />
          <span v-if="event.flyerMediaId === m.id" class="event__media-tag">
            {{ t("compose.eventFlyer") }}
          </span>
        </button>
      </div>
    </div>

    <div v-if="visibility === 'direct'" class="event__invitees">
      <span class="event__media-label">{{ t("compose.eventInvitees") }}</span>
      <div v-if="invitees.length" class="event__invitee-chips">
        <span v-for="a in invitees" :key="a.id" class="event__invitee">
          @{{ a.acct }}
          <button
            type="button"
            :aria-label="t('compose.eventInviteeRemove')"
            @click="removeInvitee(a.id)"
          >
            <FontAwesomeIcon icon="xmark" />
          </button>
        </span>
      </div>
      <input
        v-model="inviteQuery"
        class="event__invitee-input"
        :placeholder="t('compose.eventInviteePh')"
        data-testid="event-invitee-search"
        @input="onInviteInput"
      />
      <ul v-if="inviteResults.length" class="event__invitee-results">
        <li v-for="a in inviteResults" :key="a.id">
          <button type="button" @click="addInvitee(a)">
            <span class="event__invitee-name">{{ a.display_name || a.username }}</span>
            <span class="event__invitee-acct">@{{ a.acct }}</span>
          </button>
        </li>
      </ul>
      <p v-if="inviteLoading" class="event__hint">{{ t("ui.loading") }}</p>
    </div>
  </fieldset>
</template>

<style scoped>
.event {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-3);
}
.event__legend {
  padding: 0 var(--space-1);
  font-weight: 600;
  display: flex;
  justify-content: space-between;
  align-items: center;
  width: 100%;
}
.event__remove {
  border: 0;
  background: none;
  color: var(--text-muted);
  cursor: pointer;
  font: inherit;
  font-size: var(--text-sm);
}
.event__name {
  font-weight: 600;
  padding: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  font: inherit;
  background: var(--bg);
  color: var(--text);
}
.event__row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}
.event__field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  font-size: var(--text-sm);
  color: var(--text-muted);
  flex: 1;
  min-width: 9rem;
}
.event__field--grow {
  flex: 2;
}
.event__field input,
.event__field select {
  padding: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  font: inherit;
  background: var(--bg);
  color: var(--text);
}
.event__end {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.event__end input[type="datetime-local"] {
  flex: 1;
}
.event__cats {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-1);
}
.event__cat {
  border: 1px solid var(--border);
  background: none;
  border-radius: 999px;
  padding: var(--space-1) var(--space-2);
  font-size: var(--text-sm);
  color: var(--text);
  cursor: pointer;
}
.event__cat--on {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--accent-contrast, #fff);
}
.event__locs {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.event__loc {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  border: 1px dashed var(--border);
  border-radius: var(--radius-sm);
  padding: var(--space-2);
}
.event__loc-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
}
.event__loc-x {
  border: 0;
  background: none;
  color: var(--text-muted);
  cursor: pointer;
}
.event__loc input,
.event__loc select {
  padding: var(--space-1) var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  font: inherit;
  background: var(--bg);
  color: var(--text);
}
.event__loc-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: var(--space-1);
}
.event__loc-adds {
  display: flex;
  gap: var(--space-2);
}
.event__add {
  border: 1px dashed var(--border);
  background: none;
  border-radius: var(--radius-sm);
  padding: var(--space-1) var(--space-3);
  cursor: pointer;
  color: var(--text);
  font-size: var(--text-sm);
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
}
.event__media {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.event__media-label {
  font-size: var(--text-sm);
  color: var(--text-muted);
}
.event__media-row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}
.event__media-pick {
  position: relative;
  border: 2px solid transparent;
  border-radius: var(--radius-sm);
  padding: 0;
  background: none;
  cursor: pointer;
  overflow: hidden;
}
.event__media-pick img {
  width: 4.5rem;
  height: 4.5rem;
  object-fit: cover;
  display: block;
}
.event__media-pick--banner {
  border-color: var(--accent);
}
.event__media-pick--flyer {
  border-color: var(--info, #4a9eff);
}
.event__media-tag {
  position: absolute;
  inset-inline: 0;
  bottom: 0;
  font-size: 0.65rem;
  background: rgb(0 0 0 / 0.6);
  color: #fff;
  text-align: center;
}
.event__invitees {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.event__invitee-chips {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-1);
}
.event__invitee {
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
  border: 1px solid var(--border);
  border-radius: 999px;
  padding: var(--space-1) var(--space-2);
  font-size: var(--text-sm);
}
.event__invitee button {
  border: 0;
  background: none;
  color: var(--text-muted);
  cursor: pointer;
}
.event__invitee-input {
  padding: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  font: inherit;
  background: var(--bg);
  color: var(--text);
}
.event__invitee-results {
  list-style: none;
  margin: 0;
  padding: 0;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
}
.event__invitee-results button {
  display: flex;
  gap: var(--space-2);
  width: 100%;
  border: 0;
  background: none;
  padding: var(--space-2);
  cursor: pointer;
  text-align: start;
  font: inherit;
  color: var(--text);
}
.event__invitee-results button:hover {
  background: var(--bg-hover, rgb(0 0 0 / 0.04));
}
.event__invitee-acct {
  color: var(--text-muted);
  font-size: var(--text-sm);
}
.event__hint {
  color: var(--text-muted);
  font-size: var(--text-sm);
  margin: 0;
}
</style>
