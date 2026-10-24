//! The shared WHERE-predicate builder every timeline scope uses.
//!
//! Generated clauses reference two query aliases — `s` for `statuses`
//! and `a` for the author's `accounts` row — so callers must shape
//! their FROM as `statuses s JOIN accounts a ON a.id = s.account_id`.
//!
//! Bind convention: when `Filters::viewer` is `Some`, the viewer
//! account id is bind `$1` — exactly what the epic-14 `*_EXCLUSION`
//! fragments in `social::visibility` expect. Every other bind is
//! allocated in order after it.

use crate::social::visibility;

use super::scope::{RepliesPolicy, TimelineScope};

/// A query bind parameter, in `$n` order.
#[derive(Debug)]
pub enum Bind {
    /// BIGINT.
    I64(i64),
    /// BIGINT[] (`= ANY($n)`).
    I64s(Vec<i64>),
    /// TEXT.
    Text(String),
    /// TEXT[] (`= ANY($n)`).
    Texts(Vec<String>),
}

/// Read filters applied on top of a scope — all evaluated at read
/// time so settings changes never require re-fanning out.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    /// The viewing `accounts.id` — `None` for anonymous requests.
    /// Bound as `$1` when `Some`; required for `Home`/`List`.
    pub viewer: Option<i64>,
    /// `users.filter_languages` — statuses posted in these languages
    /// are hidden (a `NULL` language always passes).
    pub hidden_languages: Vec<String>,
    /// Mastodon's `?only_media=` — only statuses with attachments.
    pub only_media: bool,
    /// Mastodon's `?exclude_replies=` on the account tab — replies are
    /// hidden, except the author's own threads (self-replies stay).
    pub exclude_replies: bool,
    /// Mastodon's `?exclude_reblogs=` on the account tab — boost rows
    /// (`reblog_of_id IS NOT NULL`) are hidden.
    pub exclude_reblogs: bool,
    /// Feed-only restriction (epic 19): `public` posts exclusively,
    /// tightening scopes that would otherwise admit `unlisted` — e.g.
    /// the anonymous `Account` scope on profile feeds.
    pub public_only: bool,
}

/// A finished predicate: clause(s) joined by `AND` plus the binds to
/// apply, in order.
pub struct Where {
    /// `a AND b AND …` — always non-empty (`tombstone`/`VISIBLE`
    /// clauses are unconditional).
    pub sql: String,
    /// Binds for `$1..$n`, in order.
    pub binds: Vec<Bind>,
}

struct Builder {
    clauses: Vec<String>,
    binds: Vec<Bind>,
}

impl Builder {
    fn clause(&mut self, clause: impl Into<String>) {
        self.clauses.push(clause.into());
    }

    /// Allocate a bind and return its `$n` placeholder. The returned
    /// placeholder may be referenced by several clauses.
    fn param(&mut self, bind: Bind) -> String {
        self.binds.push(bind);
        format!("${}", self.binds.len())
    }
}

/// Build the WHERE clause for `scope` + `filters`. `Home` and `List`
/// are auth-required scopes; with `viewer: None` they emit a `FALSE`
/// clause (empty result) instead of misbinding `$1`.
pub fn build_where(scope: &TimelineScope, filters: &Filters) -> Where {
    let mut b = Builder {
        clauses: Vec::new(),
        binds: Vec::new(),
    };
    // $1 is the viewer whenever one exists — the epic-14 fragments
    // interpolate `$1` literally.
    if let Some(viewer) = filters.viewer {
        b.binds.push(Bind::I64(viewer));
    }

    b.clause("NOT s.tombstone");
    b.clause(format!("({})", visibility::VISIBLE));
    if filters.viewer.is_some() {
        b.clause(visibility::BLOCKS_EXCLUSION);
        b.clause(visibility::MUTES_EXCLUSION);
        b.clause(visibility::DOMAIN_BLOCKS_EXCLUSION);
    }

    // Instance moderation (epic 21 writes `domain_blocks`; reads can
    // honour it today): suspended domains vanish everywhere; limited
    // or timeline-removed domains only vanish from the public scopes
    // — followers keep seeing them in home.
    b.clause(
        "NOT EXISTS (SELECT 1 FROM domain_blocks idb \
         WHERE idb.domain = a.domain AND idb.action = 'suspend')",
    );
    if matches!(
        scope,
        TimelineScope::Local
            | TimelineScope::Federated { .. }
            | TimelineScope::Tag { .. }
            | TimelineScope::Account { .. }
    ) {
        b.clause(
            "NOT EXISTS (SELECT 1 FROM domain_blocks idb \
             WHERE idb.domain = a.domain \
               AND idb.action IN ('limit', 'remove_from_timeline'))",
        );
    }

    if !filters.hidden_languages.is_empty() {
        let p = b.param(Bind::Texts(filters.hidden_languages.clone()));
        b.clause(format!(
            "(s.language IS NULL OR NOT (s.language = ANY({p})))"
        ));
    }
    if filters.only_media {
        b.clause("EXISTS (SELECT 1 FROM media_attachments ma WHERE ma.status_id = s.id)");
    }
    if filters.exclude_replies {
        b.clause("(s.in_reply_to_id IS NULL OR s.in_reply_to_account_id = s.account_id)");
    }
    if filters.exclude_reblogs {
        b.clause("s.reblog_of_id IS NULL");
    }
    if filters.public_only {
        b.clause("s.visibility = 'public'");
    }

    // Viewer-scoped timelines can't resolve without $1 — emit an
    // empty match rather than a misbound clause.
    if matches!(scope, TimelineScope::Home | TimelineScope::List { .. }) && filters.viewer.is_none()
    {
        b.clause("FALSE");
    } else {
        scope_clauses(&mut b, scope, filters);
    }
    Where {
        sql: b.clauses.join(" AND "),
        binds: b.binds,
    }
}

fn scope_clauses(b: &mut Builder, scope: &TimelineScope, filters: &Filters) {
    match scope {
        TimelineScope::Local => {
            b.clause("a.domain IS NULL");
            b.clause("s.visibility = 'public'");
        }
        TimelineScope::Federated { remote_only } => {
            b.clause("s.visibility = 'public'");
            if *remote_only {
                b.clause("a.domain IS NOT NULL");
            }
        }
        TimelineScope::Tag { name, local_only } => {
            let p = b.param(Bind::Text(name.clone()));
            b.clause(format!(
                "EXISTS (SELECT 1 FROM status_tags st JOIN tags t ON t.id = st.tag_id \
                 WHERE st.status_id = s.id AND lower(t.name) = lower({p}))"
            ));
            b.clause("s.visibility = 'public'");
            if *local_only {
                b.clause("a.domain IS NULL");
            }
        }
        TimelineScope::Home => home_clauses(b),
        TimelineScope::List {
            list_id,
            replies_policy,
        } => list_clauses(b, *list_id, *replies_policy),
        TimelineScope::Account { account_id } => account_clauses(b, *account_id, filters),
    }
}

/// The profile-page tab: the account's own statuses, gated on the
/// viewer. Anonymous viewers see `public`/`unlisted` only; the owner
/// sees everything but `direct`; followers also see the
/// follower-gated visibilities.
fn account_clauses(b: &mut Builder, account_id: i64, filters: &Filters) {
    let ap = b.param(Bind::I64(account_id));
    b.clause(format!("s.account_id = {ap}"));
    if filters.viewer.is_some() {
        b.clause(
            "(s.visibility IN ('public', 'unlisted') \
             OR (s.account_id = $1 AND s.visibility <> 'direct') \
             OR (s.visibility IN ('followers_only', 'private') \
                 AND EXISTS (SELECT 1 FROM follows f \
                             WHERE f.account_id = $1 \
                               AND f.target_account_id = s.account_id)))",
        );
    } else {
        b.clause("s.visibility IN ('public', 'unlisted')");
    }
}

fn home_clauses(b: &mut Builder) {
    // Audience: own statuses plus followed authors' — this join
    // doubles as the followers_only gate.
    b.clause(
        "(s.account_id = $1 OR EXISTS (SELECT 1 FROM follows f \
         WHERE f.account_id = $1 AND f.target_account_id = s.account_id))",
    );
    // `direct` needs a mention; the rest pass through the author gate.
    b.clause(
        "(s.account_id = $1 OR s.visibility <> 'direct' \
         OR EXISTS (SELECT 1 FROM status_mentions sm \
                    WHERE sm.status_id = s.id AND sm.account_id = $1))",
    );
    // Hide-boosts: a followee's boosts disappear when
    // `follows.show_reblogs` is off. Own boosts always show.
    b.clause(
        "NOT (s.reblog_of_id IS NOT NULL AND s.account_id <> $1 \
         AND EXISTS (SELECT 1 FROM follows fh \
                     WHERE fh.account_id = $1 AND fh.target_account_id = s.account_id \
                       AND NOT fh.show_reblogs))",
    );
    // Exclusive lists remove their members from home.
    b.clause(
        "NOT EXISTS (SELECT 1 FROM list_accounts la JOIN lists l ON l.id = la.list_id \
         WHERE l.account_id = $1 AND l.exclusive AND la.account_id = s.account_id)",
    );
    // Replies land in home when addressed to the viewer, to the
    // author itself, or to someone the viewer follows.
    b.clause(
        "(s.in_reply_to_account_id IS NULL \
         OR s.in_reply_to_account_id = $1 \
         OR s.in_reply_to_account_id = s.account_id \
         OR EXISTS (SELECT 1 FROM follows fr \
                    WHERE fr.account_id = $1 \
                      AND fr.target_account_id = s.in_reply_to_account_id))",
    );
}

fn list_clauses(b: &mut Builder, list_id: i64, replies_policy: RepliesPolicy) {
    let lp = b.param(Bind::I64(list_id));
    b.clause(format!(
        "EXISTS (SELECT 1 FROM list_accounts la \
         WHERE la.list_id = {lp} AND la.account_id = s.account_id)"
    ));
    // List members are followed accounts, so the whole follower-gated
    // set is fair; `direct` stays out.
    b.clause("s.visibility IN ('public', 'unlisted', 'followers_only', 'private')");
    match replies_policy {
        RepliesPolicy::None => b.clause("s.in_reply_to_id IS NULL"),
        RepliesPolicy::List => b.clause(format!(
            "(s.in_reply_to_id IS NULL OR s.in_reply_to_account_id = s.account_id \
             OR EXISTS (SELECT 1 FROM list_accounts lr \
                        WHERE lr.list_id = {lp} \
                          AND lr.account_id = s.in_reply_to_account_id))"
        )),
        RepliesPolicy::Followed => b.clause(
            "(s.in_reply_to_id IS NULL OR s.in_reply_to_account_id = s.account_id \
             OR s.in_reply_to_account_id = $1 \
             OR EXISTS (SELECT 1 FROM follows fr \
                        WHERE fr.account_id = $1 \
                          AND fr.target_account_id = s.in_reply_to_account_id))",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn where_for(scope: &TimelineScope, viewer: Option<i64>) -> Where {
        build_where(
            scope,
            &Filters {
                viewer,
                hidden_languages: Vec::new(),
                only_media: false,
                exclude_replies: false,
                exclude_reblogs: false,
                public_only: false,
            },
        )
    }

    #[test]
    fn public_only_adds_visibility_clause() {
        let scope = TimelineScope::Account { account_id: 3 };
        let open = build_where(&scope, &Filters::default());
        assert!(!open.sql.contains("s.visibility = 'public'"));
        let strict = build_where(
            &scope,
            &Filters {
                public_only: true,
                ..Default::default()
            },
        );
        assert!(strict.sql.contains("s.visibility = 'public'"));
    }

    #[test]
    fn viewer_is_bind_one() {
        let w = where_for(&TimelineScope::Home, Some(7));
        assert!(matches!(w.binds.first(), Some(Bind::I64(7))));
        assert_eq!(w.binds.len(), 1);
        // The epic-14 fragments expect alias `a` and the `$1` viewer.
        assert!(w.sql.contains("NOT EXISTS (SELECT 1 FROM blocks b"));
        assert!(w.sql.contains(visibility::MUTES_EXCLUSION));
        assert!(w.sql.contains(visibility::DOMAIN_BLOCKS_EXCLUSION));
    }

    #[test]
    fn home_predicates() {
        let w = where_for(&TimelineScope::Home, Some(1));
        assert!(w.sql.contains("NOT fh.show_reblogs"));
        assert!(w.sql.contains("status_mentions sm"));
        assert!(w.sql.contains("l.exclusive"));
        assert!(w.sql.contains("in_reply_to_account_id"));
        assert!(w.sql.contains("idb.action = 'suspend'"));
        assert!(!w.sql.contains("'remove_from_timeline'"));
    }

    #[test]
    fn public_scopes_are_public_only() {
        for scope in [
            TimelineScope::Local,
            TimelineScope::Federated { remote_only: false },
            TimelineScope::Tag {
                name: "rust".into(),
                local_only: false,
            },
        ] {
            let w = where_for(&scope, None);
            assert!(w.sql.contains("s.visibility = 'public'"), "{scope:?}");
            assert!(w.sql.contains("'remove_from_timeline'"), "{scope:?}");
            // No viewer → no block/mute fragments and no binds.
            assert!(w.binds.is_empty() || matches!(scope, TimelineScope::Tag { .. }));
        }
        let w = where_for(&TimelineScope::Local, None);
        assert!(w.sql.contains("a.domain IS NULL"));
        let w = where_for(&TimelineScope::Federated { remote_only: true }, Some(3));
        assert!(w.sql.contains("a.domain IS NOT NULL"));
        assert_eq!(w.binds.len(), 1);
    }

    #[test]
    fn tag_binds_the_name() {
        let w = where_for(
            &TimelineScope::Tag {
                name: "Rust".into(),
                local_only: false,
            },
            None,
        );
        assert!(matches!(w.binds.first(), Some(Bind::Text(n)) if n == "Rust"));
        assert!(w.sql.contains("lower(t.name) = lower($1)"));
    }

    #[test]
    fn list_policies() {
        let none = where_for(
            &TimelineScope::List {
                list_id: 5,
                replies_policy: RepliesPolicy::None,
            },
            Some(1),
        );
        assert!(none.sql.contains("s.in_reply_to_id IS NULL"));
        assert!(none.sql.contains("la.list_id = $2"));
        let list = where_for(
            &TimelineScope::List {
                list_id: 5,
                replies_policy: RepliesPolicy::List,
            },
            Some(1),
        );
        assert!(list.sql.contains("lr.list_id = $2"));
        let followed = where_for(
            &TimelineScope::List {
                list_id: 5,
                replies_policy: RepliesPolicy::Followed,
            },
            Some(1),
        );
        assert!(followed.sql.contains("fr.account_id = $1"));
    }

    #[test]
    fn language_and_media_filters() {
        let w = build_where(
            &TimelineScope::Local,
            &Filters {
                viewer: None,
                hidden_languages: vec!["fr".into(), "de".into()],
                only_media: true,
                exclude_replies: false,
                exclude_reblogs: false,
                public_only: false,
            },
        );
        assert!(w.sql.contains("s.language = ANY($1)"));
        assert!(w.sql.contains("media_attachments ma"));
        assert!(matches!(w.binds.first(), Some(Bind::Texts(l)) if l.len() == 2));
    }
}
