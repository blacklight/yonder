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
 * acct/tag-name mapping for the anchors they describe. */
export interface RichLinkEntities {
  mentions?: Pick<StatusMention, "acct" | "url">[];
  tags?: Pick<StatusTag, "name" | "url">[];
}

// NOTE: server-side sanitizing strips every attribute but `href` from
// inbound remote HTML, so `class="mention"` / `rel="tag"` markers are
// usually gone by render time. Detection therefore leans on the
// status entities and the anchor's own `@name`/`#tag` link text —
// markers are still checked (cheap) for content that keeps them.
const MENTION_TEXT = /^@([\p{L}\p{N}._'-]+)(?:@([\w.%:-]+))?$/u;
const TAG_TEXT = /^#([^\s#]+)$/u;

function ciEq(a: string, b: string): boolean {
  return a.localeCompare(b, undefined, { sensitivity: "base" }) === 0;
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

function lastPathSegment(url: URL): string {
  const seg = url.pathname.replace(/\/+$/, "").split("/").pop() ?? "";
  try {
    return decodeURIComponent(seg);
  } catch {
    return seg;
  }
}

/** A status `mentions[]` row the anchor points to — by entity URL, or
 * by the `@name`/`@name@dom` link text naming that account. */
function matchMention(
  mentions: RichLinkEntities["mentions"],
  a: Element,
  url: URL,
  href: string,
) {
  if (!mentions) return null;
  const text = (a.textContent ?? "").trim();
  const handle = text.match(MENTION_TEXT);
  const base = lastPathSegment(url);
  return (
    mentions.find((m) => {
      if (sameRemoteUrl(m.url, href)) return true;
      const [uname, domain] = m.acct.split("@");
      if (handle) {
        if (!ciEq(uname, handle[1])) return false;
        // `@name@dom` pins the acct; bare `@name` needs the link host
        // or a unique-username match to pick the right domain.
        if (handle[2]) return ciEq(m.acct, `${handle[1]}@${handle[2]}`);
        if (domain && ciEq(domain, url.hostname)) return true;
        const sameName = mentions.filter((x) =>
          ciEq(x.acct.split("@")[0], handle[1]),
        );
        return sameName.length === 1;
      }
      // Path basename == username on the account's own domain
      // (`{host}/@name`, `…/users/name`) — a strong pin even when the
      // link text is arbitrary.
      return ciEq(uname, base) && !!domain && ciEq(domain, url.hostname);
    }) ?? null
  );
}

/** A status `tags[]` row the anchor points to — by entity URL, by the
 * `#name` link text, or a tag-shaped path/query naming the tag. */
function matchTag(
  tags: RichLinkEntities["tags"],
  a: Element,
  url: URL,
  href: string,
) {
  if (!tags) return null;
  const text = (a.textContent ?? "").trim();
  const pathMatch = url.pathname.match(/\/(?:tags|tag)\/([^/?#]+)/i);
  const slug = pathMatch
    ? lastPathSegment(url)
    : (url.searchParams.get("tag") ?? "");
  return (
    tags.find(
      (t) =>
        sameRemoteUrl(t.url, href) ||
        text === `#${t.name}` ||
        (!!slug && ciEq(t.name, slug)),
    ) ?? null
  );
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
  const text = (a.textContent ?? "").trim();
  const rel = (a.getAttribute("rel") ?? "").split(/\s+/);
  const cls = (a.getAttribute("class") ?? "").split(/\s+/);
  // Entity matches are authoritative — they carry the canonical local
  // acct/tag name. Mentions before tags: a `/users/rust` link to user
  // "rust" must not be hijacked by a "rust" hashtag.
  const mention = matchMention(links?.mentions, a, url, href);
  if (mention) return `/@${mention.acct}`;
  const tag = matchTag(links?.tags, a, url, href);
  if (tag) return `/tags/${encodeURIComponent(tag.name)}`;
  // Text heuristics cover markerless markup and posts without
  // entities: an `@handle`/`#tag` link label *is* the reference.
  const handle = text.match(MENTION_TEXT);
  if (handle)
    return `/@${handle[2] ? `${handle[1]}@${handle[2]}` : `${handle[1]}@${url.hostname}`}`;
  const hash = text.match(TAG_TEXT);
  if (hash) return `/tags/${encodeURIComponent(hash[1])}`;
  // Marker-carrying anchors (when attributes survive) — hashtag first,
  // as Mastodon's tag links carry `class="mention hashtag"`.
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
