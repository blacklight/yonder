//! Startup banner for `yonder serve`.
//!
//! Printed on stderr once the HTTP listener is bound — the true
//! "started successfully" point. Emitted directly rather than through
//! `tracing` so the art renders identically under every
//! `server.log_format` (a multi-line `info!` event would be escaped
//! into a single JSON `message` string in JSON mode), and kept off
//! stdout so the CLI's `--json` output contract stays clean.

use crate::{activitypub::federation_base_url, config::Config};

/// `toilet -f smblock YONDER`
const ART: &str = "\
▌ ▌▞▀▖▙ ▌▛▀▖▛▀▘▛▀▖
▝▞ ▌ ▌▌▌▌▌ ▌▙▄ ▙▄▘
 ▌ ▌ ▌▌▝▌▌ ▌▌  ▌▚
 ▘ ▝▀ ▘ ▘▀▀ ▀▀▘▘ ▘";

/// The banner text: wordmark, then version/listen and instance
/// feature lines.
fn render(config: &Config, addr: &str) -> String {
    let federation = if config.federation.enabled {
        "on"
    } else {
        "off"
    };
    let worker = if config.jobs.enabled {
        "in-process"
    } else {
        "external"
    };
    format!(
        "{ART}\n\n  yonder {version} — listening on {addr}\n  instance: {base_url} — federation: {federation} — worker: {worker}\n",
        version = env!("CARGO_PKG_VERSION"),
        base_url = federation_base_url(config),
    )
}

/// Print the banner on stderr as a single write so interleaved log
/// lines can't land inside the art.
pub fn print(config: &Config, addr: &str) {
    eprintln!("\n{}", render(config, addr));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_reports_version_listen_and_instance() {
        let mut config = Config::default();
        config.instance.domain = "example.com".into();
        config.federation.enabled = true;
        config.jobs.enabled = false;
        let out = render(&config, "127.0.0.1:8080");
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
        assert!(out.contains("listening on 127.0.0.1:8080"));
        assert!(out.contains("instance: https://example.com"));
        assert!(out.contains("federation: on"));
        assert!(out.contains("worker: external"));
    }

    #[test]
    fn banner_honours_explicit_base_url() {
        let mut config = Config::default();
        config.federation.enabled = false;
        config.federation.base_url = Some("http://localhost:3000/".into());
        let out = render(&config, "0.0.0.0:3000");
        assert!(out.contains("instance: http://localhost:3000"));
        assert!(out.contains("federation: off"));
        assert!(out.contains("worker: in-process"));
    }
}
