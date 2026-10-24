//! Rule application: the `hide` SQL clause for timeline predicates
//! and the `filtered` attribute for `warn`/`blur` (epic 20).
//!
//! A keyword is a word or a `#hashtag`. Words match `content` and
//! `spoiler_text`; hashtag keywords match `status_tags`/`tags` rows,
//! not the literal text. Any `hide` match excludes the status — the
//! SQL side is `NOT (m1 OR m2 OR …)`; the JSON side reports which
//! `warn`/`blur` filters matched.

use serde_json::{json, Value};

use crate::timelines::filter::{Bind, HideClause};

use super::rules::{KeywordMatch, Rule};

/// Is `kw` a hashtag keyword (`#name`) rather than a word?
fn is_hashtag(kw: &str) -> bool {
    kw.starts_with('#')
}

/// One match clause for `kw` — returns (sql_fragment_template, bind).
/// The template contains a single `{p}` the caller replaces with the
/// allocated placeholder.
fn keyword_clause(kw: &KeywordMatch, alloc: &mut impl FnMut(Bind) -> String) -> String {
    if is_hashtag(&kw.keyword) {
        let p = alloc(Bind::Text(
            kw.keyword.trim_start_matches('#').to_lowercase(),
        ));
        return format!(
            "EXISTS (SELECT 1 FROM status_tags st JOIN tags t ON t.id = st.tag_id \
             WHERE st.status_id = s.id AND lower(t.name) = lower({p}))"
        );
    }
    if kw.whole_word {
        let p = alloc(Bind::Text(format!(
            "\\m{}\\M",
            super::rules::escape_regex(&kw.keyword.to_lowercase())
        )));
        format!(
            "(lower(s.content) ~* {p} \
             OR lower(COALESCE(s.spoiler_text, '')) ~* {p})"
        )
    } else {
        let p = alloc(Bind::Text(kw.keyword.to_lowercase()));
        format!(
            "(position({p} in lower(s.content)) > 0 \
             OR position({p} in lower(COALESCE(s.spoiler_text, ''))) > 0)"
        )
    }
}

/// Compile the `hide` rules into one `NOT (…)` clause for the
/// timeline predicates — `None` when no hide rules exist (the common
/// case, zero extra SQL).
pub fn hide_clause(rules: &[Rule]) -> Option<HideClause> {
    let mut binds = Vec::new();
    let mut matches: Vec<String> = Vec::new();
    {
        let mut alloc = |b: Bind| {
            binds.push(b);
            format!("@{}@", binds.len()) // placeholder, renumbered by build_where
        };
        for rule in rules.iter().filter(|r| r.action == "hide") {
            for kw in &rule.keywords {
                matches.push(keyword_clause(kw, &mut alloc));
            }
            if !rule.status_ids.is_empty() {
                // Pinned statuses: match is membership in the rule's
                // own `filter_statuses` list.
                let p = alloc(Bind::I64s(rule.status_ids.clone()));
                matches.push(format!("s.id = ANY({p})"));
            }
        }
    }
    if matches.is_empty() {
        return None;
    }
    Some(HideClause {
        sql: format!("NOT ({})", matches.join(" OR ")),
        binds,
    })
}

/// Postgres `word` characters for `\m`/`\M` boundaries. Caveat: the
/// DB side uses the database locale's word-char set while this uses
/// Unicode alphanumerics — they agree on ASCII and accented Latin but
/// can disagree on more exotic scripts (the `filtered` attribute and
/// the `hide` SQL could then differ; both fail closed on misses).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The in-memory mirror of `lower(haystack) ~* '\m<needle>\M'`:
/// every occurrence of `needle` must sit at a word start AND a word
/// end — i.e. the needle itself must begin and end with a word char
/// (so `c++` or `-foo` can never match, exactly like `\m`/`\M`), and
/// the characters on each side of the match must be non-word (or the
/// string's edges). Multi-word keywords (`hello world`) work: only
/// the outer boundaries matter.
fn whole_word_match(haystack: &str, needle: &str) -> bool {
    let first = needle.chars().next();
    let last = needle.chars().next_back();
    if first.is_none_or(|c| !is_word_char(c)) || last.is_none_or(|c| !is_word_char(c)) {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let (ms, me) = (start + pos, start + pos + needle.len());
        let left_ok = haystack[..ms]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let right_ok = haystack[me..]
            .chars()
            .next()
            .is_none_or(|c| !is_word_char(c));
        if left_ok && right_ok {
            return true;
        }
        // Advance by the needle's first character: `ms + 1` could
        // split a multi-byte char (any match begins with it, so it
        // sits at `ms`) and the next `haystack[start..]` would panic.
        start = ms + first.map_or(1, char::len_utf8);
    }
    false
}

/// Whether `rule` matches this status's text/tags/pin list — the
/// in-memory mirror of [`hide_clause`]'s per-keyword SQL.
fn rule_matches(
    rule: &Rule,
    content: &str,
    spoiler: &str,
    tag_names: &[String],
    status_id: i64,
) -> Vec<String> {
    let mut matched = Vec::new();
    let haystack = format!("{}\n{}", content.to_lowercase(), spoiler.to_lowercase());
    for kw in &rule.keywords {
        let hit = if is_hashtag(&kw.keyword) {
            let name = kw.keyword.trim_start_matches('#').to_lowercase();
            tag_names.iter().any(|t| t.to_lowercase() == name)
        } else {
            let needle = kw.keyword.to_lowercase();
            if kw.whole_word {
                whole_word_match(&haystack, &needle)
            } else {
                haystack.contains(&needle)
            }
        };
        if hit {
            matched.push(kw.keyword.clone());
        }
    }
    if rule.status_ids.contains(&status_id) {
        matched.push("__status".to_string());
    }
    matched
}

/// The `filtered` attribute for one status under `rules` — entries
/// for every `warn`/`blur` rule that matched (hide rules have already
/// removed the row in SQL). `[]` when nothing matched.
pub fn filtered_attribute(
    rules: &[Rule],
    content: &str,
    spoiler: &str,
    tag_names: &[String],
    status_id: i64,
) -> Value {
    let out: Vec<Value> = rules
        .iter()
        .filter(|r| r.action != "hide")
        .filter_map(|r| {
            let hits = rule_matches(r, content, spoiler, tag_names, status_id);
            if hits.is_empty() {
                return None;
            }
            Some(json!({
                "filter": { "id": r.filter_id.to_string(), "title": r.title, "filter_action": r.action },
                "keyword_matches": hits,
            }))
        })
        .collect();
    Value::Array(out)
}

/// Attach `filtered` to a page of already-rendered Status entities.
///
/// `rows` are the page's `StatusRow`s (same order as `entities`);
/// tag names are batch-loaded in one query. With no `warn`/`blur`
/// rules the attribute is `[]` everywhere — the common case costs
/// nothing. `hide` rules never reach here: their rows were already
/// excluded in SQL.
pub async fn attach_filtered(
    db: &sqlx::PgPool,
    rules: &[Rule],
    rows: &[crate::posts::StatusRow],
    entities: &mut [serde_json::Value],
) -> crate::Result<()> {
    use sqlx::Row as _;
    for e in entities.iter_mut() {
        e["filtered"] = Value::Array(Vec::new());
    }
    if rows.is_empty() || entities.len() != rows.len() || !rules.iter().any(|r| r.action != "hide")
    {
        return Ok(());
    }
    let ids: Vec<i64> = rows.iter().map(|s| s.id).collect();
    let tag_rows = sqlx::query(
        "SELECT st.status_id, t.name FROM status_tags st JOIN tags t ON t.id = st.tag_id \
         WHERE st.status_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let mut tag_map: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
    for tr in &tag_rows {
        tag_map
            .entry(tr.get::<i64, _>("status_id"))
            .or_default()
            .push(tr.get("name"));
    }
    for (i, row) in rows.iter().enumerate() {
        let tags = tag_map.get(&row.id).map(Vec::as_slice).unwrap_or(&[]);
        entities[i]["filtered"] = filtered_attribute(
            rules,
            &row.content,
            row.spoiler_text.as_deref().unwrap_or_default(),
            tags,
            row.id,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::rules::KeywordMatch;

    fn rule(action: &str, keywords: &[(&str, bool)], pins: &[i64]) -> Rule {
        Rule {
            filter_id: 1,
            title: "t".into(),
            action: action.into(),
            keywords: keywords
                .iter()
                .enumerate()
                .map(|(i, (k, w))| KeywordMatch {
                    id: i as i64,
                    keyword: k.to_string(),
                    whole_word: *w,
                })
                .collect(),
            status_ids: pins.to_vec(),
        }
    }

    #[test]
    fn whole_word_vs_substring() {
        let rules = vec![rule("warn", &[("cat", true)], &[])];
        assert!(filtered_attribute(&rules, "my category", "", &[], 0)
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            filtered_attribute(&rules, "my cat sleeps", "", &[], 0)
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let rules = vec![rule("warn", &[("cat", false)], &[])];
        assert_eq!(
            filtered_attribute(&rules, "my category", "", &[], 0)
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn hashtag_matches_tags_not_text() {
        let rules = vec![rule("warn", &[("#rust", true)], &[])];
        assert!(filtered_attribute(&rules, "#rust is great", "", &[], 0)
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            filtered_attribute(&rules, "", "", &["rust".into()], 0)
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn whole_word_phrases_match_like_sql() {
        let rules = vec![rule("warn", &[("hello world", true)], &[])];
        assert_eq!(
            filtered_attribute(&rules, "say hello world please", "", &[], 0)
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // Substring-only neighbours don't match.
        assert!(filtered_attribute(&rules, "say hello worlds", "", &[], 0)
            .as_array()
            .unwrap()
            .is_empty());
        assert!(filtered_attribute(&rules, "say xhello world", "", &[], 0)
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn whole_word_edge_keywords() {
        // `\m`/`\M` need word chars at both ends of the match — a
        // keyword starting or ending in punctuation can never match
        // (same as the SQL side).
        let rules = vec![rule("warn", &[("c++", true), ("-foo", true)], &[])];
        assert!(
            filtered_attribute(&rules, "i love c++ and -foo", "", &[], 0)
                .as_array()
                .unwrap()
                .is_empty()
        );
        // Trailing punctuation on the haystack side is a boundary.
        let rules = vec![rule("warn", &[("cat", true)], &[])];
        assert_eq!(
            filtered_attribute(&rules, "one cat.", "", &[], 0)
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn whole_word_multibyte_needles() {
        // A needle starting with a multi-byte char: after a boundary
        // failure the scan must not resume mid-character (regression:
        // `start = ms + 1` used to panic on "réélection"/"élection").
        let rules = vec![rule("warn", &[("élection", true)], &[])];
        assert!(filtered_attribute(&rules, "réélection", "", &[], 0)
            .as_array()
            .unwrap()
            .is_empty());
        assert!(filtered_attribute(&rules, "aété", "", &[], 0)
            .as_array()
            .unwrap()
            .is_empty());
        // …but a genuine boundary match still hits.
        assert_eq!(
            filtered_attribute(&rules, "xélection élection!", "", &[], 0)
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn pinned_status_matches() {
        let rules = vec![rule("warn", &[], &[42])];
        assert_eq!(
            filtered_attribute(&rules, "", "", &[], 42)
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn hide_rules_compile() {
        let rules = vec![
            rule("hide", &[("cat", true), ("#rust", false)], &[7]),
            rule("warn", &[("ignored", false)], &[]),
        ];
        let clause = hide_clause(&rules).expect("hide rules compile");
        assert!(clause.sql.starts_with("NOT ("));
        assert!(clause.sql.contains("status_tags"));
        assert!(clause.sql.contains("~*"));
        assert!(clause.sql.contains("s.id = ANY("));
        // hide-only: the warn rule contributes nothing.
        assert_eq!(clause.binds.len(), 3);
        assert!(hide_clause(&rules[1..]).is_none());
    }
}
