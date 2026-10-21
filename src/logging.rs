//! `tracing` initialization.
//!
//! [`init`] installs the global subscriber from `server.log_format` and
//! `server.log_level`; `RUST_LOG` overrides the configured level when set.
//! Request-scoped spans (a `tower-http` `TraceLayer` carrying
//! `info_span!` fields) land with the HTTP stack in epic 04.

use tracing_subscriber::{fmt, EnvFilter};

use crate::{
    config::{LogFormat, ServerConfig},
    Error, Result,
};

/// Install the global `tracing` subscriber. Must run before any work that
/// should be observed — subsequent calls return an error.
pub fn init(server: &ServerConfig) -> Result<()> {
    let default_directive = server.log_level.parse().map_err(|_| {
        Error::Internal(format!("invalid server.log_level: {:?}", server.log_level))
    })?;
    let filter = EnvFilter::builder()
        .with_default_directive(default_directive)
        .from_env_lossy();

    match server.log_format {
        LogFormat::Pretty => fmt().pretty().with_env_filter(filter).try_init(),
        LogFormat::Json => fmt().json().with_env_filter(filter).try_init(),
        LogFormat::Compact => fmt().compact().with_env_filter(filter).try_init(),
    }
    .map_err(|e| Error::Internal(format!("failed to initialize logging: {e}")))
}
