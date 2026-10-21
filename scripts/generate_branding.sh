#!/bin/sh
# Regenerate every raster/derived asset under branding/ from the SVG sources.
#
# Sources of truth (hand-authored, committed):
#   branding/logo.svg               — the mark, named layers, one brand color
#   branding/favicon.svg            — mark re-weighted for tiny sizes
#   branding/maskable.svg           — full-bleed PWA icon (80% safe zone)
#   branding/src/banner-text.svg    — banner with a live <text> wordmark
#
# Generated (committed, reproducible):
#   branding/logo-<size>.png, favicon-<size>.png, favicon.ico,
#   banner.svg (wordmark converted to paths), banner.png,
#   maskable-<size>.png
#
# Requires: rsvg-convert, inkscape, magick (ImageMagick 7).

set -eu

cd "$(dirname "$0")/.."
branding=branding

command -v rsvg-convert >/dev/null || { echo "rsvg-convert not found" >&2; exit 1; }
command -v inkscape >/dev/null || { echo "inkscape not found" >&2; exit 1; }
command -v magick >/dev/null || { echo "magick not found" >&2; exit 1; }

# Banner: convert the wordmark to paths so the shipped SVG is
# font-independent. Inkscape rewrites the file; keep the authored text
# version untouched in branding/src/.
inkscape "$branding/src/banner-text.svg" \
    --export-text-to-path \
    --export-plain-svg \
    --export-filename="$branding/banner.svg"

# Logo and favicon PNGs (transparent background). Small logo sizes use the
# re-weighted favicon variant: at 16-48px the horizon arc of the full logo
# merges with the home circle, while the heavier favicon mark stays legible.
for size in 16 32 48; do
    rsvg-convert -w "$size" -h "$size" "$branding/favicon.svg" \
        -o "$branding/logo-$size.png"
done
for size in 512 256 128 64; do
    rsvg-convert -w "$size" -h "$size" "$branding/logo.svg" \
        -o "$branding/logo-$size.png"
done
for size in 16 32 48; do
    rsvg-convert -w "$size" -h "$size" "$branding/favicon.svg" \
        -o "$branding/favicon-$size.png"
done

# Multi-size favicon.ico.
magick "$branding/favicon-16.png" "$branding/favicon-32.png" \
    "$branding/favicon-48.png" "$branding/favicon.ico"

# Banner PNG.
rsvg-convert -w 1280 -h 320 "$branding/banner.svg" -o "$branding/banner.png"

# Maskable PWA icons (full-bleed background baked in by the SVG itself).
for size in 192 512; do
    rsvg-convert -w "$size" -h "$size" "$branding/maskable.svg" \
        -o "$branding/maskable-$size.png"
done

echo "branding assets regenerated:"
ls -1 "$branding"/*.png "$branding"/*.ico "$branding/banner.svg"
