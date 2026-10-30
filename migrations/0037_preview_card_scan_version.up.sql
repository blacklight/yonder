-- Card-shape version stamped on every `preview_cards` upsert —
-- pre-upgrade rows stay 0, which is how the `FetchLinkPreview`
-- sweep knows they still need one rescan to pick up embed metadata
-- (e.g. the video-embed fields added in 0036).
ALTER TABLE preview_cards
    ADD COLUMN scan_version INT NOT NULL DEFAULT 0;
