import { requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";
import type { Account, Status } from "./entities";

export type EventsScope =
  | "mine"
  | "participating"
  | "home"
  | "local"
  | "federated"
  | `account:${string}`;

export interface EventsQuery {
  scope?: EventsScope;
  state?: string;
  category?: string;
  /** RFC 3339 lower bound — defaults to now server-side. */
  from?: string;
  /** RFC 3339 upper bound — defaults to +90d; unbounded when `from`
   * is set without `to`. */
  to?: string;
  /** Hashtag filter (case-insensitive). */
  tag?: string;
  /** true → only virtual-location (online) events. */
  online?: boolean;
  /** `lat,lon` — keep events with a physical location within
   * `radius_km` (default 50). */
  near?: string;
  radius_km?: number;
  include_cancelled?: boolean;
  cursor?: string;
  limit?: number;
}

/** `GET /api/v1/yonder/events` — chronological event statuses. */
export function listEvents(query: EventsQuery = {}): Promise<Page<Status>> {
  return requestWithResponse<Status[]>({
    path: "/yonder/events",
    query: { ...query },
  }).then(({ response, data }) => paginated<Status>(response, data));
}

/** Extract the keyset cursor from a `Link`/`next` URL — shared by the
 * events pages' infinite scroll. */
export function cursorOf(
  url: string | undefined,
  key = "cursor",
): string | undefined {
  if (!url) return undefined;
  return new URL(url, location.origin).searchParams.get(key) ?? undefined;
}

/** The FEP-8a8e category list (mirrors the backend's
 * `FEP_CATEGORIES`) — labels are humanized codes, not translated. */
export const FEP_CATEGORIES = [
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
] as const;

/** FEP category code → Font Awesome icon name (registered in
 * `icons.ts`). Unknown codes fall back to `tag`. */
export const EVENT_CATEGORY_ICONS: Record<string, string> = {
  ARTS: "palette",
  AUTO_BOAT_AIR: "car",
  BOOK_CLUBS: "book",
  BUSINESS: "briefcase",
  CAUSES: "bullhorn",
  CLIMATE_ENVIRONMENT: "leaf",
  COMMUNITY: "people-group",
  COMEDY: "face-laugh",
  CRAFTS: "scissors",
  CREATIVE_JAM: "puzzle-piece",
  DIY_MAKER_SPACES: "toolbox",
  FAMILY_EDUCATION: "children",
  FASHION_BEAUTY: "shirt",
  FESTIVALS: "campground",
  FILM_MEDIA: "film",
  FOOD_DRINK: "utensils",
  GAMES: "gamepad",
  INCLUSIVE_SPACES: "universal-access",
  LANGUAGE_CULTURE: "language",
  LEARNING: "book-open",
  LGBTQ: "rainbow",
  MEETING: "user-group",
  MEDITATION_WELLBEING: "spa",
  MOVEMENTS_POLITICS: "flag",
  MUSIC: "music",
  NETWORKING: "network-wired",
  OUTDOORS_ADVENTURE: "mountain",
  PARTY: "champagne-glasses",
  PERFORMING_VISUAL_ARTS: "theater-masks",
  PETS: "dog",
  PHOTOGRAPHY: "camera",
  SCIENCE_TECH: "flask",
  SPIRITUALITY_RELIGION_BELIEFS: "hands-praying",
  SPORTS: "person-running",
  THEATRE: "ticket",
  WORKSHOPS_SKILL_SHARING: "screwdriver-wrench",
};

/** Icon name for a category code — `tag` for codes outside the
 * FEP list (remote events can carry arbitrary categories). */
export function eventCategoryIcon(code: string): string {
  return EVENT_CATEGORY_ICONS[code] ?? "tag";
}

/** `POST /api/v1/yonder/events/{id}/rsvp`. */
export function eventRsvp(
  id: string,
  state: "accepted" | "tentative" | "declined" | "none",
  message?: string,
): Promise<unknown> {
  return requestWithResponse<unknown>({
    path: `/yonder/events/${encodeURIComponent(id)}/rsvp`,
    method: "POST",
    body: { state, message },
  }).then(({ data }) => data);
}

/** `POST /api/v1/yonder/events/{id}/invitees` (organizer). */
export function eventInvitees(
  id: string,
  account_ids: string[],
): Promise<unknown> {
  return requestWithResponse<unknown>({
    path: `/yonder/events/${encodeURIComponent(id)}/invitees`,
    method: "POST",
    body: { account_ids },
  }).then(({ data }) => data);
}

/** `POST /api/v1/yonder/events/{id}/dm-invites` (organizer) —
 * Catodon-style DM invitations, any visibility. */
export function eventDmInvites(
  id: string,
  account_ids: string[],
): Promise<unknown> {
  return requestWithResponse<unknown>({
    path: `/yonder/events/${encodeURIComponent(id)}/dm-invites`,
    method: "POST",
    body: { account_ids },
  }).then(({ data }) => data);
}

/** `DELETE /api/v1/yonder/events/{id}/participants/{account_id}` (organizer). */
export function eventRemoveParticipant(
  id: string,
  accountId: string,
): Promise<unknown> {
  return requestWithResponse<unknown>({
    path: `/yonder/events/${encodeURIComponent(id)}/participants/${encodeURIComponent(accountId)}`,
    method: "DELETE",
  }).then(({ data }) => data);
}

/** `POST .../participation_requests/{account_id}/{decision}` (organizer). */
export function eventDecideRequest(
  id: string,
  accountId: string,
  decision: "authorize" | "reject",
): Promise<unknown> {
  return requestWithResponse<unknown>({
    path: `/yonder/events/${encodeURIComponent(id)}/participation_requests/${encodeURIComponent(accountId)}/${decision}`,
    method: "POST",
  }).then(({ data }) => data);
}

export interface EventParticipationRow {
  account_id: string;
  /** Full Account entity (avatar, display name, acct, emojis);
   * `null` when the account row is gone. */
  account?: Account | null;
  state: string;
  participation_message?: string | null;
}

/** `GET .../participations` (organizer). */
export function eventParticipations(
  id: string,
): Promise<{ participations: EventParticipationRow[] }> {
  return requestWithResponse<{ participations: EventParticipationRow[] }>({
    path: `/yonder/events/${encodeURIComponent(id)}/participations`,
  }).then(({ data }) => data);
}

/** `POST /api/v1/yonder/calendar/token` — mint/rotate subscription. */
export function calendarTokenCreate(): Promise<{ calendar_url: string }> {
  return requestWithResponse<{ calendar_url: string }>({
    path: "/yonder/calendar/token",
    method: "POST",
  }).then(({ data }) => data);
}

/** `DELETE /api/v1/yonder/calendar/token` — revoke subscription. */
export function calendarTokenRevoke(): Promise<unknown> {
  return requestWithResponse<unknown>({
    path: "/yonder/calendar/token",
    method: "DELETE",
  }).then(({ data }) => data);
}
