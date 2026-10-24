-- Epic 20, migration 0019: custom emoji packs.
--
-- `emoji_packs` groups `custom_emojis` for admin organization; the
-- existing `custom_emojis.category` stays the picker grouping. Packs
-- can exist empty, and deleting a pack unlinks rather than deletes
-- its emoji (ON DELETE SET NULL).

CREATE TABLE emoji_packs (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE CHECK (char_length(name) <= 100),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE custom_emojis
    ADD COLUMN pack_id BIGINT REFERENCES emoji_packs(id) ON DELETE SET NULL;

CREATE INDEX ix_custom_emojis_pack ON custom_emojis(pack_id);
