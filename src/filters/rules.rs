//! The viewer's active filter rules for one context — loaded once
//! per request, expired filters excluded.

use sqlx::{PgPool, Row};

use crate::Result;

/// Where a rule set applies — the values of `filters.context`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterContext {
    Home,
    Notifications,
    Public,
    Thread,
    Account,
}

impl FilterContext {
    /// The `filters.context` array value.
    pub fn as_str(self) -> &'static str {
        match self {
            FilterContext::Home => "home",
            FilterContext::Notifications => "notifications",
            FilterContext::Public => "public",
            FilterContext::Thread => "thread",
            FilterContext::Account => "account",
        }
    }
}

/// One keyword matcher.
#[derive(Debug, Clone)]
pub struct KeywordMatch {
    pub id: i64,
    pub keyword: String,
    pub whole_word: bool,
}

/// A filter as it applies to one context: the filter row plus its
/// keywords and pinned statuses.
#[derive(Debug)]
pub struct Rule {
    pub filter_id: i64,
    pub title: String,
    pub action: String,
    pub keywords: Vec<KeywordMatch>,
    pub status_ids: Vec<i64>,
}

/// Load `account_id`'s non-expired filters that apply in `context`.
/// Anonymous viewers get an empty set — callers short-circuit on it.
pub async fn load_active(
    db: &PgPool,
    account_id: i64,
    context: FilterContext,
) -> Result<Vec<Rule>> {
    let rows = sqlx::query(
        "SELECT id, title, action FROM filters \
         WHERE account_id = $1 AND context @> ARRAY[$2] \
           AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(account_id)
    .bind(context.as_str())
    .fetch_all(db)
    .await?;
    // One query per child table regardless of filter count — a
    // keyword-heavy account must not multiply request latency.
    let ids: Vec<i64> = rows.iter().map(|r| r.get("id")).collect();
    let kw_rows = sqlx::query(
        "SELECT id, filter_id, keyword, whole_word FROM filter_keywords \
         WHERE filter_id = ANY($1) ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let pin_rows =
        sqlx::query("SELECT filter_id, status_id FROM filter_statuses WHERE filter_id = ANY($1)")
            .bind(&ids)
            .fetch_all(db)
            .await?;
    let mut keywords: std::collections::HashMap<i64, Vec<KeywordMatch>> =
        std::collections::HashMap::new();
    for k in &kw_rows {
        keywords
            .entry(k.get("filter_id"))
            .or_default()
            .push(KeywordMatch {
                id: k.get("id"),
                keyword: k.get("keyword"),
                whole_word: k.get("whole_word"),
            });
    }
    let mut pins: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    for p in &pin_rows {
        pins.entry(p.get("filter_id"))
            .or_default()
            .push(p.get("status_id"));
    }
    Ok(rows
        .iter()
        .map(|r| {
            let filter_id: i64 = r.get("id");
            Rule {
                filter_id,
                title: r.get("title"),
                action: r.get("action"),
                keywords: keywords.remove(&filter_id).unwrap_or_default(),
                status_ids: pins.remove(&filter_id).unwrap_or_default(),
            }
        })
        .collect())
}

/// Escape a keyword for embedding in a Postgres POSIX regex —
/// every metacharacter is backslash-escaped so the keyword matches
/// literally. `\m`/`\M` word boundaries are added by the caller.
pub fn escape_regex(keyword: &str) -> String {
    let mut out = String::with_capacity(keyword.len());
    for c in keyword.chars() {
        if "\\.+*?()[]{}|^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_every_metacharacter() {
        assert_eq!(escape_regex("a.b+c(x)"), "a\\.b\\+c\\(x\\)");
        assert_eq!(escape_regex("cat|dog"), "cat\\|dog");
        assert_eq!(escape_regex("\\d+"), "\\\\d\\+");
        assert_eq!(escape_regex("rust"), "rust");
        assert_eq!(escape_regex("[x]$"), "\\[x\\]\\$");
    }
}
