// Hand-written entity interfaces matching the backend's JSON shapes.
// The OpenAPI document types the responses as free-form `Value`
// (handlers build `json!` literals), so these declarations are the
// contract — each field cites the renderer it mirrors.

/** `users::account::to_account` — the public Account entity. */
export interface Account {
  id: string;
  username: string;
  /** `user` locally, `user@domain` for remote accounts. */
  acct: string;
  display_name: string;
  note: string;
  url: string;
  avatar: string;
  header: string;
  locked: boolean;
  bot: boolean;
  discoverable?: boolean;
  /** `users.indexable = false` — the account opted out of search-engine indexing. */
  noindex?: boolean;
  created_at: string;
  followers_count: number;
  following_count: number;
  statuses_count: number;
  /** `users::account::public_fields` — `verified_at` non-null marks verified fields. */
  fields: { name: string; value: string; verified_at?: string | null }[];
  emojis: CustomEmoji[];
  moved?: Account | null;
  last_status_at?: string | null;
  /** Instance-silenced (`limited_at`) — Mastodon's silenced flag. */
  limited?: boolean;
  /** Instance-suspended — the profile renders with a tombstone badge. */
  suspended?: boolean;
  yonder?: {
    avatar_blurhash?: string | null;
    header_blurhash?: string | null;
    /** Staff role — only emitted on local non-`user` accounts. */
    role?: string;
    /** Set when `moved_to_account_id` points at a gone destination. */
    moved_unavailable?: boolean;
  };
}

/** `verify_credentials` extension — CredentialAccount additions. */
export interface CredentialAccount extends Account {
  source?: {
    privacy: string;
    sensitive: boolean;
    language: string | null;
    /** Resolved default `content_type` for new posts. */
    content_type?: string;
    note: string;
    fields: { name: string; value: string }[];
  };
  hide_collections?: boolean;
  indexable?: boolean;
  role?: { id: string; name: string; permissions: string; color: string };
  yonder?: {
    role?: string;
    settings?: YonderSettings;
    /** Email confirmation outstanding — writes 403 until resolved. */
    verification_pending?: boolean;
  };
}

/** `users::settings::yonder_json` — the SPA-facing settings subset. */
export interface YonderSettings {
  interface_language: string | null;
  default_posting_language: string | null;
  default_visibility: string | null;
  default_sensitive: boolean;
  /** `text/plain`/`text/markdown` — `null` = instance default. */
  default_content_type: string | null;
  /** Resolved default (`default_content_type` ?? instance default). */
  effective_content_type?: string;
  theme: string;
  accent: string | null;
  frontend: string | null;
  cw_always_expand: boolean;
  effective_posting_language?: string | null;
  [key: string]: unknown;
}

export interface MediaAttachment {
  id: string;
  type: "image" | "video" | "audio" | "gifv" | string;
  url: string;
  preview_url: string | null;
  remote_url: string | null;
  description: string | null;
  blurhash: string | null;
  meta?: {
    original?: { width?: number; height?: number; aspect?: number };
    focus?: { x: number; y: number };
  };
  yonder?: {
    content_type?: string;
    processed?: boolean;
    /** `{mode}:{latex}` — set on renderer-generated formula images. */
    math_source?: string | null;
  };
}

export interface PreviewCard {
  url: string;
  title: string;
  description: string;
  image: string | null;
  /** `video` cards carry an iframeable player (`embed_url`). */
  type?: "link" | "photo" | "video" | "rich";
  /** Mastodon-compat synthesized `<iframe>` markup. */
  html?: string;
  /** iframeable player URL (video cards only). */
  embed_url?: string;
  width?: number;
  height?: number;
  author_name?: string;
  author_url?: string;
  provider_name?: string;
  provider_url?: string;
}

export interface PollOption {
  title: string;
  votes_count: number | null;
}

/** `posts::polls::entity` — the Mastodon Poll entity. */
export interface Poll {
  id: string;
  expires_at: string | null;
  expired: boolean;
  multiple: boolean;
  votes_count: number;
  voters_count: number;
  voted: boolean;
  own_votes: number[];
  options: PollOption[];
}

export interface StatusMention {
  id: string;
  username: string;
  acct: string;
  url: string;
}

export interface StatusTag {
  name: string;
  url: string;
}

/** One `history[]` bucket on a `Tag` entity (stringified counts). */
export interface TagHistoryDay {
  day: string;
  uses: string;
  accounts: string;
}

/** Mastodon `Tag` entity plus Yonder's `uses`/`accounts` window
 *  totals, which only trend responses carry. */
export interface Tag {
  id: string;
  name: string;
  url: string;
  history: TagHistoryDay[];
  following: boolean;
  featured: boolean;
  uses?: string;
  accounts?: string;
}

export interface CustomEmoji {
  shortcode: string;
  url: string;
  static_url?: string;
  visible_in_picker: boolean;
  category?: string;
}

/** `interactions::reactions::reaction_summary` (`pleroma.emoji_reactions`). */
export interface EmojiReaction {
  name: string;
  count: number;
  me: boolean;
  url?: string | null;
  accounts?: string[];
}

export type StatusVisibility =
  "public" | "unlisted" | "private" | "direct" | "local";

/** `posts::entity::status_entity` — the Status entity. */
export interface Status {
  id: string;
  uri: string;
  url: string | null;
  account: Account;
  /** Rendered HTML (already sanitized server-side). */
  content: string;
  /** `yonder.content_type`: `text/plain`, `text/markdown`, ... */
  content_type?: string;
  created_at: string;
  edited_at: string | null;
  visibility: StatusVisibility;
  sensitive: boolean;
  spoiler_text: string;
  media_attachments: MediaAttachment[];
  mentions: StatusMention[];
  tags: StatusTag[];
  emojis: CustomEmoji[];
  card: PreviewCard | null;
  poll: Poll | null;
  application: unknown;
  language: string | null;
  in_reply_to_id: string | null;
  in_reply_to_account_id: string | null;
  /** Present on boost rows — the wrapped status. */
  reblog: Status | null;
  replies_count: number;
  reblogs_count: number;
  favourites_count: number;
  quotes_count: number;
  quote_id: string | null;
  /** The Mastodon-4.4 quote block — `quoted_status` when materialized. */
  quote: { state: string; quoted_status: Status | null } | null;
  reblogged: boolean;
  favourited: boolean;
  bookmarked: boolean;
  pinned: boolean;
  muted: boolean;
  pleroma?: { emoji_reactions?: EmojiReaction[] };
  yonder?: {
    source?: string;
    content_type?: string;
    quote?: { id: string | null; uri: string | null } | null;
    /** FEP-b2b8 long-form metadata — present (non-null) on `Article`/
     * `Page` statuses. `cover` is a full MediaAttachment entity. */
    article?: {
      title: string | null;
      summary: string | null;
      cover: MediaAttachment | null;
    } | null;
  };
}

/** `statuses::context` — the Mastodon Context entity. */
export interface StatusContext {
  /** Reply chain above the status, root first. */
  ancestors: Status[];
  /** Every visible reply in the subtree, flat and chronological. */
  descendants: Status[];
}

/** `statuses::translate` — the Mastodon Translation entity. */
export interface Translation {
  /** Translated HTML content. */
  content: string;
  /** BCP-47 code the backend detected, when reported. */
  detected_source_language: string | null;
  media_attachments: { id: string; description: string }[];
  poll: { id: string; options: { title: string }[] } | null;
  provider: string;
  spoiler_text: string;
}

/** `posts::entity::scheduled_status_entity`. */
export interface ScheduledStatus {
  id: string;
  scheduled_at: string;
  params: Record<string, unknown>;
  media_attachments: MediaAttachment[];
}

/** `social::entity::relationship` — the full Relationship entity. */
export interface Relationship {
  id: string;
  following: boolean;
  showing_reblogs?: boolean;
  notifying?: boolean;
  followed_by: boolean;
  blocking: boolean;
  blocked_by?: boolean;
  muting: boolean;
  muting_notifications?: boolean;
  requested: boolean;
  requested_by?: boolean;
  domain_blocking?: boolean;
  note: string;
  endorsed?: boolean;
  subscribing?: boolean;
  [key: string]: unknown;
}

/** `notifications::entity` — Mastodon Notification. */
export interface Notification {
  id: string;
  type: string;
  created_at: string;
  /** Set once the row has been rendered — feeds `unread_count`. */
  read?: boolean;
  account: Account | null;
  status?: Status | null;
  /** `admin.report` rows carry the referenced Report object. */
  report?: Report | null;
  /** `move` rows (Yonder/Akkoma extension) carry the destination. */
  target?: Account | null;
}

/** `moderation::reports` entity (subset consumed by the SPA). */
export interface Report {
  id: string;
  action_taken?: boolean;
  comment?: string;
  category?: string;
  account?: Account | null;
  target_account?: Account | null;
  [key: string]: unknown;
}

/** `posts::conversations` + `v1::conversations` — Mastodon Conversation. */
export interface Conversation {
  /** The conversation key (64-char hex), not a numeric id. */
  id: string;
  unread: boolean;
  accounts: Account[];
  last_status: Status | null;
}

/** `social::lists::entity` — a List. */
export interface ListEntity {
  id: string;
  title: string;
  replies_policy: "followed" | "list" | "none";
  exclusive?: boolean;
}

/** `filters` v2 entity (subset). */
export interface Filter {
  id: string;
  title: string;
  context: string[];
  expires_at: string | null;
  filter_action: string;
  keywords: FilterKeyword[];
  statuses: FilterStatus[];
}
export interface FilterKeyword {
  id: string;
  keyword: string;
  whole_word: boolean;
}
export interface FilterStatus {
  id: string;
  status_id: string;
}

/** `tokens::list_user_tokens` — a named token (never carries the secret). */
export interface Token {
  id: string;
  name: string;
  scopes?: string[];
  expires_at?: string | null;
  created_at?: string;
  last_used_at?: string | null;
  [key: string]: unknown;
}
/** `POST /api/v1/tokens` response — the only place the secret appears. */
export interface CreatedToken {
  id: string;
  name: string;
  token: string;
}

/** `sessions::list_sessions` row. */
export interface SessionEntity {
  id: string;
  browser?: string | null;
  ip?: string | null;
  created_at?: string;
  last_activity_at?: string | null;
  current?: boolean;
  [key: string]: unknown;
}

/** `apps::authorized_applications` row. */
export interface AuthorizedApp {
  id: string;
  name: string;
  website?: string | null;
  scopes?: string;
  created_at?: string;
  [key: string]: unknown;
}

/** `transfer::status_json` — an archive_exports/archive_imports row. */
export interface TransferRow {
  id: string;
  kind: string;
  state: string;
  source_type?: string;
  source_origin?: string | null;
  source_actor_uri?: string | null;
  stage?: string | null;
  processed: number;
  total: number | null;
  failed: number;
  file_size: number | null;
  error: string | null;
  options?: Record<string, unknown>;
  category_state?: Record<string, Record<string, unknown>>;
  summary?: Record<string, unknown> | null;
  created_at: string;
  completed_at: string | null;
  expires_at: string | null;
}

/** `import_connections` row projection — never secrets. */
export interface ImportConnection {
  id: string;
  origin: string;
  source_actor_uri: string;
  source_acct: string | null;
  scopes: string;
  state: string;
  created_at: string;
}

/** One `historical_observations` row (owner-scoped read). */
export interface HistoricalObservation {
  kind: "like" | "boost" | "quote" | "reply";
  actor_uri: string | null;
  activity_uri: string | null;
  object_uri: string | null;
  observed_at: string;
  original_at: string | null;
  source_origin: string | null;
  visibility: string;
}

/** One `private_history_messages` row. */
export interface HistoryMessage {
  id: string;
  sender_uri: string;
  recipient_uris: string[];
  body: string | null;
  media: { kind: "local" | "remote"; url: string }[];
  source_uri: string;
  source_origin: string | null;
  original_at: string | null;
}

/** `GET /api/v1/yonder/move` — the account's move state. */
export interface MoveState {
  acct: string;
  moved: boolean;
  target: string | null;
  moved_at: string | null;
  cooldown_until: string | null;
  event: { id: string; state: string; error: string | null } | null;
  followers: {
    migrated: number;
    skipped: number;
    failed: number;
    pending: number;
  } | null;
}

/** `GET /api/v1/yonder/restricted/session` — suspended-account metadata. */
export interface RestrictedSession {
  restricted: boolean;
  account: { username: string; acct: string };
  email: string | null;
  suspended_at: string | null;
  contact_email: string | null;
  contact_account: Account | null;
}

/** `invites::list` row. */
export interface Invite {
  id: string;
  code: string;
  url?: string;
  uses?: number;
  max_uses: number | null;
  expires_at: string | null;
  created_at?: string;
  [key: string]: unknown;
}

/** `GET /api/v1/preferences` — Mastodon Preferences (subset). */
export interface Preferences {
  "posting:default:visibility"?: string;
  "posting:default:sensitive"?: boolean;
  "posting:default:language"?: string | null;
  [key: string]: unknown;
}

/** `two_factor::status` response. */
export interface TwoFactorStatus {
  enabled: boolean;
  /** Factors able to complete a login: "totp" | "webauthn" | "backup_code". */
  methods: string[];
  totp: { enabled: boolean; last_verified_at: string | null };
  webauthn: WebauthnCredential[];
  backup_codes_remaining: number;
  [key: string]: unknown;
}
/** `webauthn_credentials` row. */
export interface WebauthnCredential {
  id: string;
  name: string;
  created_at?: string;
  last_used_at?: string | null;
  [key: string]: unknown;
}

/** `GET /api/v2/instance` — only the fields the SPA consumes. */
export interface InstanceEntity {
  domain?: string;
  title?: string;
  version?: string;
  source_url?: string;
  description?: string;
  short_description?: string;
  languages?: string[];
  /** Mastodon 4.x object; `thumbnail.url` is the /about hero image. */
  thumbnail?: { url?: string; blurhash?: string | null } | null;
  icon?: { src?: string; size?: string }[];
  usage?: { users?: { active_month?: number } };
  contact?: {
    email?: string | null;
    name?: string | null;
    /** Yonder extension — the configured profile/link of the admin. */
    url?: string | null;
    account?: Account | null;
  };
  configuration?: {
    statuses?: {
      max_characters?: number;
      max_media_attachments?: number;
    };
    polls?: {
      max_options?: number;
      max_characters_per_option?: number;
      min_expiration?: number;
      max_expiration?: number;
    };
    translation?: {
      enabled?: boolean;
    };
  };
  rules?: { id: string; text: string }[];
  registrations?: {
    enabled?: boolean;
    approval_required?: boolean;
    message?: string | null;
    invites_enabled?: boolean;
  };
  /** Akkoma-style capability block — `fields_limits` drives the
   * profile-edit caps; emitted identically on v1 and v2. */
  pleroma?: {
    metadata?: {
      vapid_public_key?: string | null;
      fields_limits?: {
        max_fields?: number;
        max_remote_fields?: number;
        name_length?: number;
        value_length?: number;
      };
    };
  };
  /** Yonder instance extensions — `single_user` is the personal-instance
   * redirect target for anonymous `/` visitors (`instance.single_user`). */
  yonder?: {
    single_user?: string | null;
    [key: string]: unknown;
  };
  [key: string]: unknown;
}

/** `statuses::StatusCreateSchema` — the `POST /statuses` body. */
export interface StatusCreateBody {
  /** Raw source text; optional when media or a poll is attached. */
  status?: string;
  content_type?: string;
  visibility?: StatusVisibility;
  language?: string;
  sensitive?: boolean;
  spoiler_text?: string;
  in_reply_to_id?: string;
  media_ids?: string[];
  poll?: {
    options: string[];
    expires_in: number;
    multiple?: boolean;
    hide_totals?: boolean;
  };
  quote_id?: string;
  scheduled_at?: string;
  /** `note`/`article` — FEP-b2b8 long-form text. */
  post_type?: string;
  /** Article headline (AS2 `name`), required for `article`. */
  title?: string;
  /** Article teaser (AS2 `summary`, plain text). */
  summary?: string;
  /** Representative image attachment id (AS2 `image`). */
  cover_media_id?: string;
}
