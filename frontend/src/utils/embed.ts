/**
 * Snippet builders for embedding a status on third-party pages. The
 * `<iframe>` variant points at the standalone `/statuses/{id}/embed`
 * page; the `<script>` variant drops a `.yonder-embed` placeholder that
 * `/embed.js` replaces with the same frame — plus automatic height
 * syncing via postMessage.
 */

/** Default pixel height for the no-JS `<iframe>` snippet. */
export const EMBED_IFRAME_HEIGHT = 400;

function origin(): string {
  return typeof window !== "undefined" ? window.location.origin : "";
}

/** URL of the embeddable status page (the iframe/script target). */
export function embedPageUrl(statusId: string): string {
  return `${origin()}/statuses/${encodeURIComponent(statusId)}/embed`;
}

/** URL of the static script that renders `.yonder-embed` placeholders. */
export function embedScriptUrl(): string {
  return `${origin()}/embed.js`;
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

/** The no-JS `<iframe>` embed served by the standalone embed route. */
export function iframeEmbed(statusId: string, title: string): string {
  return (
    `<iframe src="${escapeHtml(embedPageUrl(statusId))}" ` +
    `width="100%" height="${EMBED_IFRAME_HEIGHT}" frameborder="0" ` +
    `loading="lazy" title="${escapeHtml(title)}"></iframe>`
  );
}

/**
 * The `<script>` embed: a placeholder div rendered by `/embed.js` into
 * the same embeddable page as the `<iframe>` — but the frame's height
 * follows the post's content via postMessage, so nothing gets clipped.
 * The inner link is the no-JS fallback — embed.js clears it when it
 * takes over, and it doubles as the source-page backlink Webmention
 * receivers verify.
 */
export function scriptEmbed(statusId: string, title: string): string {
  const url = embedPageUrl(statusId);
  return (
    `<div class="yonder-embed" data-status="${escapeHtml(statusId)}">` +
    `<a href="${escapeHtml(url)}">${escapeHtml(title)}</a></div>\n` +
    `<script async src="${escapeHtml(embedScriptUrl())}" charset="utf-8"></script>`
  );
}
