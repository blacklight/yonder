# Webmentions

Yonder speaks [Webmention](https://www.w3.org/TR/webmention/), the W3C
protocol for federated mentions between plain websites, via the
`webmentions-sdk` crate.

## Receiving

`POST /webmentions` accepts `source`/`target` form fields and answers
`202` after enqueueing a `webmention_incoming` job. The job fetches the
source page (SSRF-guarded, body-capped), verifies it links back to the
target, parses Microformats2, and stores the result in the
`webmentions` ledger table.

When the target resolves to a local status (its AP URI
`/ap/users/{u}/statuses/{id}`, web permalink `/@{u}/{id}`, or the SPA
route `/statuses/{id}`), the mention materializes as a **reply
status** authored by a **pseudo-actor** — an `accounts` row keyed on
the author's site:

- `actor_id` is `urn:yonder:webmention:<hash>` — non-HTTP, so the
  remote-actor refresh pipeline never dereferences it.
- `acct` follows the Bridgy convention: `alice@example.com` for an
  author URL with a path, `example.com@example.com` for a bare-site
  author.
- If the author URL already matches a known account's `actor_id`/`url`
  (the author has a real fediverse presence), that account is reused.
- `display_name`/`avatar` come from the page's `h-card`.

The reply is upserted on `urn:yonder:webmention:status:<hash>` —
re-sent mentions update in place (source edits propagate), a source
that disappears or stops linking tombstones the reply. The target
author gets the normal `mention` notification; the reply's visibility
is clamped to the target's.

The endpoint is advertised via `Link: <…/webmentions>; rel="webmention"`
on every `text/*` response and a `<link rel="webmention">` element in
the outgoing source page.

## Sending

Publishing, editing or deleting a local `public`/`unlisted` status
with URLs in its content enqueues a `webmention_outgoing` job. The job
renders the status as a Microformats2 `h-entry` document — the same
document `GET /webmentions/source/{id}` serves for remote verification
— and the SDK extracts its link targets, discovers each target's
endpoint (`Link` header or `<link>`/`<a rel="webmention">`) and POSTs
`source`/`target`. Stored `outgoing` rows diff against the extracted
set so edits deliver new targets and retract removed ones; deletions
retract everything (the source page returns `410`).

Same-instance links are excluded — they already flow through the local
mention pipeline.

The SDK reports per-target delivery failures (a failed discovery
fetch, a `>= 400` endpoint response) through `on_mention_failed` —
the job fails when any target failed, so a transient error retries
with the normal job backoff instead of silently dropping the mention.

## Interaction backfeed

Interactions with a webmention-materialized reply backfeed to its
source site, the way brid.gy does:

- **Replies** deliver `in-reply-to` mentions — the reply's `h-entry`
  carries `u-in-reply-to` to the parent's remote permalink (any remote
  parent, AP objects included).
- **Boosts** carry `u-repost-of` and **quotes** `u-quotation-of` to the
  boosted/quoted remote status's permalink.
- **Likes** on webmention replies deliver `like-of` from
  `GET /webmentions/like/{status_id}/{account_id}`, an `h-entry` page
  asserting the favourite (`u-like-of` + the liker's h-card).
  Unfavouriting returns `410` and the SDK retracts the delivered like.

Likes only backfeed to `urn:yonder:webmention:` statuses — remote
ActivityPub objects already receive the federated `Like` activity. All
backfeed respects visibility: only `public`/`unlisted` materialized
replies produce like pages.

## Configuration

```toml
[webmentions]
enabled = true                    # master switch (routes + jobs)
request_timeout_seconds = 10      # remote fetch timeout
discovery_max_bytes = 1048576     # endpoint-discovery body cap
source_max_bytes = 1048576        # incoming source-page body cap
allow_private_targets = false     # dev/test only — disables the SSRF guard

[rate_limit.webmention]
limit = 60
period_seconds = 300
```

The receiver rejects sources on `suspend`ed `domain_blocks` domains
(`403`) and targets not on this instance (`400`). Transient source
fetch failures retry with the normal job backoff.
