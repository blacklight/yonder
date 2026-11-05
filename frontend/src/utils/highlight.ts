import hljs from "highlight.js/lib/common";

/** `language-x` marker on a `code` element — pulldown-cmark's fenced
 * block output and the convention most fediverse software follows. */
const LANG_CLASS = /(?:^|\s)language-([\w+#.-]+)/;

/**
 * Apply syntax highlighting to fenced code blocks (`pre > code`) in
 * sanitized post HTML. Runs *after* [`sanitizeHtml`] — the `language-*`
 * class is the only markup it needs, and everything it emits is
 * highlight.js's own escaped span markup.
 *
 * A known `language-*` class highlights directly; anything else falls
 * back to `highlightAuto`. Returns the input untouched when there is
 * no code to highlight.
 */
export function highlightCodeBlocks(html: string): string {
  if (!html.includes("<pre")) return html;
  const doc = new DOMParser().parseFromString(html, "text/html");
  let changed = false;
  for (const code of doc.querySelectorAll<HTMLElement>("pre code")) {
    const text = code.textContent ?? "";
    if (!text.trim()) continue;
    const lang = code.getAttribute("class")?.match(LANG_CLASS)?.[1];
    let result: { value: string; language?: string };
    try {
      result =
        lang && hljs.getLanguage(lang)
          ? hljs.highlight(text, { language: lang, ignoreIllegals: true })
          : hljs.highlightAuto(text);
    } catch {
      continue;
    }
    code.innerHTML = result.value;
    // Re-assert the marker classes highlight.js expects — the source
    // may have carried only `language-x` (or nothing).
    code.className = `hljs${lang ? ` language-${lang}` : ""}`;
    changed = true;
  }
  return changed ? doc.body.innerHTML : html;
}
