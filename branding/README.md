# Yonder branding assets

Original visual identity for Yonder. All artwork is original — no
trademarked or third-party material.

## Concept: "the network beyond the horizon"

A home instance (the large dot) rests on a shallow horizon arc; three thin
connections reach upward to equal-sized dots — the distant instances of the
fediverse. The distant nodes carry a lighter tint of the brand color, a
depth cue: the farther instances read fainter. The mark is purely
geometric and two-toned, so it stays legible at 16px and works unchanged
on light and dark backgrounds.

**Brand colors:** `#0d9488` (deep teal — home, horizon, connections) and
`#2dd4bf` (lighter teal — distant nodes). To recolor, replace those hex
values in `logo.svg`, `favicon.svg`, `maskable.svg` and
`src/banner-text.svg`, then re-run `scripts/generate_branding.sh`.

## Files

| File | Purpose |
|---|---|
| `logo.svg` | Source of truth for the mark. Named layers: `home`, `horizon`, `rays`, `nodes`. |
| `logo-{512,256,128,64}.png` | Raster renders of `logo.svg` (transparent background). |
| `logo-{48,32,16}.png` | Small sizes, rendered from `favicon.svg` — at 16–48px the horizon arc of the full mark merges with the home circle, so the re-weighted variant is used instead. |
| `favicon.svg` | The mark re-weighted for tiny sizes: horizon arc dropped, strokes and dots heavier. |
| `favicon-{16,32,48}.png` | Raster favicons. |
| `favicon.ico` | Multi-size ICO (16/32/48) for legacy browsers. |
| `banner.svg` | README header: mark + "yonder" wordmark, 1280×320. Wordmark converted to paths — renders identically without Poppins installed. |
| `banner.png` | Raster banner, embedded at the top of the repo `README.md`. |
| `maskable.svg` | Full-bleed brand background with the white mark inside the central 80% safe zone, for adaptive/icon masking. |
| `maskable-{192,512}.png` | PWA manifest icons. |
| `src/banner-text.svg` | Editable banner source with a live `<text>` wordmark (Poppins SemiBold). |

## Regenerating

```bash
scripts/generate_branding.sh
```

Requires `rsvg-convert`, `inkscape` and ImageMagick (`magick`). The script
is idempotent: re-running it reproduces byte-identical PNGs. The SVGs are
the sources of truth; never edit the generated PNGs by hand.

## Consumers

- **Default instance logo** (epic 22, admin/instance settings): serve
  `branding/logo.svg` (or a PNG size) when no instance logo is configured.
- **PWA manifest** (epic 23, frontend): icons `branding/maskable-192.png`
  and `branding/maskable-512.png` with `purpose: "maskable"`, plus
  `branding/favicon-32.png` / `branding/favicon.ico` for the document
  favicon and `branding/logo.svg` for in-app branding.
