# CHANGELOG

## Unreleased

### Added

- `math`: Render LaTeX formulas (`$…$`, `$$…$$`, `\(…\)`, `\[…\]`) to
  inline images through a configurable external renderer command —
  real LaTeX (tectonic/pdflatex), MathJax or KaTeX all work via the
  stdin→PNG contract. Formulas become real media attachments that
  federate as standard images to clients without formula support, with
  a linked LaTeX-source fallback for clients that strip `<img>`;
  formula rows are hidden from the on-screen media gallery since they
  already render inline. Disabled by default — see `[math]` in
  `config.toml.example`.

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
