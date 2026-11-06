-- FEP-044f quote approval: the remote quoted author's QuoteAuthorization
-- URI accepted for this status. Emitted as `quoteAuthorization` so
-- Mastodon-family instances can leave the quote's pending state.
ALTER TABLE statuses ADD COLUMN quote_authorization_uri TEXT;
