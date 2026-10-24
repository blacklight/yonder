-- Epic 20, migration 0019: drop emoji packs.

DROP INDEX IF EXISTS ix_custom_emojis_pack;
ALTER TABLE custom_emojis DROP COLUMN IF EXISTS pack_id;
DROP TABLE IF EXISTS emoji_packs;
