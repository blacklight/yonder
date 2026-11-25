-- 0059_event_links — non-location Link attachments on events.
--
-- `parse_event` collects `attachment[]` entries of `type: Link` that
-- are not promoted to virtual locations (event homepage, registration
-- page, …). They are stored as a JSON array of `{name, href}` on the
-- `events` row and surfaced as `yonder.event.links`.

ALTER TABLE events ADD COLUMN links JSONB NOT NULL DEFAULT '[]';
