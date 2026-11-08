-- LaTeX formula rendering: marks media_attachments rows that were
-- generated from `$…$`/`$$…$$`/`\(…\)`/`\[…\]` delimiters in post
-- text. The value is `{mode}:{latex}` (mode = inline|display) — the
-- reuse key that keeps re-edits and remote Updates from regenerating
-- identical formula images.
ALTER TABLE media_attachments ADD COLUMN math_source TEXT;
