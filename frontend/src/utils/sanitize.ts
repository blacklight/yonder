import DOMPurify from "dompurify";

// Client-side re-sanitization of backend-rendered status HTML.
// Mirrors the server policy (`src/posts/render.rs`): ammonia's
// default tag set, anchors keep only `href` (+ forced
// `rel="nofollow noopener"`/`target="_blank"`), URL schemes are
// http/https/mailto + same-document/relative links.
//
// DOMPurify's default element set already approximates ammonia's
// (it strips scripts, handlers, forms); the anchor hook enforces the
// href-only attribute policy and the link decorations.
const URI_RE = /^(?:https?:\/\/|mailto:|\/|#)/i;

let hooked = false;
function ensureHooks() {
  if (hooked) return;
  hooked = true;
  DOMPurify.addHook("afterSanitizeAttributes", (node) => {
    if (!(node instanceof Element) || node.tagName !== "A") return;
    const href = node.getAttribute("href");
    for (const attr of Array.from(node.attributes)) {
      node.removeAttribute(attr.name);
    }
    if (href) node.setAttribute("href", href);
    node.setAttribute("rel", "nofollow noopener");
    node.setAttribute("target", "_blank");
  });
}

/** Sanitize a backend-rendered status HTML string for `v-html`. */
export function sanitizeHtml(html: string): string {
  ensureHooks();
  return DOMPurify.sanitize(html, {
    USE_PROFILES: { html: true },
    ALLOWED_URI_REGEXP: URI_RE,
    // Mirror ammonia's exclusions explicitly — forms and interactive
    // elements never survive the backend renderer either.
    FORBID_TAGS: [
      "style",
      "form",
      "input",
      "button",
      "select",
      "option",
      "textarea",
      "label",
      "fieldset",
    ],
  });
}
