#!/usr/bin/env python3
"""
Regenerate the table of contents of Markdown files.

Two marker styles are supported for every file passed on the command line:

- A managed region delimited by ``<!-- toc -->`` and ``<!-- tocstop -->``
  is regenerated in place.
- A standalone ``[TOC]`` or ``[[_TOC_]]`` marker line is replaced by a
  managed ``<!-- toc -->``/``<!-- tocstop -->`` region containing the TOC
  (the marker is consumed on the first run; the region is idempotent
  thereafter).

The table of contents is generated from the file's ATX headings, skipping
fenced code blocks and the first H1 (the document title).

Heading anchors are slugified the way Forgejo/Gitea render them (emoji and
punctuation dropped, whitespace collapsed to single dashes), so the links
work natively on the canonical repository host. GitHub slugifies some
headings differently (leading dashes for emoji, double dashes around
``&``, retained variation selectors); headings whose slug cannot be
generated identically by every renderer should therefore carry an explicit
``<a name="slug"></a>`` anchor, which survives sanitization on both forges.
"""

import re
import sys

TOC_START = "<!-- toc -->"
TOC_END = "<!-- tocstop -->"

_HEADING_RE = re.compile(r"^(#{1,6})\s+(.+?)\s*#*$")
_FENCE_RE = re.compile(r"^\s*(`{3,}|~{3,})")
_MARKER_LINE_RE = re.compile(r"^\s*(\[TOC\]|\[\[_TOC_\]\])\s*$")
_HTML_TAG_RE = re.compile(r"<[^>]+>")
_NON_WORD_RE = re.compile(r"[^\w\s-]")
_SEPARATORS_RE = re.compile(r"[\s-]+")
# Emoji modifiers (ZWJ, variation selectors) that Python's ``\w`` class
# keeps but forges drop when slugifying headings.
_MODIFIERS_RE = re.compile(r"[\u200d\ufe0e\ufe0f]")

_BULLETS = ("-", "*", "+")


def slugify(title: str) -> str:
    """Return the anchor slug for a heading title, Forgejo-style."""
    title = _HTML_TAG_RE.sub("", title)
    title = _MODIFIERS_RE.sub("", title.lower())
    title = _NON_WORD_RE.sub("", title)
    return _SEPARATORS_RE.sub("-", title).strip("-")


def _label(title: str) -> str:
    return _HTML_TAG_RE.sub("", title).strip()


def iter_headings(text: str):
    """
    Yield ``(line_number, level, raw_title)`` for every ATX heading,
    skipping fenced code blocks.
    """
    in_fence = False
    fence_marker = ""
    for lineno, line in enumerate(text.splitlines()):
        fence = _FENCE_RE.match(line)
        if fence:
            marker = fence.group(1)[0]
            if in_fence and marker == fence_marker:
                in_fence = False
            elif not in_fence:
                in_fence = True
                fence_marker = marker
            continue
        if in_fence:
            continue

        match = _HEADING_RE.match(line)
        if match:
            yield lineno, len(match.group(1)), match.group(2)


def headings(text: str) -> list[tuple[int, str, str]]:
    """
    Collect ``(level, label, slug)`` for ATX headings, skipping the first
    h1 (the document title).
    """
    result = []
    seen: dict[str, int] = {}
    first_h1_skipped = False

    for _, level, title in iter_headings(text):
        if level == 1 and not first_h1_skipped:
            first_h1_skipped = True
            continue

        slug = slugify(title)
        count = seen.get(slug, 0)
        seen[slug] = count + 1
        if count:
            slug = f"{slug}-{count}"
        result.append((level, _label(title), slug))

    return result


def build_toc(text: str) -> str:
    """Build the TOC list (without markers) for the given document."""
    lines = []
    for level, label, slug in headings(text):
        indent = "  " * (level - 2)
        bullet = _BULLETS[(level - 2) % len(_BULLETS)]
        lines.append(f"{indent}{bullet} [{label}](#{slug})")
    return "\n".join(lines)


def _trailing_newline(text: str) -> str:
    return "\n" if text.endswith("\n") else ""


def update_text(text: str) -> str:
    """
    Return the document with its TOC refreshed or inserted.

    A ``<!-- toc -->``/``<!-- tocstop -->`` region is regenerated in place;
    a standalone ``[TOC]``/``[[_TOC_]]`` line is replaced by a managed
    region. Documents with neither are returned unchanged.
    """
    lines = text.splitlines()

    start = next((i for i, line in enumerate(lines) if TOC_START in line), None)
    if start is not None:
        end = next(
            (i for i in range(start + 1, len(lines)) if TOC_END in lines[i]),
            None,
        )
        if end is None:
            return text
        toc = build_toc(text)
        lines[start + 1 : end] = ["", *toc.splitlines(), ""]
        return "\n".join(lines) + _trailing_newline(text)

    marker = next(
        (i for i, line in enumerate(lines) if _MARKER_LINE_RE.match(line)),
        None,
    )
    if marker is None:
        return text
    toc = build_toc(text)
    lines[marker : marker + 1] = [
        TOC_START,
        "",
        *toc.splitlines(),
        "",
        TOC_END,
    ]
    return "\n".join(lines) + _trailing_newline(text)


def main(argv: list[str]) -> int:
    ret = 0
    for name in argv:
        with open(name, encoding="utf-8") as f:
            original = f.read()
        updated = update_text(original)
        if updated != original:
            with open(name, "w", encoding="utf-8") as f:
                f.write(updated)
            print(f"updated table of contents in {name}")
    return ret


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
