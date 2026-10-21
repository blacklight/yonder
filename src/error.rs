//! Crate-wide error type.
//!
//! [`Error`] implements `axum::response::IntoResponse`, emitting
//! Mastodon-style `{"error": "..."}` JSON bodies. Client-facing
//! variants carry their real message; infrastructure variants return a
//! generic 500 so internals never leak (the cause is logged instead).

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;
use utoipa::ToSchema;

#[derive(Debug, Error)]
pub enum Error {
    // figment::Error is ~200 bytes; boxing keeps `Result` small
    // (clippy::result_large_err).
    #[error("configuration error: {0}")]
    Config(#[from] Box<figment::Error>),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),

    #[error("redis pool error: {0}")]
    RedisPool(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0} not found")]
    NotFound(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    #[error("validation error: {0}")]
    Validation(String),

    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<figment::Error> for Error {
    fn from(error: figment::Error) -> Self {
        Self::Config(Box::new(error))
    }
}

impl From<bb8::RunError<redis::RedisError>> for Error {
    fn from(error: bb8::RunError<redis::RedisError>) -> Self {
        match error {
            bb8::RunError::User(e) => Self::Redis(e),
            bb8::RunError::TimedOut => {
                Self::RedisPool("timed out acquiring connection".to_string())
            }
        }
    }
}

impl Error {
    /// HTTP status code this error maps to once it crosses the API boundary.
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Config(_)
            | Self::Database(_)
            | Self::Migration(_)
            | Self::Redis(_)
            | Self::RedisPool(_)
            | Self::Io(_)
            | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Message safe to send to clients: real message for client-facing
    /// variants, generic text for infrastructure failures.
    fn public_message(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::NotFound(_) | Self::Unauthorized | Self::Forbidden | Self::Validation(_) => {
                self.to_string().into()
            }
            _ => "Internal server error".into(),
        }
    }

    /// Machine-readable error kind for the eventual JSON error body.
    pub fn error_kind(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::Database(_) => "database",
            Self::Migration(_) => "migration",
            Self::Redis(_) => "redis",
            Self::RedisPool(_) => "redis_pool",
            Self::Io(_) => "io",
            Self::NotFound(_) => "not_found",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::Validation(_) => "validation",
            Self::Internal(_) => "internal",
        }
    }
}

/// Mastodon-compatible JSON error body: `{"error": "<message>"}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Human-readable error description.
    pub error: String,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status_code();
        if status.is_server_error() {
            tracing::error!(error = %self, kind = self.error_kind(), "request failed");
        }
        (
            status,
            Json(ErrorBody {
                error: self.public_message().into_owned(),
            }),
        )
            .into_response()
    }
}
