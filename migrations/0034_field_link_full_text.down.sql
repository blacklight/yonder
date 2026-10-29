-- Restore the scheme-less link text for URL-valued local fields
-- (`render_verified_link(raw, None)`'s `display_url` text: strip the
-- scheme and a lone trailing slash).
UPDATE accounts a
SET profile_fields = f.fields
FROM (
    SELECT id, jsonb_agg(
        CASE
            WHEN raw ~ '^https?://[^[:space:]"''<>]+$'
            THEN jsonb_set(
                elem, '{value}',
                to_jsonb('<a href="' || esc || '" rel="me">' || label || '</a>'))
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
                   '"', '&quot;') AS esc,
               replace(replace(replace(replace(
                   regexp_replace(
                       split_part(
                           COALESCE(
                               NULLIF(elem->>'value_raw', ''),
                               (regexp_match(elem->>'value', '^<a href="([^"]+)"'))[1]
                           ),
                           '://', 2),
                       '^([^/]*)/$', '\1'),
                   '&', '&amp;'), '<', '&lt;'), '>', '&gt;'),
                   '"', '&quot;') AS label
        FROM accounts a2,
             jsonb_array_elements(a2.profile_fields) WITH ORDINALITY AS t(elem, ord)
    ) s
    GROUP BY id
) f
WHERE a.id = f.id
  AND a.domain IS NULL
  AND a.profile_fields <> '[]'::jsonb;
