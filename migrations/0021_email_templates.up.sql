-- Epic 22, migration 0021: admin-editable email templates.
--
-- `kind` names a mail class. The four seeded kinds are the ones the
-- BOOTSTRAP admin panel exposes for editing; `EmailSend` jobs whose
-- payload has no inline `text_body` render the template matching their
-- `kind`. Bodies use `{{var}}` placeholders — see
-- `src/admin/email.rs::render` for the substitution contract.

CREATE TABLE email_templates (
    kind       TEXT PRIMARY KEY,
    subject    TEXT NOT NULL,
    text_body  TEXT NOT NULL,
    html_body  TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO email_templates (kind, subject, text_body) VALUES
    ('verify_email',
     'Verify your email address for {{instance_name}}',
     'Hello {{user_handle}},

Confirm your email address for {{instance_name}} by opening this link:

{{url}}

If you did not request this, you can ignore this message.'),
    ('password_reset',
     'Reset your {{instance_name}} password',
     'Hello {{user_handle}},

Someone requested a password reset for your account. Open this link to choose a new password:

{{url}}

If you did not request this, you can ignore this message.'),
    ('welcome',
     'Welcome to {{instance_name}}',
     'Hello {{user_handle}},

Welcome to {{instance_name}}! Your account is ready.

You can now complete your profile, follow others and start posting.'),
    ('account_status',
     'Your {{instance_name}} account status has changed',
     'Hello {{user_handle}},

A moderation action was applied to your account: {{status}}

{{reason}}');
