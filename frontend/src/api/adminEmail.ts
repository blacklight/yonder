import { request } from "./client";

export interface EmailTemplate {
  kind: string;
  subject: string;
  text_body: string;
  html_body: string | null;
}

/** Server-restricted set (epic 22 `email::EDITABLE_KINDS`). */
export const EMAIL_TEMPLATE_KINDS = [
  "verify_email",
  "password_reset",
  "welcome",
  "account_status",
] as const;
export type EmailTemplateKind = (typeof EMAIL_TEMPLATE_KINDS)[number];

/** `GET /api/v1/yonder/admin/email_templates/{kind}`. */
export function getEmailTemplate(kind: string): Promise<EmailTemplate> {
  return request({
    path: `/yonder/admin/email_templates/${encodeURIComponent(kind)}`,
  });
}

/** `PUT …/{kind}` — upsert; audited as `email_template.update`. */
export function putEmailTemplate(
  kind: string,
  body: { subject: string; text_body: string; html_body: string | null },
): Promise<EmailTemplate> {
  return request({
    path: `/yonder/admin/email_templates/${encodeURIComponent(kind)}`,
    method: "PUT",
    body,
  });
}

/**
 * `POST /api/v1/yonder/admin/email/test` — renders the `welcome`
 * template through the SMTP transport inline. `to` defaults to the
 * calling admin's email server-side.
 */
export function sendTestEmail(to?: string): Promise<{ to?: string }> {
  return request({
    path: "/yonder/admin/email/test",
    method: "POST",
    body: { to },
  });
}
