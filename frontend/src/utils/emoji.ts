import type { CustomEmoji } from "@/api/entities";

/** Everything emoji substitution needs from a `CustomEmoji` entity. */
export type EmojiRef = Pick<CustomEmoji, "shortcode" | "url">;

/** One slice of emoji-expanded text. */
export type EmojiPart =
  { text: string; emoji?: undefined } | { text?: undefined; emoji: EmojiRef };

// Mirrors the backend's candidate rule (`emoji::render`): an ASCII
// colon, a 1..=100 `[A-Za-z0-9_]` run, then a closing colon.
const SHORTCODE = /^[A-Za-z0-9_]{1,100}$/;

/** Lowercase shortcode → emoji (lookup is case-insensitive, matching
 * the backend map). */
function emojiMap(emojis: EmojiRef[] | undefined): Map<string, EmojiRef> {
  const map = new Map<string, EmojiRef>();
  for (const e of emojis ?? []) map.set(e.shortcode.toLowerCase(), e);
  return map;
}

/**
 * Split `text` into plain-text runs and emoji parts for every
 * `:shortcode:` found in `emojis`. Unknown or malformed shortcodes
 * stay verbatim — the same contract the backend's `substitute` uses
 * for HTML, applied here to plain-text fields (`display_name`,
 * `spoiler_text`) that Mastodon expects clients to expand from the
 * `emojis` array.
 */
export function emojiParts(
  text: string,
  emojis: EmojiRef[] | undefined,
): EmojiPart[] {
  const map = emojiMap(emojis);
  if (map.size === 0 || !text.includes(":")) return [{ text }];
  const parts: EmojiPart[] = [];
  let literal = "";
  let pos = 0;
  while (pos < text.length) {
    if (text[pos] === ":") {
      const close = text.indexOf(":", pos + 1);
      if (close > pos + 1) {
        const code = text.slice(pos + 1, close);
        const emoji = SHORTCODE.test(code) ? map.get(code.toLowerCase()) : null;
        if (emoji) {
          if (literal) parts.push({ text: literal });
          literal = "";
          parts.push({ emoji });
          pos = close + 1;
          continue;
        }
        if (!SHORTCODE.test(code)) {
          // Not a candidate — don't let this colon swallow a later
          // `:code:`: emit the colon and keep scanning.
          literal += ":";
          pos += 1;
          continue;
        }
      }
    }
    literal += text[pos];
    pos += 1;
  }
  if (literal) parts.push({ text: literal });
  return parts;
}

/** Elements whose subtrees never get emoji substitution — mirrors
 * the backend's `SKIP_TAGS` so links and code stay literal. */
const SKIP_TAGS = new Set(["a", "code", "pre", "script", "style"]);

/**
 * Expand `:shortcode:` in an HTML string's text runs into
 * `<img class="emoji">` elements. Used for HTML that arrives without
 * backend substitution — notably translated status content, whose
 * markup comes from the translate API with literal shortcodes — and
 * harmless elsewhere (backend-substituted HTML has no surviving
 * `:code:` outside skip zones).
 */
export function emojifyHtml(
  html: string,
  emojis: EmojiRef[] | undefined,
): string {
  const map = emojiMap(emojis);
  if (map.size === 0 || !html.includes(":")) return html;
  const doc = new DOMParser().parseFromString(html, "text/html");
  const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT, {
    acceptNode(node) {
      if (!node.nodeValue?.includes(":")) return NodeFilter.FILTER_REJECT;
      for (let el = node.parentElement; el; el = el.parentElement) {
        if (SKIP_TAGS.has(el.tagName.toLowerCase()))
          return NodeFilter.FILTER_REJECT;
      }
      return NodeFilter.FILTER_ACCEPT;
    },
  });
  const targets: Text[] = [];
  while (walker.nextNode()) targets.push(walker.currentNode as Text);
  let changed = false;
  for (const node of targets) {
    const parts = emojiParts(node.nodeValue ?? "", emojis);
    if (parts.length === 1 && parts[0].text !== undefined) continue;
    const frag = doc.createDocumentFragment();
    for (const part of parts) {
      if (part.emoji) {
        const img = doc.createElement("img");
        img.className = "emoji";
        img.src = part.emoji.url;
        img.alt = `:${part.emoji.shortcode}:`;
        img.title = img.alt;
        img.setAttribute("draggable", "false");
        frag.appendChild(img);
      } else if (part.text) {
        frag.appendChild(doc.createTextNode(part.text));
      }
    }
    node.replaceWith(frag);
    changed = true;
  }
  return changed ? doc.body.innerHTML : html;
}
