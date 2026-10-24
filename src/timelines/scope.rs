//! Timeline scopes — which set of statuses a read covers.

/// `lists.replies_policy` — how member replies behave in the feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepliesPolicy {
    /// Replies to people the list owner follows are included.
    Followed,
    /// Replies to other list members are included (Mastodon default).
    List,
    /// No replies at all.
    None,
}

impl RepliesPolicy {
    /// The stored `lists.replies_policy` spelling (`CHECK`
    /// constrains it to these three); unknown values degrade to
    /// `List`, the Mastodon default.
    pub fn parse(stored: &str) -> RepliesPolicy {
        match stored {
            "followed" => RepliesPolicy::Followed,
            "none" => RepliesPolicy::None,
            _ => RepliesPolicy::List,
        }
    }
}

/// Which timeline is being read.
#[derive(Debug, Clone)]
pub enum TimelineScope {
    /// The viewer's home feed: own + followed accounts. Requires a
    /// viewer — `Filters::viewer` must be `Some(account_id)`.
    Home,
    /// Local users' public posts (`?local=true`).
    Local,
    /// Local + remote public posts. `remote_only` implements
    /// Mastodon's `?remote=true` (remote accounts only).
    Federated { remote_only: bool },
    /// Members of one list. `replies_policy` comes from the
    /// `lists` row the handler already loaded for its ownership
    /// check.
    List {
        /// `lists.id` — ownership is verified by the handler.
        list_id: i64,
        /// The list's stored `replies_policy`.
        replies_policy: RepliesPolicy,
    },
    /// Public statuses carrying one hashtag.
    Tag {
        /// Tag text without `#`; matched case-insensitively.
        name: String,
        /// Mastodon's `?local=true` — restrict to local authors.
        local_only: bool,
    },
    /// One account's statuses — the profile page's posts/media tabs
    /// (epic 18). `Filters` carries `exclude_replies`/`exclude_reblogs`/
    /// `only_media`; `Filters::viewer` is whoever is looking, not the
    /// profile owner.
    Account {
        /// `accounts.id` of the profile.
        account_id: i64,
    },
}

impl TimelineScope {
    /// Stable key fragment for cache naming (`timeline:{key}`).
    pub fn key(&self) -> String {
        match self {
            TimelineScope::Home => "home".to_string(),
            TimelineScope::Local => "local".to_string(),
            TimelineScope::Federated { remote_only } => {
                if *remote_only {
                    "remote".to_string()
                } else {
                    "federated".to_string()
                }
            }
            TimelineScope::List { list_id, .. } => format!("list:{list_id}"),
            TimelineScope::Tag { name, local_only } => {
                if *local_only {
                    format!("tag:{}:local", name.to_lowercase())
                } else {
                    format!("tag:{}", name.to_lowercase())
                }
            }
            TimelineScope::Account { account_id } => format!("account:{account_id}"),
        }
    }
}
