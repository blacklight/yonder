#!/bin/sh
# LaTeX → PNG renderer for Yonder's [math] feature.
#
# Contract (see config.toml.example [math]):
#   argv:   <this script> [inline|display]   (Yonder appends the mode)
#   stdin:  LaTeX math source, delimiters stripped
#   stdout: PNG bytes
#   exit≠0 / timeout / empty stdout → Yonder leaves the formula as text.
#
# Backends, auto-detected in order:
#   1. tectonic  — single static binary, downloads its TeX bundle on
#      first run (needs network + a writable cache once).
#   2. pdflatex  — classic TeX Live (texlive-latex-base +
#      texlive-latex-recommended + fonts-lmodern).
# Either way the PDF is rasterized — prefer a transparency-capable
# backend so dark themes don't show a white box (Yonder's frontend
# approximates dark renders with a CSS invert filter):
#   MATH_PDF2PNG="gs -q -dSAFER -dBATCH -dNOPAUSE -sDEVICE=pngalpha -dTextAlphaBits=4 -dGraphicsAlphaBits=4 -r144 -o"
#     → ghostscript pngalpha (transparent background; install ghostscript;
#       -q keeps the GPL banner off stdout, and gs wants the resolution
#       attached: -r144, not "-r 144")
#   default: pdftoppm -png -r $MATH_DPI (poppler-utils; white page bg)
# `mutool draw -o out.png in.pdf` (mupdf-tools, alpha-capable) also works:
#   MATH_PDF2PNG="mutool draw -o"
#
# Tune with env vars:
#   MATH_TEX_ENGINE=tectonic|pdflatex   force a backend
#   MATH_DPI=144                        rasterization resolution (pdftoppm)
#
# Install (Debian/Ubuntu):
#   apt-get install poppler-utils tectonic        # or: texlive-latex-base texlive-latex-recommended
# Then in config.toml:
#   [math]
#   enabled = true
#   command = "/usr/local/bin/latex-math-render.sh"
set -eu

mode="${1:-inline}"
engine="${MATH_TEX_ENGINE:-auto}"
dpi="${MATH_DPI:-144}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# LaTeX source arrives on stdin.
cat > "$tmp/math.tex"

# Wrap in a minimal standalone document. `standalone` typesets its
# body in a box, where display math (`\[...\]`) is illegal — so both
# modes use `$...$`, with `\displaystyle` for `$$...$$` sources.
{
    printf '%s\n' \
        '\documentclass[12pt,border=2pt]{standalone}' \
        '\usepackage{amsmath,amssymb,amsthm}' \
        '\begin{document}'
    if [ "$mode" = "display" ]; then
        printf '%s\n' '$\displaystyle'
    else
        printf '%s\n' '$'
    fi
    cat "$tmp/math.tex"
    printf '%s\n' '$'
    printf '%s\n' '\end{document}'
} > "$tmp/doc.tex"

if [ "$engine" = "auto" ]; then
    if command -v tectonic >/dev/null 2>&1; then
        engine="tectonic"
    elif command -v pdflatex >/dev/null 2>&1; then
        engine="pdflatex"
    else
        echo "latex-math-render: no TeX engine found (install tectonic or texlive)" >&2
        exit 1
    fi
elif ! command -v "$engine" >/dev/null 2>&1; then
    echo "latex-math-render: $engine not found" >&2
    exit 1
fi

case "$engine" in
    tectonic)
        tectonic --outdir "$tmp" "$tmp/doc.tex" >/dev/null
        ;;
    pdflatex)
        (cd "$tmp" && pdflatex -interaction=nonstopmode -halt-on-error doc.tex >/dev/null 2>&1)
        ;;
esac
[ -f "$tmp/doc.pdf" ] || { echo "latex-math-render: no PDF produced" >&2; exit 1; }

if [ -n "${MATH_PDF2PNG:-}" ]; then
    # e.g. MATH_PDF2PNG="mutool draw -o"
    # shellcheck disable=SC2086
    $MATH_PDF2PNG "$tmp/out.png" "$tmp/doc.pdf"
    cat "$tmp/out.png"
else
    pdftoppm -f 1 -l 1 -png -r "$dpi" -singlefile "$tmp/doc.pdf" "$tmp/out"
    cat "$tmp/out.png"
fi
