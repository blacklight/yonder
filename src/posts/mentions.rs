//! Mention extraction and resolution — epic 12.
//!
//! Two mention shapes are scanned from the raw source: local
//! `@username` and remote `@username@domain` (pubby's `MENTION_RE`
//! only matches the remote shape, so both are scanned here). Local
//! mentions resolve against local `accounts` rows; remote mentions
//! look up `accounts` and fall back to a WebFinger resolve through
//! pubby's SSRF-guarded client — a failed resolve drops the mention
//! with a logged warning, never fails the post.
//!
//! Replies carry the parent's author + its `status_mentions` into the
//! new status's mention set (Mastodon semantics — also what makes
//! `direct` replies reach the right audience), deduped and excluding
//! the post's own author. An *explicit* self-mention typed in the
//! source is kept though — Mastodon records it too: it linkifies in
//! the content and lands in `status_mentions`/`Mention` tags, while
//! `notifications::emit` already skips notifying the author.

use serde_json::json;
use sqlx::Row;

use crate::{Error, Result};

use super::{render::MentionLink, PostContext, StatusRow};

/// One `@token` found in the source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionToken {
    /// The literal token as typed (`@alice`, `@bob@remote.example`).
    pub token: String,
    /// Username part.
    pub username: String,
    /// Domain part — `None` for local mentions.
    pub domain: Option<String>,
}

/// A mention resolved to an `accounts` row.
#[derive(Debug, Clone)]
pub struct ResolvedMention {
    /// `accounts.id` — feeds `status_mentions`.
    pub account_id: i64,
    /// `accounts.actor_id` — the AP `Mention.href` and audience entry.
    pub actor_url: String,
    /// Profile URL the rendered anchor points to (`accounts.url`,
    /// falling back to `actor_url`).
    pub profile_url: String,
    /// Display `acct` (`@user` local, `@user@domain` remote) for the
    /// entity `mentions[]`.
    pub acct: String,
    /// The token as it appeared in the source — empty for carried-over
    /// mentions (nothing to linkify).
    pub token: String,
}

impl ResolvedMention {
    /// The render-time link — only for tokens present in the source.
    pub fn link(&self) -> Option<MentionLink> {
        if self.token.is_empty() {
            None
        } else {
            Some(MentionLink {
                token: self.token.clone(),
                href: self.profile_url.clone(),
            })
        }
    }
}

/// Whether the character before a `@` candidate disqualifies it —
/// same rule as pubby's `mention_start_ok` (word chars and `_`).
fn boundary_ok(text: &str, at: usize) -> bool {
    match text[..at].chars().last() {
        None => true,
        Some(c) => !(c.is_alphanumeric() || c == '_'),
    }
}

/// A domain is a plausible host when it contains a `.` and its TLD is
/// alphabetic — mirrors pubby's `MENTION_RE` domain shape loosely.
pub(crate) fn plausible_domain(domain: &str) -> bool {
    match domain.rsplit_once('.') {
        Some((head, tld)) => {
            !head.is_empty()
                && tld.len() >= 2
                && tld.chars().all(|c| c.is_ascii_alphabetic())
                && domain
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        }
        None => false,
    }
}

/// Scan `text` for `@user` / `@user@domain` mention tokens. Boundary
/// rules match pubby's (`mention_start_ok`), so `a@b.com` (email) does
/// not match.
pub fn extract(text: &str) -> Vec<MentionToken> {
    let mut out: Vec<MentionToken> = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = text[pos..].find('@') {
        let at = pos + rel;
        if !boundary_ok(text, at) {
            pos = at + 1;
            continue;
        }
        let name_start = at + 1;
        let name_end = text[name_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'))
            .map(|e| name_start + e)
            .unwrap_or(text.len());
        let username = text[name_start..name_end]
            .trim_start_matches(['-', '.'])
            .trim_end_matches(['-', '.']);
        if username.is_empty() || username.chars().count() > 100 {
            pos = name_end.max(at + 1);
            continue;
        }
        // Optional `@domain` — only accepted when it is a plausible
        // host; otherwise the token is a local mention and the `@…`
        // stays plain text for the next iteration to re-scan.
        let mut end = name_end;
        let mut domain: Option<String> = None;
        if text[name_end..].starts_with('@') {
            let dstart = name_end + 1;
            let dend = text[dstart..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
                .map(|e| dstart + e)
                .unwrap_or(text.len());
            let d = text[dstart..dend].trim_end_matches(['.', '-']);
            let d_end = dstart + d.len();
            if !d.is_empty() && plausible_domain(d) {
                domain = Some(d.to_string());
                end = d_end;
            }
        }
        let token = &text[at..end];
        if !out.iter().any(|t: &MentionToken| t.token == token) {
            out.push(MentionToken {
                token: token.to_string(),
                username: username.to_string(),
                domain,
            });
        }
        pos = end.max(at + 1);
    }
    out
}

/// Resolve one token to an [`ResolvedMention`] — `None` when no local/
/// known-remote account matches and WebFinger resolution is
/// unavailable or failed.
async fn resolve_token(ctx: &PostContext, token: &MentionToken) -> Result<Option<ResolvedMention>> {
    let row = match &token.domain {
        None => {
            sqlx::query(
                "SELECT id, actor_id, url, username, domain FROM accounts \
                 WHERE lower(username) = lower($1) AND domain IS NULL AND deleted_at IS NULL",
            )
            .bind(&token.username)
            .fetch_optional(&ctx.db)
            .await?
        }
        Some(domain) => {
            match sqlx::query(
                "SELECT id, actor_id, url, username, domain FROM accounts \
                 WHERE lower(username) = lower($1) AND lower(domain) = lower($2)",
            )
            .bind(&token.username)
            .bind(domain)
            .fetch_optional(&ctx.db)
            .await?
            {
                Some(row) => Some(row),
                // Unknown remote — WebFinger resolve (SSRF-guarded,
                // bounded by federation.request_timeout_seconds), then
                // a bare `accounts` upsert plus a `RemoteActorFetch`
                // enrichment, exactly like inbound Flag handling.
                None => match crate::activitypub::lookup::resolve_actor_url(
                    ctx.ap.as_ref(),
                    &token.username,
                    domain,
                )
                .await?
                {
                    Some(actor_url) => {
                        crate::activitypub::lookup::upsert_remote_stub(
                            &ctx.db,
                            &ctx.dispatcher,
                            &actor_url,
                        )
                        .await?;
                        sqlx::query(
                            "SELECT id, actor_id, url, username, domain FROM accounts \
                             WHERE actor_id = $1",
                        )
                        .bind(&actor_url)
                        .fetch_optional(&ctx.db)
                        .await?
                    }
                    None => None,
                },
            }
        }
    };
    let Some(row) = row else {
        tracing::debug!(mention = %token.token, "mention unresolved; dropped");
        return Ok(None);
    };
    let actor_url: String = row.get("actor_id");
    let username: String = row.get("username");
    let domain: Option<String> = row.get("domain");
    let acct = match &domain {
        Some(d) => format!("@{username}@{d}"),
        None => format!("@{username}"),
    };
    Ok(Some(ResolvedMention {
        account_id: row.get("id"),
        actor_url: actor_url.clone(),
        profile_url: row
            .get::<Option<String>, _>("url")
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| actor_url.clone()),
        acct,
        token: token.token.clone(),
    }))
}

/// Resolve every mention in `source` for `author_account_id`, plus
/// reply carry-over (`in_reply_to`'s author and its `status_mentions`,
/// deduped, excluding the author). Returns mentions in source order
/// followed by carried-over ones.
pub async fn resolve(
    ctx: &PostContext,
    source: &str,
    author_account_id: i64,
    in_reply_to: Option<&StatusRow>,
) -> Result<Vec<ResolvedMention>> {
    let mut out: Vec<ResolvedMention> = Vec::new();
    for token in extract(source) {
        if let Some(m) = resolve_token(ctx, &token).await? {
            // Explicit self-mentions are kept — Mastodon records and
            // linkifies them (only notifications skip the author).
            if !out.iter().any(|o| o.account_id == m.account_id) {
                out.push(m);
            }
        }
    }

    if let Some(parent) = in_reply_to {
        // Carry-over: parent author + parent's mentions, excluding the
        // author and anything already resolved. Tokens stay empty —
        // implicit mentions are audience/tags only, not linkified.
        let rows = sqlx::query(
            "SELECT a.id, a.actor_id, a.url, a.username, a.domain FROM accounts a \
             WHERE a.id IN ( \
                 SELECT account_id FROM status_mentions WHERE status_id = $1 \
                 UNION SELECT $2 \
             ) AND a.id <> $3 AND a.deleted_at IS NULL",
        )
        .bind(parent.id)
        .bind(parent.account_id)
        .bind(author_account_id)
        .fetch_all(&ctx.db)
        .await?;
        for row in rows {
            let account_id: i64 = row.get("id");
            if out.iter().any(|o| o.account_id == account_id) {
                continue;
            }
            let actor_url: String = row.get("actor_id");
            let username: String = row.get("username");
            let domain: Option<String> = row.get("domain");
            out.push(ResolvedMention {
                account_id,
                actor_url: actor_url.clone(),
                profile_url: row
                    .get::<Option<String>, _>("url")
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| actor_url.clone()),
                acct: match &domain {
                    Some(d) => format!("@{username}@{d}"),
                    None => format!("@{username}"),
                },
                token: String::new(),
            });
        }
    }
    Ok(out)
}

/// Persist the `status_mentions` rows (call inside the status txn).
pub async fn persist(
    conn: &mut sqlx::PgConnection,
    status_id: i64,
    mentions: &[ResolvedMention],
) -> Result<()> {
    for m in mentions {
        sqlx::query(
            "INSERT INTO status_mentions (status_id, account_id) VALUES ($1, $2) \
             ON CONFLICT DO NOTHING",
        )
        .bind(status_id)
        .bind(m.account_id)
        .execute(&mut *conn)
        .await?;
    }
    // Mentions are final at this point — refresh the conversation key
    // (epic 25; covers local create and edit, both inside the same tx).
    super::conversations::refresh_conversation_key(&mut *conn, status_id).await?;
    Ok(())
}

/// The `accounts` lookup used by [`crate::posts::remote`] when a
/// `Mention` tag names a known actor.
pub async fn account_id_for_actor(
    ctx: &PostContext,
    actor_url: &str,
) -> Result<Option<(i64, String, Option<String>, String)>> {
    let row =
        sqlx::query("SELECT id, username, domain, actor_id FROM accounts WHERE actor_id = $1")
            .bind(actor_url)
            .fetch_optional(&ctx.db)
            .await?;
    Ok(row.map(|r| {
        (
            r.get("id"),
            r.get("username"),
            r.get("domain"),
            r.get("actor_id"),
        )
    }))
}

/// Map the resolved set to AP `Mention` tag objects.
pub fn mention_tags(mentions: &[ResolvedMention]) -> Vec<serde_json::Value> {
    mentions
        .iter()
        .map(|m| {
            json!({
                "type": "Mention",
                "href": m.actor_url,
                "name": m.acct,
            })
        })
        .collect()
}

/// The audience list — actor URLs (deduped).
pub fn mention_actor_urls(mentions: &[ResolvedMention]) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for m in mentions {
        if !urls.contains(&m.actor_url) {
            urls.push(m.actor_url.clone());
        }
    }
    urls
}

/// Error wrapper for callers that never let a failed resolution fail
/// the post (kept for symmetry with the create path).
pub fn warn_unresolved(token: &MentionToken, error: &Error) {
    tracing::warn!(mention = %token.token, error = %error, "mention resolution failed; dropped");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<String> {
        extract(text).into_iter().map(|t| t.token).collect()
    }

    #[test]
    fn extracts_local_and_remote() {
        assert_eq!(
            tokens("hi @alice and @bob@remote.example!"),
            vec!["@alice", "@bob@remote.example"]
        );
    }

    #[test]
    fn email_is_not_a_mention() {
        assert_eq!(tokens("mail me at a@b.com please"), Vec::<String>::new());
    }

    #[test]
    fn mention_needs_boundary() {
        assert_eq!(tokens("xa@alice"), Vec::<String>::new());
        assert_eq!(tokens("(@alice)"), vec!["@alice"]);
    }

    #[test]
    fn dedupes_tokens() {
        assert_eq!(tokens("@alice @alice @alice"), vec!["@alice"]);
    }
}
