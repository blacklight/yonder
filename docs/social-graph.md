# Social graph

Follows, follow requests, blocks, mutes, per-user domain blocks,
lists, private notes, subscribe-on-post and follower/following
counters — the Mastodon-compatible relationship layer.

## Data model

All tables come from migration `0004`; `0016` adds behavior only:

- `follows` — accepted follows. `show_reblogs` (hide-boosts) and
  `notify` (notify-on-post) reconcile on re-follow.
- `follow_requests` — pending requests (locked local accounts, or
  outbound remote follows awaiting `Accept`). `activity` keeps the raw
  `Follow` for pubby's `Accept`/`Reject` helpers.
- `blocks`, `mutes` (`hide_notifications`, `expires_at`),
  `account_domain_blocks`, `subscriptions`, `account_notes`,
  `lists` + `list_accounts`.

## Counters

`accounts.followers_count`/`following_count` for **local** accounts are
owned by the `follows` triggers in `0016` — every writer (REST,
pubby's storage adapter for inbound Follow/Undo, imports, cascaded
deletes) stays correct with no app code. The migration backfills a
one-shot recount; `social::counters::recount` is the repair tool.

Remote accounts' counts come from their actor documents:
`upsert_remote_account` maps `followersCount`/`followingCount`/
`statusesCount` on every fetch (best-effort).

## Federation

- Outbound follows are real AP: `Follow` through `delivery::publish`,
  `Undo(Follow)` on unfollow. Remote `Accept`/`Reject` settle via
  pubby's `on_follow_response`.
- Inbound `Follow` is pubby-native (auto-accept, or stored request for
  locked accounts). The `on_activity` hook enqueues a delayed
  `FollowEvent` job that reads which row landed and emits `follow` or
  `follow_request` — the hook runs before pubby stores, so the event
  can't be decided inline.
- `Block`/`Undo(Block)` are hand-built (pubby has no builder) and
  published to remote targets. An inbound `Block` severs the
  relationship between the blocked local account and the remote
  blocker in both directions (follows and pending requests).
- Authorize/reject of remote follow requests runs through
  `pubby_sdk::follows::{accept,reject}_follow_request`; the
  `DeliverFn` wrapper stores the `Accept`/`Reject` in `activities`
  before the delivery ledger enqueues it (the ledger only delivers
  stored activities).

## Semantics

- `admin.unfollow_blocked` (default true): blocking deletes `follows`
  and `follow_requests` in both directions.
- `admin.blockers_visible` (default false): when off, `blocked_by` is
  masked in Relationship entities so a blocked user doesn't learn who
  blocked them.
- Blocks forbid follows in both directions (checked in
  `relationships::follow`); mute-with-`duration` sets `expires_at`,
  expired mutes stop matching at read time.
- Notes are private by construction: joined only into the *author's*
  Relationship render.
- `hide_network` (Mastodon `hide_collections`) empties public
  followers/following lists for non-owner viewers.

## Yonder extension

`POST/DELETE /api/v1/accounts/{id}/subscribe` — subscribe to an
account's posts without following (`subscriptions` table). Exposed as
`subscribing` on the Relationship entity. Mastodon clients use
`follow {notify: true}` instead (`notifying`); epic 16's `status`
fanout reads both.

## Endpoints

| Method | Path | Scope |
|---|---|---|
| POST/DELETE | `/api/v1/accounts/{id}/follow` `/unfollow` | `write:follows` |
| POST | `/api/v1/follows` (deprecated, `uri`) | `write:follows` |
| GET | `/api/v1/accounts/relationships?id[]=` | `read:follows` |
| GET | `/api/v1/accounts/{id}/followers` `/following` | public |
| POST | `/api/v1/accounts/{id}/note` (`comment`) | `write:accounts` |
| POST/DELETE | `/api/v1/accounts/{id}/subscribe` | `write:follows` |
| GET | `/api/v1/follow_requests`; POST `/{id}/authorize` `/{id}/reject` | `read/write:follows` |
| POST/DELETE | `/api/v1/accounts/{id}/block` `/mute` | `write:blocks`/`write:mutes` |
| GET | `/api/v1/blocks` `/mutes` | `read:blocks`/`read:mutes` |
| GET/POST/DELETE | `/api/v1/domain_blocks` (`domain`) | `read/write:blocks` |
| GET/POST | `/api/v1/lists`; GET/PUT/DELETE `/{id}` | `read/write:lists` |
| GET/POST/DELETE | `/api/v1/lists/{id}/accounts` (`account_ids[]`) | `read/write:lists` |
| GET | `/api/v1/accounts/{id}/lists` | `read:lists` |

Writes carry the `relation` rate-limit bucket; list endpoints are
`max_id`/`since_id`/`limit` + `Link`-header paginated.
