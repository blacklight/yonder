-- 0064_event_spoiler_cleanup — drop machine-generated CWs on events.
--
-- Before the ingest fix, a remote event's AS2 `summary` — the
-- when/where/description preamble Yonder and Gancio emit for
-- Mastodon-style clients — was folded into `spoiler_text`, so the
-- whole event card rendered collapsed behind an unreadable content
-- warning. Events never carry genuine CWs (authoring sets none), so
-- any `spoiler_text` identical to the stored `summary` provably came
-- from that ingest path and is cleared. The equality guard makes the
-- cleanup a no-op for anything a remote author may have written by
-- hand.

UPDATE statuses
SET spoiler_text = NULL
WHERE object_type = 'Event'
  AND spoiler_text IS NOT NULL
  AND summary IS NOT NULL
  AND spoiler_text = summary;
