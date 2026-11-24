import { request } from "./client";

/** `GET /api/v1/yonder/geocoding/search` hit — the §9.1 place
 * location shape, ready to merge into an `EventLocationDraft`. */
export interface GeoSuggestion {
  name?: string | null;
  street_address?: string | null;
  locality?: string | null;
  region?: string | null;
  postal_code?: string | null;
  country?: string | null;
  address_text?: string | null;
  latitude?: number | null;
  longitude?: number | null;
}

/** Address autocomplete — `[]` when the instance has no
 * `[events.geocoding]` provider configured. */
export function geocodeSearch(q: string): Promise<GeoSuggestion[]> {
  return request<GeoSuggestion[]>({
    path: "/yonder/geocoding/search",
    query: { q },
  });
}
