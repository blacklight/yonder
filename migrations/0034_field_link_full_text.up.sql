-- Local profile-field `value` is the AP `PropertyValue` wire shape.
-- Mastodon's remote-field verifier only fetches a field whose rendered
-- value is a single `<a>` with `href` equal to its own text
-- (`Account::Field#extract_url_from_html`); our anchors used the
-- scheme-less display text, so remote instances never attempted
-- verification. Re-render URL-valued entries with the full URL as the
-- link text — the same shape `render_verified_link(raw, Some(raw))`
-- now writes. Entries from before `value_raw` existed are covered by
-- extracting the href back out of the stored anchor.
UPDATE accounts a
SET profile_fields = f.fields,
    updated_at = now()
FROM (
    SELECT id, jsonb_agg(
        CASE
            WHEN raw ~ '^https?://[^[:space:]"''<>]+$'
            THEN jsonb_set(
                elem, '{value}',
                to_jsonb('<a href="' || esc || '" rel="me">' || esc || '</a>'))
            ELSE elem
        END
        ORDER BY ord) AS fields
    FROM (
        SELECT a2.id, ord, elem,
               COALESCE(
                   NULLIF(elem->>'value_raw', ''),
                   (regexp_match(elem->>'value', '^<a href="([^"]+)"'))[1]
               ) AS raw,
               replace(replace(replace(replace(
                   COALESCE(
                       NULLIF(elem->>'value_raw', ''),
                       (regexp_match(elem->>'value', '^<a href="([^"]+)"'))[1]
                   ),
                   '&', '&amp;'), '<', '&lt;'), '>', '&gt;'),
                   '"', '&quot;') AS esc
        FROM accounts a2,
             jsonb_array_elements(a2.profile_fields) WITH ORDINALITY AS t(elem, ord)
    ) s
    GROUP BY id
) f
WHERE a.id = f.id
  AND a.domain IS NULL
  AND a.profile_fields <> '[]'::jsonb;
