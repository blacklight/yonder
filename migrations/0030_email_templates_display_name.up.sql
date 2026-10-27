-- The email template vars gain `{{user_display_name}}` — the account's
-- display name, falling back to the username — alongside `{{user_handle}}`
-- (still the bare username). Rewrite stored templates' greeting
-- placeholder so existing installs address recipients by display name
-- instead of the raw handle; templates that want the handle can keep or
-- re-add `{{user_handle}}`.
UPDATE email_templates
SET subject    = replace(subject, '{{user_handle}}', '{{user_display_name}}'),
    text_body  = replace(text_body, '{{user_handle}}', '{{user_display_name}}'),
    html_body  = replace(html_body, '{{user_handle}}', '{{user_display_name}}'),
    updated_at = now()
WHERE subject   LIKE '%{{user_handle}}%'
   OR text_body LIKE '%{{user_handle}}%'
   OR html_body LIKE '%{{user_handle}}%';
