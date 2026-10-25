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
  created_at: string;
  followers_count: number;
  following_count: number;
  statuses_count: number;
  fields: { name: string; value: string }[];
  emojis: CustomEmoji[];
  moved?: Account | null;
  last_status_at?: string | null;
}

/** `verify_credentials` extension — CredentialAccount additions. */
export interface CredentialAccount extends Account {
  source?: {
    privacy: string;
    sensitive: boolean;
    language: string | null;
    note: string;
    fields: { name: string; value: string }[];
  };
  hide_collections?: boolean;
  indexable?: boolean;
  role?: { id: string; name: string; permissions: string; color: string };
  yonder?: {
    role?: string;
    settings?: YonderSettings;
  };
}

/** `users::settings::yonder_json` — the SPA-facing settings subset. */
export interface YonderSettings {
  interface_language: string | null;
  default_posting_language: string | null;
  default_visibility: string | null;
  default_sensitive: boolean;
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
  yonder?: { content_type?: string; processed?: boolean };
}

export interface PreviewCard {
  url: string;
  title: string;
  description: string;
  image: string | null;
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

export type StatusVisibility = "public" | "unlisted" | "private" | "direct";

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
  /** The materialized quoted status, when resolvable. */
  quote: Status | null;
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
  };
}

/** `posts::entity::scheduled_status_entity`. */
export interface ScheduledStatus {
  id: string;
  scheduled_at: string;
  params: Record<string, unknown>;
  media_attachments: MediaAttachment[];
}

/** `interactions::entity::relationship` — the Relationship entity. */
export interface Relationship {
  id: string;
  following: boolean;
  followed_by: boolean;
  blocking: boolean;
  muting: boolean;
  requested: boolean;
  note: string;
  [key: string]: unknown;
}

/** `notifications::entity` — Mastodon Notification. */
export interface Notification {
  id: string;
  type: string;
  created_at: string;
  account: Account | null;
  status?: Status | null;
}

/** `GET /api/v1/instance` — only the fields the SPA consumes. */
export interface InstanceEntity {
  uri?: string;
  title?: string;
  version?: string;
  description?: string;
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
  };
  rules?: { id: string; text: string }[];
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
}
