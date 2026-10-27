UPDATE email_templates
SET subject    = replace(subject, '{{user_display_name}}', '{{user_handle}}'),
    text_body  = replace(text_body, '{{user_display_name}}', '{{user_handle}}'),
    html_body  = replace(html_body, '{{user_display_name}}', '{{user_handle}}'),
    updated_at = now()
WHERE subject   LIKE '%{{user_display_name}}%'
   OR text_body LIKE '%{{user_display_name}}%'
   OR html_body LIKE '%{{user_display_name}}%';
