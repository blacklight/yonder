//! Server-side place lookup (`[events.geocoding]`) — the composer
//! address autocomplete and the pl-fe `location_id` flow share this
//! module. `provider = "nominatim"` proxies a Nominatim instance
//! (self-hostable, usage policy requires a descriptive `user_agent`);
//! `provider = "google"` calls the Geocoding API with `google_api_key`.
//! `provider = "none"` disables lookup — search answers `[]`.

use serde::Serialize;
use serde_json::Value;

use crate::config::{EventsGeocodingConfig, GeocodingProvider};
use crate::{Error, Result};

/// One geocoding hit normalized across providers. The field names
/// match the §9.1 `event.locations[]` place shape so the SPA can map
/// a pick straight onto the draft.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PlaceHit {
    pub name: Option<String>,
    pub street_address: Option<String>,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub address_text: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

/// Blocking GET against the configured geocoder — runs under
/// `spawn_blocking` so the synchronous `ureq` agent never stalls a
/// Tokio worker. `user_agent` is sent when non-empty (Nominatim's
/// usage policy requires one; Google ignores it).
pub(crate) fn geocode_fetch(
    url: String,
    user_agent: String,
) -> std::result::Result<Vec<u8>, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .new_agent();
    let mut req = agent.get(&url);
    if !user_agent.is_empty() {
        req = req.header("User-Agent", &user_agent);
    }
    req.call()
        .map_err(|e| format!("geocoder request failed: {e}"))?
        .into_body()
        .with_config()
        .limit(1024 * 1024)
        .read_to_vec()
        .map_err(|e| format!("geocoder request failed: {e}"))
}

/// Async wrapper: clone the config strings, run the blocking fetch
/// off-runtime, map failures to 502.
pub async fn geocode_get(cfg: &EventsGeocodingConfig, url: String) -> Result<Vec<u8>> {
    let user_agent = cfg.user_agent.clone();
    tokio::task::spawn_blocking(move || geocode_fetch(url, user_agent))
        .await
        .map_err(|e| Error::Internal(format!("geocoder task failed: {e}")))?
        .map_err(Error::Upstream)
}

fn encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn num_field(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|n| {
        n.as_f64()
            .or_else(|| n.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

/// Nominatim `search`/`lookup` result → [`PlaceHit`]. `road` +
/// `house_number` compose the street line when both exist.
pub(crate) fn hit_from_nominatim(item: &Value) -> PlaceHit {
    let addr = item.get("address").cloned().unwrap_or(Value::Null);
    let pick = |k: &str| str_field(&addr, k);
    let street = match (pick("road"), pick("house_number")) {
        (Some(road), Some(house)) => Some(format!("{house} {road}")),
        (Some(road), None) => Some(road),
        (None, Some(house)) => Some(house),
        (None, None) => None,
    };
    PlaceHit {
        name: str_field(item, "name").or_else(|| str_field(item, "display_name")),
        street_address: street,
        locality: pick("city")
            .or_else(|| pick("town"))
            .or_else(|| pick("village"))
            .or_else(|| pick("municipality")),
        region: pick("state"),
        postal_code: pick("postcode"),
        country: pick("country"),
        address_text: str_field(item, "display_name"),
        latitude: num_field(item, "lat"),
        longitude: num_field(item, "lon"),
    }
}

/// One `address_components[]` entry → its `long_name` when `types`
/// contains `want`.
fn google_component<'a>(components: &'a [Value], want: &str) -> Option<&'a str> {
    components.iter().find_map(|c| {
        let types = c.get("types")?.as_array()?;
        types
            .iter()
            .any(|t| t.as_str() == Some(want))
            .then(|| c.get("long_name").and_then(Value::as_str))
            .flatten()
    })
}

/// Google Geocoding `results[]` entry → [`PlaceHit`].
fn hit_from_google(result: &Value) -> PlaceHit {
    let components = result
        .get("address_components")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let street = match (
        google_component(&components, "street_number"),
        google_component(&components, "route"),
    ) {
        (Some(num), Some(route)) => Some(format!("{num} {route}")),
        (Some(num), None) => Some(num.to_string()),
        (None, Some(route)) => Some(route.to_string()),
        (None, None) => None,
    };
    let locality = google_component(&components, "locality")
        .or_else(|| google_component(&components, "postal_town"))
        .or_else(|| google_component(&components, "sublocality"))
        .or_else(|| google_component(&components, "neighborhood"));
    let location = result.get("geometry").and_then(|g| g.get("location"));
    PlaceHit {
        name: None,
        street_address: street,
        locality: locality.map(str::to_string),
        region: google_component(&components, "administrative_area_level_1").map(str::to_string),
        postal_code: google_component(&components, "postal_code").map(str::to_string),
        country: google_component(&components, "country").map(str::to_string),
        address_text: str_field(result, "formatted_address"),
        latitude: location.and_then(|l| num_field(l, "lat")),
        longitude: location.and_then(|l| num_field(l, "lng")),
    }
}

async fn search_nominatim(cfg: &EventsGeocodingConfig, query: &str) -> Result<Vec<PlaceHit>> {
    let url = format!(
        "{}/search?format=jsonv2&addressdetails=1&limit=5&q={}",
        cfg.nominatim_url.trim_end_matches('/'),
        encode(query)
    );
    let bytes = geocode_get(cfg, url).await?;
    let results: Vec<Value> = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Upstream(format!("geocoder search failed: {e}")))?;
    Ok(results.iter().map(hit_from_nominatim).collect())
}

async fn search_google(cfg: &EventsGeocodingConfig, query: &str) -> Result<Vec<PlaceHit>> {
    let url = format!(
        "https://maps.googleapis.com/maps/api/geocode/json?address={}&key={}",
        encode(query),
        encode(&cfg.google_api_key)
    );
    let bytes = geocode_get(cfg, url).await?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Upstream(format!("geocoder search failed: {e}")))?;
    match body.get("status").and_then(Value::as_str) {
        Some("OK") => Ok(body
            .get("results")
            .and_then(Value::as_array)
            .map(|rs| rs.iter().take(5).map(hit_from_google).collect())
            .unwrap_or_default()),
        Some("ZERO_RESULTS") => Ok(Vec::new()),
        other => Err(Error::Upstream(format!(
            "geocoder search failed: {}",
            other.unwrap_or("missing status")
        ))),
    }
}

/// Address-autocomplete hits for `query`, normalized across
/// providers. `provider = "none"` (or a `google` provider without a
/// key — logged, not fatal) answers an empty list.
pub async fn search(cfg: &EventsGeocodingConfig, query: &str) -> Result<Vec<PlaceHit>> {
    let query = query.trim();
    if query.len() < 2 {
        return Ok(Vec::new());
    }
    match cfg.provider {
        GeocodingProvider::None => Ok(Vec::new()),
        GeocodingProvider::Nominatim => search_nominatim(cfg, query).await,
        GeocodingProvider::Google => {
            if cfg.google_api_key.trim().is_empty() {
                tracing::warn!("events.geocoding.provider = google but google_api_key is empty");
                return Ok(Vec::new());
            }
            search_google(cfg, query).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nominatim_hit_maps_address_parts() {
        let item = json!({
            "place_id": 123,
            "name": "Cafe",
            "display_name": "Cafe, 12 Main St, Berlin, Deutschland",
            "lat": "52.52",
            "lon": 13.4,
            "address": {
                "house_number": "12",
                "road": "Main St",
                "city": "Berlin",
                "state": "Berlin",
                "postcode": "10115",
                "country": "Deutschland"
            }
        });
        let hit = hit_from_nominatim(&item);
        assert_eq!(hit.name.as_deref(), Some("Cafe"));
        assert_eq!(hit.street_address.as_deref(), Some("12 Main St"));
        assert_eq!(hit.locality.as_deref(), Some("Berlin"));
        assert_eq!(hit.region.as_deref(), Some("Berlin"));
        assert_eq!(hit.postal_code.as_deref(), Some("10115"));
        assert_eq!(hit.country.as_deref(), Some("Deutschland"));
        assert_eq!(
            hit.address_text.as_deref(),
            Some("Cafe, 12 Main St, Berlin, Deutschland")
        );
        assert_eq!(hit.latitude, Some(52.52));
        assert_eq!(hit.longitude, Some(13.4));
    }

    #[test]
    fn google_hit_maps_address_components() {
        let result = json!({
            "formatted_address": "12 Main St, Berlin, Germany",
            "geometry": {"location": {"lat": 52.52, "lng": 13.405}},
            "address_components": [
                {"long_name": "12", "types": ["street_number"]},
                {"long_name": "Main Street", "types": ["route"]},
                {"long_name": "Berlin", "types": ["locality", "political"]},
                {"long_name": "Berlin", "types": ["administrative_area_level_1", "political"]},
                {"long_name": "10115", "types": ["postal_code"]},
                {"long_name": "Germany", "types": ["country", "political"]}
            ]
        });
        let hit = hit_from_google(&result);
        assert_eq!(hit.name, None);
        assert_eq!(hit.street_address.as_deref(), Some("12 Main Street"));
        assert_eq!(hit.locality.as_deref(), Some("Berlin"));
        assert_eq!(hit.region.as_deref(), Some("Berlin"));
        assert_eq!(hit.postal_code.as_deref(), Some("10115"));
        assert_eq!(hit.country.as_deref(), Some("Germany"));
        assert_eq!(
            hit.address_text.as_deref(),
            Some("12 Main St, Berlin, Germany")
        );
        assert_eq!(hit.latitude, Some(52.52));
        assert_eq!(hit.longitude, Some(13.405));
    }
}
