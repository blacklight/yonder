//! Stable exit-code mapping for the `yonder` CLI.
//!
//! Command errors map to process exit codes by `Error` variant;
//! usage errors (exit 2) come from clap before dispatch and never
//! reach this function. `migrate status` reports pending migrations
//! via `Ok(1)` upstream — a status signal, not an error.

use crate::Error;

/// The process exit code for a command error.
pub fn exit_code(error: &Error) -> u8 {
    match error {
        Error::NotFound(_) => 3,
        Error::Validation(_) | Error::BadRequest(_) | Error::Conflict(_) => 4,
        Error::Config(_) | Error::Migration(_) => 5,
        Error::Redis(_) | Error::RedisPool(_) | Error::Database(_) => 6,
        Error::Io(_)
        | Error::Internal(_)
        | Error::Federation(_)
        | Error::Forbidden(_)
        | Error::Unauthorized
        | Error::RateLimited(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_maps() {
        let cases: Vec<(Error, u8)> = vec![
            (Error::NotFound("x".into()), 3),
            (Error::Validation("x".into()), 4),
            (Error::BadRequest("x".into()), 4),
            (Error::Conflict("x".into()), 4),
            (
                Error::Config(Box::new(figment::Error::from(
                    figment::error::Kind::Message("x".into()),
                ))),
                5,
            ),
            (
                Error::Migration(sqlx::migrate::MigrateError::VersionNotPresent(1)),
                5,
            ),
            (Error::RedisPool("x".into()), 6),
            (Error::Database(sqlx::Error::RowNotFound), 6),
            (Error::Io(std::io::Error::other("x")), 1),
            (Error::Internal("x".into()), 1),
            (Error::Forbidden("x"), 1),
            (Error::Unauthorized, 1),
            (Error::RateLimited("x".into()), 1),
        ];
        for (error, want) in cases {
            assert_eq!(exit_code(&error), want, "{error:?}");
        }
    }
}
