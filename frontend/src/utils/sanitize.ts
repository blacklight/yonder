import DOMPurify from "dompurify";
import type { StatusMention, StatusTag } from "@/api/entities";

let hooked = false;

/** Anchors get a fixed rel/target — `target="_self"` et al. are overwritten. */
function ensureHook() {
  if (hooked) return;
  hooked = true;
  DOMPurify.addHook("afterSanitizeAttributes", (node) => {
    if (node.tagName === "A") {
      // Internal links stay in-app and same-tab; everything else is
      // remote content that opens in a new tab.
      if ((node.getAttribute("href") ?? "").startsWith("/")) return;
      node.setAttribute("rel", "nofollow noopener");
      node.setAttribute("target", "_blank");
    }
  });
}

/** Mention/tag entities from the status entity — the authoritative
 * acct/tag-name mapping when the anchor's href matches the entity URL. */
export interface RichLinkEntities {
  mentions?: Pick<StatusMention, "acct" | "url">[];
  tags?: Pick<StatusTag, "name" | "url">[];
}

function sameRemoteUrl(a: string, b: string): boolean {
  try {
    const x = new URL(a);
    const y = new URL(b);
    return (
      x.host === y.host &&
      x.pathname.replace(/\/+$/, "") === y.pathname.replace(/\/+$/, "")
    );
  } catch {
    return a === b;
  }
}

/** `#tag` name for a marker-carrying anchor: `/tags/<name>` path
 * (Mastodon's shape, and what remote software overwhelmingly uses),
 * else the `#name` link text. */
function tagNameOf(a: Element, url: URL): string | null {
  const m = url.pathname.match(/\/(?:tags|tag)\/([^/?#]+)/i);
  if (m) return decodeURIComponent(m[1]);
  const text = a.textContent?.trim() ?? "";
  return text.startsWith("#") ? text.slice(1) : null;
}

/** `acct` for a `class="mention"` anchor without an entity match:
 * `/@name`, `/@name@dom`, `/users/name` / `/profile/name` /
 * `/accounts/name` URL shapes, else `@name@dom` link text. */
function acctOf(a: Element, url: URL): string | null {
  const at = url.pathname.match(/^\/@([\w.%]+)(?:@([\w.%:-]+))?\/?$/);
  if (at) {
    const name = decodeURIComponent(at[1]);
    return at[2] ? `${name}@${at[2]}` : `${name}@${url.hostname}`;
  }
  const actor = url.pathname.match(
    /^\/(?:users|profile|accounts)\/([\w.%@-]+)\/?$/i,
  );
  if (actor) return `${decodeURIComponent(actor[1])}@${url.hostname}`;
  const text = (a.textContent ?? "").trim().match(/^@?([\w.%-]+)@([\w.-]+)$/);
  return text ? `${text[1]}@${text[2]}` : null;
}

/** Local route for a content anchor, or null to leave it external. */
function localizeAnchor(
  a: Element,
  links: RichLinkEntities | undefined,
): string | null {
  const href = a.getAttribute("href") ?? "";
  // Only absolute http(s) links get rewritten — existing `/` paths are
  // already local, and mailto/fragment/relative links stay untouched.
  let url: URL;
  try {
    url = new URL(href);
  } catch {
    return null;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return null;
  // Same-origin links always collapse to SPA paths — covers local
  // mentions (`{base}/@name`), tags and status permalinks.
  if (url.origin === location.origin)
    return url.pathname + url.search + url.hash;
  const tag = links?.tags?.find((t) => sameRemoteUrl(t.url, href));
  if (tag) return `/tags/${encodeURIComponent(tag.name)}`;
  const mention = links?.mentions?.find((m) => sameRemoteUrl(m.url, href));
  if (mention) return `/@${mention.acct}`;
  const rel = (a.getAttribute("rel") ?? "").split(/\s+/);
  const cls = (a.getAttribute("class") ?? "").split(/\s+/);
  // Tag anchors first — Mastodon's hashtag links carry BOTH classes
  // (`class="mention hashtag"` + `rel="tag"`).
  if (rel.includes("tag") || cls.includes("hashtag")) {
    const name = tagNameOf(a, url);
    if (name) return `/tags/${encodeURIComponent(name)}`;
  }
  if (cls.includes("mention")) {
    const acct = acctOf(a, url);
    if (acct) return `/@${acct}`;
  }
  return null;
}

/**
 * Rewrite remote-content links to local routes before sanitizing:
 * mention anchors → `/@acct`, hashtag anchors → `/tags/<name>`,
 * same-origin links → their path. Runs pre-sanitize because `class`
 * and `rel` — the mention/hashtag markers — are stripped by the
 * allowlist. Internal links keep same-tab navigation (the SPA router
 * intercepts them at click time).
 */
function localizeAnchors(html: string, links?: RichLinkEntities): string {
  if (!html.includes("<a")) return html;
  const doc = new DOMParser().parseFromString(html, "text/html");
  let changed = false;
  for (const a of doc.querySelectorAll("a[href]")) {
    // Already-local paths stay same-tab — drop any remote new-tab
    // attributes so they behave like the links we rewrite.
    if ((a.getAttribute("href") ?? "").startsWith("/")) {
      if (a.hasAttribute("target") || a.hasAttribute("rel")) {
        a.removeAttribute("target");
        a.removeAttribute("rel");
        changed = true;
      }
      continue;
    }
    const local = localizeAnchor(a, links);
    if (local) {
      a.setAttribute("href", local);
      a.removeAttribute("target");
      a.removeAttribute("rel");
      changed = true;
    }
  }
  return changed ? doc.body.innerHTML : html;
}

/**
 * Sanitize remote-rendered HTML (status content, bios, field values).
 * Mandatory for remote bios — stored raw, never trusted. When the
 * status entity's `mentions`/`tags` are passed, their URLs give the
 * authoritative local route for those anchors.
 */
export function sanitizeHtml(html: string, links?: RichLinkEntities): string {
  ensureHook();
  return DOMPurify.sanitize(localizeAnchors(html, links), {
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
