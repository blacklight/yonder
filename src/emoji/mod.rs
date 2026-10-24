//! Custom emoji — administration, packs, import and `:shortcode:`
//! rendering (epic 20).
//!
//! `model` owns `custom_emojis`/`emoji_packs` rows and storage;
//! `entity` renders the Mastodon `CustomEmoji` shape and the public
//! list; `render` does read-time `:shortcode:` substitution (stored
//! content keeps the literal text so emoji added later still render);
//! `import` handles zip and filesystem bulk imports.

pub mod entity;
pub mod import;
pub mod model;
pub mod render;

pub use model::EmojiRow;
