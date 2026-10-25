import DOMPurify from "dompurify";

let hooked = false;

/** Anchors get a fixed rel/target — `target="_self"` et al. are overwritten. */
function ensureHook() {
  if (hooked) return;
  hooked = true;
  DOMPurify.addHook("afterSanitizeAttributes", (node) => {
    if (node.tagName === "A") {
      node.setAttribute("rel", "nofollow noopener");
      node.setAttribute("target", "_blank");
    }
  });
}

/**
 * Sanitize remote-rendered HTML (status content, bios, field values).
 * Mandatory for remote bios — stored raw, never trusted.
 */
export function sanitizeHtml(html: string): string {
  ensureHook();
  return DOMPurify.sanitize(html, {
    FORBID_TAGS: ["style", "form", "input", "button", "select", "textarea"],
    FORBID_CONTENTS: ["style"],
    FORBID_ATTR: ["class", "title"],
  });
}

/** Only explicit http(s) URLs may become `href` values. */
export function safeHttpUrl(url: string | null | undefined): string | null {
  if (!url) return null;
  try {
    const u = new URL(url);
    return u.protocol === "http:" || u.protocol === "https:" ? u.href : null;
  } catch {
    return null;
  }
}
