# CHANGELOG

## 1.0.4

### Added

- `markdown`: Render inline `![alt](url)` images in posts and federate them
  as ActivityPub image attachments
  ([`599cda5`](https://git.platypush.tech/blacklight/yonder/commit/599cda5e79feda652fb69d7ae2fdb213551bd4b7)).
- `math`: Render LaTeX formulas (`$…$`, `$$…$$`, `\(…\)`, `\[…\]`) to inline
  images through a configurable external renderer command — real LaTeX
  (tectonic/pdflatex), MathJax or KaTeX all work via the stdin→PNG contract.
  Formulas become real media attachments that federate as standard images
  to clients without formula support, with a linked LaTeX-source fallback
  for clients that strip `<img>`; formula rows are hidden from the
  on-screen media gallery since they already render inline. Disabled by
  default — see `[math]` in `config.toml.example`
  ([`b2c871f`](https://git.platypush.tech/blacklight/yonder/commit/b2c871ff02dc7d7abee6d7adb884d8ac0bf8a277)).

### Fixed

- `timelines`: Keep local statuses visible when domain blocks are present
  ([`aa01f58`](https://git.platypush.tech/blacklight/yonder/commit/aa01f5846a7ff76a1b8650a9b67b48db9b91e7d5)).
- `status`: Keep media and link previews behind content warnings
  ([`e5f82b5`](https://git.platypush.tech/blacklight/yonder/commit/e5f82b51fd696adda47172a6d06a083a5d36edb9)).
- `previews`: Reject media URLs as embed players
  ([`3136813`](https://git.platypush.tech/blacklight/yonder/commit/31368135d3907c7280c80ab710abd053c4fca2c8)).
- `ap`: Treat type-only `Image`, `Video`, and `Audio` ActivityPub attachments
  as media kinds, preserving inline images from SocialHome/PyFed
  ([`994f2c3`](https://git.platypush.tech/blacklight/yonder/commit/994f2c3be8a9d9bc8f5436656745d90f12077c3e)).
- `math`: Fix multi-line `$$…$$` formulas rendering as literal LaTeX or
  corrupted single lines — formulas are now extracted from the raw source
  before Markdown/plain-text rendering, preserving `aligned` environments,
  `\\` row separators and backslash spacing commands verbatim.

### Changed

- `docs`: Move the installation guide to `INSTALL.md`
  ([`5d358c1`](https://git.platypush.tech/blacklight/yonder/commit/5d358c1456cd0ab1d35bb5fe58f18d59f88649b5)).

## 1.0.3

### Added

- `layout`: Add a Compose shortcut to the middle of the mobile bottom
  navigation bar, replacing the duplicate profile link
  ([`f9e0e70`](https://git.platypush.tech/blacklight/yonder/commit/f9e0e7013fe28f67684f91541d0cff5ee3455ac1),
  [`d704211`](https://git.platypush.tech/blacklight/yonder/commit/d7042112c9021bc15fc497a8edfea181d57d8ca6)).
- `ci`: Build release artifacts once per tag and publish a GitHub release
  ([`681b00e`](https://git.platypush.tech/blacklight/yonder/commit/681b00e59d10b27098f9f29e5577742582ee835c)).

### Fixed

- `theme`: Persist the theme toggle via the settings store
  ([`c57a12d`](https://git.platypush.tech/blacklight/yonder/commit/c57a12d5a047babc36d7ba0816b43ca815c9f9cf)).
- `oauth`: Accept a trailing slash on `/oauth/authorize`
  ([`3e25778`](https://git.platypush.tech/blacklight/yonder/commit/3e257789d51b377c7abf662fc0283194a19f1fd7)).

### Changed

- `docs`: Add an architecture overview and link it from the README
  ([`81f9f88`](https://git.platypush.tech/blacklight/yonder/commit/81f9f887c67d35c97fbfbddaa37122d2d320b966)).

## 1.0.1

### Added

- Publish `quoteAuthorization` when accepting a `QuoteRequest`
  ([`d924240`](https://git.platypush.tech/blacklight/yonder/commit/d9242404276dc876fbf3d9e98d2d27926d63e746)).

### Fixed

- `compose`: Show an "Updated" toast with an Open link after edits
  ([`cdb29eb`](https://git.platypush.tech/blacklight/yonder/commit/cdb29eba11785e5a8a546292a0fcd1b2995ed301)).
- `auth`: Sync settings on restore to apply compose defaults
  ([`6c94cd7`](https://git.platypush.tech/blacklight/yonder/commit/6c94cd720ede0a9e5956b5a3d025644c5d2316f9)).

## 1.0.0

First public release of Yonder! 🎉
