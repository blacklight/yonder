//! Serde helpers for lenient deserialization.
//!
//! Environment variables always arrive as strings; these helpers keep
//! collection-typed keys settable through env vars
//! (`YONDER_HTTP__CORS_ORIGINS="https://a.example,https://b.example"`).

use serde::{de, Deserializer};

/// Accept either a sequence of strings or a comma-separated string and
/// produce `Vec<String>`.
pub fn string_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Visitor;

    impl<'de> de::Visitor<'de> for Visitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or a sequence of strings")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(String::from)
                .collect())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                out.push(item);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(Visitor)
}
