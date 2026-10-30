-- Embeddable video players for link previews: `twitter:player` /
-- `og:video` (text/html) metadata from the fetched page. Presence of
-- `embed_url` upgrades the card entity to `type: "video"` — the
-- frontend renders a click-to-play iframe instead of a bare link
-- card (YouTube, Vimeo, …). Width/height are the declared player
-- dimensions (0 = unknown, renders 16:9).
ALTER TABLE preview_cards
    ADD COLUMN embed_url    TEXT NOT NULL DEFAULT '',
    ADD COLUMN embed_width  INT  NOT NULL DEFAULT 0,
    ADD COLUMN embed_height INT  NOT NULL DEFAULT 0;
