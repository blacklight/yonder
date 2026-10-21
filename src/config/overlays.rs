//! Database config overlays → figment provider.
//!
//! `config_overlays` rows pair a dotted TOML path (`"rate_limit.search.limit"`)
//! with a JSON value. They are folded into a nested object here so
//! `Serialized::defaults` can merge them as the highest-priority layer.

use figment::providers::Serialized;
use serde_json::{Map, Value};

/// Build a figment provider from `(dotted_path, value)` overlay rows.
pub fn provider(overlays: &[(String, Value)]) -> Serialized<Value> {
    Serialized::defaults(to_nested(overlays))
}

/// Fold flat dotted-path keys into a nested JSON object. Later rows win on
/// collisions; a scalar sitting in the middle of a path is replaced by an
/// object so deeper keys can land.
fn to_nested(overlays: &[(String, Value)]) -> Value {
    let mut root = Map::new();
    for (key, value) in overlays {
        let parts: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
        let Some((last, ancestors)) = parts.split_last() else {
            continue;
        };
        let mut node = &mut root;
        for part in ancestors {
            let entry = node
                .entry((*part).to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            node = entry.as_object_mut().expect("entry was coerced to object");
        }
        node.insert((*last).to_string(), value.clone());
    }
    Value::Object(root)
}
