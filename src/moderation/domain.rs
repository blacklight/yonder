//! Domain normalization for allow/block comparisons — a port of
//! pubby's `moderation.rs` (`normalize_domain`/`extract_domain`) so
//! `domain_blocks` rows and lookups agree on the same canonical form:
//! scheme, path, port and credentials stripped, IDN converted to
//! punycode (the form `accounts.domain` stores), everything
//! lowercased. `""` means "unparseable" — callers reject it rather
//! than storing a row that can never match.

/// Normalize a domain or URL for allow/block comparisons.
///
/// Strips the scheme, path, port and credentials, then lowercases the
/// hostname. Internationalized names come out in punycode — the same
/// serialization the `url` crate gives `accounts.domain`, so SQL
/// equality checks (`idb.domain = a.domain`) match. Returns `""` for
/// empty or unparseable input.
pub fn normalize_domain(value: &str) -> String {
    normalize_inner(value, true)
}

fn normalize_inner(value: &str, retry_percent: bool) -> String {
    let candidate = value.trim().to_lowercase();
    if candidate.is_empty() {
        return String::new();
    }

    // Prefix bare hosts with a dummy *special* scheme so they parse
    // with the host in the netloc position — `url` requires a scheme,
    // and only special schemes (http/https/ws/wss/ftp/file) get IDNA
    // processing on the host.
    let candidate = if candidate.contains("://") {
        candidate
    } else {
        format!("https://{candidate}")
    };

    let host = url::Url::parse(&candidate)
        .ok()
        .and_then(|u| {
            // `url`'s `Host::Domain` is already IDNA-processed
            // (punycode). Bare IPv6 (no brackets) and IP literals are
            // kept as-is.
            match u.host()? {
                url::Host::Ipv6(ip) => Some(ip.to_string()),
                url::Host::Ipv4(ip) => Some(ip.to_string()),
                url::Host::Domain(d) => Some(d.to_string()),
            }
        })
        .map(|h| h.trim().to_string())
        .unwrap_or_default();

    // Non-special schemes (`foo://...`) skip IDNA processing and keep
    // the host percent-encoded — retry once on the decoded form.
    if retry_percent && host.contains('%') {
        return normalize_inner(&host, false);
    }
    host
}

/// Extract the normalized domain from an actor URL, inbox URL, or
/// bare domain.
pub fn extract_domain(url_or_actor: &str) -> String {
    normalize_domain(url_or_actor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_hosts_normalize() {
        assert_eq!(normalize_domain("Example.COM"), "example.com");
        assert_eq!(normalize_domain("  sub.example.com  "), "sub.example.com");
        assert_eq!(normalize_domain("example.com:8443"), "example.com");
    }

    #[test]
    fn urls_normalize_to_host() {
        assert_eq!(
            normalize_domain("https://Example.com:443/users/alice"),
            "example.com"
        );
        assert_eq!(
            normalize_domain("https://user:pw@example.com:8443/ap/inbox"),
            "example.com"
        );
        assert_eq!(normalize_domain("http://example.com/"), "example.com");
    }

    #[test]
    fn idn_becomes_punycode() {
        // Both spellings of the same IDN collapse to the punycode host
        // — the form `accounts.domain` holds for remote actors.
        assert_eq!(
            normalize_domain("xn--nxasmq6b.example"),
            "xn--nxasmq6b.example"
        );
        assert_eq!(normalize_domain("βόλοσ.example"), "xn--nxasmq6b.example");
        // A non-special scheme still collapses to the same host.
        assert_eq!(
            normalize_domain("ap://βόλοσ.example/x"),
            "xn--nxasmq6b.example"
        );
    }

    #[test]
    fn ip_literals_normalize() {
        assert_eq!(normalize_domain("http://1.2.3.4:8080/x"), "1.2.3.4");
        assert_eq!(normalize_domain("http://[2001:db8::1]/x"), "2001:db8::1");
    }

    #[test]
    fn garbage_normalizes_to_empty() {
        assert_eq!(normalize_domain(""), "");
        assert_eq!(normalize_domain("   "), "");
        assert_eq!(normalize_domain("http://"), "");
    }
}
