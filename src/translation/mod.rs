//! Third-party translation for `POST /api/v1/statuses/{id}/translate`.
//!
//! `content.translation_backend` picks the provider; each needs its
//! credentials (or endpoint) set before [`enabled`] reports it usable:
//!
//! | Backend          | Config                                    | Wire format                          |
//! |------------------|-------------------------------------------|--------------------------------------|
//! | `google-web`     | —                                         | GET `translate_a/single` scraper     |
//! | `google-api`     | `google_translate_api_key`                | `translate/v2` JSON                  |
//! | `deepl`          | `deepl_api_key` (+ optional `deepl_api_url`) | `/v2/translate` JSON              |
//! | `libretranslate` | `libretranslate_url` (+ optional key)     | `{url}/translate` JSON               |
//!
//! Calls run on pubby's sync `HttpClient` inside `block_in_place`
//! (same pattern as link-preview fetches) — relaxed, since every
//! target is either a hardcoded public host or an admin-configured
//! URL that legitimately lives on a LAN.

use std::{collections::HashMap, time::Duration};

use pubby_sdk::http::HttpClient;
use serde_json::{json, Value};

use crate::{
    config::{Config, ContentConfig, TranslationBackend},
    Error, Result,
};

/// Per-request timeout for the provider call.
const TIMEOUT: Duration = Duration::from_secs(15);

/// The default DeepL endpoint — the free tier. Pro keys point
/// `content.deepl_api_url` at `https://api.deepl.com`.
const DEEPL_FREE_URL: &str = "https://api-free.deepl.com";

/// Whether a string carries markup — providers with a tag-aware mode
/// keep `<a>`/`<p>` intact instead of translating them as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemFormat {
    Html,
    Plain,
}

/// One source string headed for the backend.
pub struct Item {
    pub text: String,
    pub format: ItemFormat,
}

/// Every translated string plus the metadata the Translation entity
/// reports (`provider`, `detected_source_language`).
pub struct Outcome {
    pub texts: Vec<String>,
    pub detected_language: Option<String>,
    pub provider: &'static str,
}

/// Whether the configured backend has the inputs it needs — the
/// `translation.enabled` flag on `/api/v2/instance` mirrors this.
pub fn enabled(content: &ContentConfig) -> bool {
    match content.translation_backend {
        TranslationBackend::GoogleWeb => true,
        TranslationBackend::GoogleApi => content.google_translate_api_key.is_some(),
        TranslationBackend::DeepL => content.deepl_api_key.is_some(),
        TranslationBackend::LibreTranslate => content.libretranslate_url.is_some(),
    }
}

/// The provider name surfaced on the Translation entity, Mastodon's
/// wording.
fn provider_name(backend: TranslationBackend) -> &'static str {
    match backend {
        TranslationBackend::GoogleWeb | TranslationBackend::GoogleApi => "Google Translate",
        TranslationBackend::DeepL => "DeepL.com",
        TranslationBackend::LibreTranslate => "LibreTranslate",
    }
}

/// Translate every item into `target` (BCP-47). Items run in order;
/// the detected source language is taken from the first response that
/// reports one.
pub async fn translate_all(config: &Config, items: &[Item], target: &str) -> Result<Outcome> {
    let content = &config.content;
    let backend = content.translation_backend;
    let provider = provider_name(backend);
    if !enabled(content) {
        return Err(Error::Internal(format!("{provider} is not configured")));
    }
    let http = HttpClient::new_relaxed(TIMEOUT);
    let mut texts = Vec::with_capacity(items.len());
    let mut detected = None;
    for item in items {
        let (text, lang) = crate::activitypub::actors::sync_call(|| {
            translate_one(&http, backend, content, item, target)
        })
        .map_err(|e| Error::Internal(format!("{provider}: {e}")))?;
        if detected.is_none() {
            detected = lang;
        }
        texts.push(text);
    }
    Ok(Outcome {
        texts,
        detected_language: detected,
        provider,
    })
}

/// One provider call — returns the translated text plus the source
/// language the backend detected (normalized to lowercase BCP-47;
/// DeepL reports it uppercased).
fn translate_one(
    http: &HttpClient,
    backend: TranslationBackend,
    content: &ContentConfig,
    item: &Item,
    target: &str,
) -> pubby_sdk::Result<(String, Option<String>)> {
    let (text, detected) = match backend {
        TranslationBackend::GoogleWeb => google_web(http, &item.text, target)?,
        TranslationBackend::GoogleApi => google_api(http, content, item, target)?,
        TranslationBackend::DeepL => deepl(http, content, item, target)?,
        TranslationBackend::LibreTranslate => libretranslate(http, content, item, target)?,
    };
    Ok((text, detected.map(|l| l.to_lowercase())))
}

/// JSON POST helper — every backend here speaks it except the gtx
/// scraper GET.
fn post_json(
    http: &HttpClient,
    url: &str,
    extra_headers: &[(&str, &str)],
    body: Value,
) -> pubby_sdk::Result<Value> {
    let mut headers = HashMap::from([("Content-Type".to_string(), "application/json".to_string())]);
    for (k, v) in extra_headers {
        headers.insert(k.to_string(), v.to_string());
    }
    let resp = http.post(url, &headers, body.to_string().as_bytes())?;
    if !resp.is_success() {
        return Err(pubby_sdk::ActivityPubError::General(format!(
            "translation request failed: HTTP {}",
            resp.status
        )));
    }
    resp.json()
}

/// `translate.googleapis.com/translate_a/single` — the same free
/// endpoint the Google Translate frontend drives. Takes no format
/// hint; HTML survives the round trip well enough for timeline
/// content.
fn google_web(
    http: &HttpClient,
    text: &str,
    target: &str,
) -> pubby_sdk::Result<(String, Option<String>)> {
    let url = format!(
        "https://translate.googleapis.com/translate_a/single\
         ?client=gtx&sl=auto&tl={}&dt=t&q={}",
        url::form_urlencoded::byte_serialize(target.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(text.as_bytes()).collect::<String>(),
    );
    let resp = http.get(&url, &HashMap::new())?;
    if !resp.is_success() {
        return Err(pubby_sdk::ActivityPubError::General(format!(
            "translation request failed: HTTP {}",
            resp.status
        )));
    }
    let data = resp.json()?;
    // [[["translated","original",…], …], null, "source_lang", …]
    let translated = data[0]
        .as_array()
        .map(|segments| {
            segments
                .iter()
                .filter_map(|s| s[0].as_str())
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());
    let Some(translated) = translated else {
        return Err(pubby_sdk::ActivityPubError::General(
            "malformed translate_a response".to_string(),
        ));
    };
    Ok((translated, data[2].as_str().map(str::to_string)))
}

/// `translation.googleapis.com/language/translate/v2` — billed API,
/// `format` selects tag handling.
fn google_api(
    http: &HttpClient,
    content: &ContentConfig,
    item: &Item,
    target: &str,
) -> pubby_sdk::Result<(String, Option<String>)> {
    let key = content
        .google_translate_api_key
        .as_deref()
        .unwrap_or_default();
    let url = format!(
        "https://translation.googleapis.com/language/translate/v2?key={}",
        url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>(),
    );
    let format = match item.format {
        ItemFormat::Html => "html",
        ItemFormat::Plain => "text",
    };
    let data = post_json(
        http,
        &url,
        &[],
        json!({"q": item.text, "target": target, "format": format}),
    )?;
    let t = &data["data"]["translations"][0];
    let text = t["translatedText"].as_str().ok_or_else(|| {
        pubby_sdk::ActivityPubError::General("malformed translate/v2 response".to_string())
    })?;
    Ok((
        text.to_string(),
        t["detectedSourceLanguage"].as_str().map(str::to_string),
    ))
}

/// `{deepl_api_url}/v2/translate` — free or pro tier via the URL.
/// `tag_handling=html` keeps markup; `source_lang` stays unset so
/// DeepL detects.
fn deepl(
    http: &HttpClient,
    content: &ContentConfig,
    item: &Item,
    target: &str,
) -> pubby_sdk::Result<(String, Option<String>)> {
    let key = content.deepl_api_key.as_deref().unwrap_or_default();
    let base = content
        .deepl_api_url
        .as_deref()
        .unwrap_or(DEEPL_FREE_URL)
        .trim_end_matches('/');
    let url = format!("{base}/v2/translate");
    let auth = format!("DeepL-Auth-Key {key}");
    let mut body = json!({
        "text": [item.text],
        "target_lang": target.to_uppercase(),
    });
    if item.format == ItemFormat::Html {
        body["tag_handling"] = json!("html");
    }
    let data = post_json(http, &url, &[("Authorization", &auth)], body)?;
    let t = &data["translations"][0];
    let text = t["text"].as_str().ok_or_else(|| {
        pubby_sdk::ActivityPubError::General("malformed DeepL response".to_string())
    })?;
    Ok((
        text.to_string(),
        t["detected_source_language"].as_str().map(str::to_string),
    ))
}

/// `{libretranslate_url}/translate` — `source: "auto"` gets the
/// `detectedLanguage` object back; `format` selects tag handling.
fn libretranslate(
    http: &HttpClient,
    content: &ContentConfig,
    item: &Item,
    target: &str,
) -> pubby_sdk::Result<(String, Option<String>)> {
    let base = content
        .libretranslate_url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/');
    let url = format!("{base}/translate");
    let format = match item.format {
        ItemFormat::Html => "html",
        ItemFormat::Plain => "text",
    };
    let mut body = json!({
        "q": item.text,
        "source": "auto",
        "target": target,
        "format": format,
    });
    if let Some(key) = &content.libretranslate_api_key {
        body["api_key"] = json!(key);
    }
    let data = post_json(http, &url, &[], body)?;
    let text = data["translatedText"].as_str().ok_or_else(|| {
        pubby_sdk::ActivityPubError::General("malformed LibreTranslate response".to_string())
    })?;
    Ok((
        text.to_string(),
        data["detectedLanguage"]["language"]
            .as_str()
            .map(str::to_string),
    ))
}
