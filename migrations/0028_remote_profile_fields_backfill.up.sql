-- Remote actor `attachment[]` `PropertyValue` entries were stored in
-- `accounts.actor_data` but never projected into `profile_fields`,
-- leaving every remote account fieldless. Rebuild the column in
-- place — no network I/O. Mirrors the extraction rules in
-- `activitypub::storage::remote_profile_fields`: any `PropertyValue`
-- spelling (plain, `schema:`-prefixed, the schema.org IRI) or a
-- missing/non-string `type` counts; non-field attachments are
-- dropped; names are trimmed; entries cap at 32; `verified_at` stays
-- null.
UPDATE accounts a
   SET profile_fields = f.fields,
       updated_at = now()
  FROM (
    SELECT acct.id,
           (SELECT jsonb_agg(
                     jsonb_build_object(
                       'name', left(btrim(elem->>'name'), 255),
                       'value', left(elem->>'value', 4096),
                       'verified_at', to_jsonb(NULL::text))
                     ORDER BY ord)
              FROM jsonb_array_elements(acct.actor_data->'attachment')
                   WITH ORDINALITY AS t(elem, ord)
             WHERE (elem->'type' IS NULL
                    OR jsonb_typeof(elem->'type') <> 'string'
                    OR lower(regexp_replace(elem->>'type', '^.*[#/:]', ''))
                       = 'propertyvalue')
               AND btrim(elem->>'name') <> ''
               AND elem->>'value' <> ''
             LIMIT 32) AS fields
      FROM accounts acct
     WHERE acct.domain IS NOT NULL
       AND jsonb_typeof(acct.actor_data->'attachment') = 'array'
  ) f
 WHERE a.id = f.id
   AND f.fields IS NOT NULL
   AND a.profile_fields IS DISTINCT FROM f.fields;
