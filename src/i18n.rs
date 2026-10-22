//! Locale constants shared by configuration (`i18n.fallback_locale`)
//! and the user-settings domain — kept as a leaf so `config` does not
//! depend on `users`.

/// UI and posting locales Yonder supports (BOOTSTRAP i18n set — the
/// frontend ships translations for exactly these).
pub const SUPPORTED_LOCALES: &[&str] = &["en", "es", "it", "de", "ar"];
