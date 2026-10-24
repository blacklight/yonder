//! Client IP resolution.
//!
//! Rate limiting and auth auditing need the real client address. When
//! `server.trusted_proxy_hops` > 0 the request arrives through that
//! many trusted reverse proxies, so the client IP is read from
//! `X-Forwarded-For` (the entry `hops` from the right — proxies append
//! as they forward). Otherwise the socket peer (`ConnectInfo`) is used.

use std::net::SocketAddr;

use axum::{
    extract::ConnectInfo,
    http::{header::HeaderName, Extensions, HeaderMap},
};

static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// Resolve the client IP for a request given the configured count of
/// trusted proxy hops.
pub fn client_ip(headers: &HeaderMap, extensions: &Extensions, trusted_proxy_hops: u16) -> String {
    if trusted_proxy_hops > 0 {
        if let Some(ip) = headers
            .get(&X_FORWARDED_FOR)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| xff_client(v, trusted_proxy_hops))
        {
            return ip;
        }
    }
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Pick the client entry out of an `X-Forwarded-For` list: the value
/// `hops` positions from the right. `hops = 1` means one trusted proxy
/// appended its view of the client, so the client is the last entry.
fn xff_client(header: &str, hops: u16) -> Option<String> {
    let entries: Vec<&str> = header.split(',').map(|e| e.trim()).collect();
    let idx = entries.len().checked_sub(hops as usize)?;
    entries.get(idx).map(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xff_picks_hop_from_right() {
        assert_eq!(
            xff_client("203.0.113.1, 10.0.0.1, 10.0.0.2", 1).as_deref(),
            Some("10.0.0.2")
        );
        assert_eq!(
            xff_client("203.0.113.1, 10.0.0.1, 10.0.0.2", 2).as_deref(),
            Some("10.0.0.1")
        );
        assert_eq!(
            xff_client("203.0.113.1, 10.0.0.1, 10.0.0.2", 3).as_deref(),
            Some("203.0.113.1")
        );
        // More hops configured than entries present — header is
        // spoofed/short; fall back to the socket address.
        assert_eq!(xff_client("203.0.113.1", 2), None);
    }
}

/// Extractor resolving the client IP for session/audit rows — `"unknown"`
/// when neither XFF nor `ConnectInfo` yields an address (e.g. `oneshot`
/// tests without socket info).
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

impl axum::extract::FromRequestParts<std::sync::Arc<crate::http::AppState>> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &std::sync::Arc<crate::http::AppState>,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(ClientIp(client_ip(
            &parts.headers,
            &parts.extensions,
            state.config().server.trusted_proxy_hops,
        )))
    }
}
