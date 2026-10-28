-- 0032_media_attachments_preview_remote_url — poster art for remote
-- media. Remote Audio/Video objects carry cover art in `image`/
-- `icon`/`preview` (or on the `attachment` itself); storing the URL
-- lets the API emit a real `preview_url` so clients can render
-- Mastodon-style players instead of a bare element.
ALTER TABLE media_attachments ADD COLUMN preview_remote_url text;
