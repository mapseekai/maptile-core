//! Map style management.
//!
//! - [`store::StyleStore`] holds parsed `style.json` documents.
//! - [`store::rewrite_urls`] converts relative source/glyph/sprite URLs to
//!   absolute base-URL-prefixed forms (used both for client delivery and as
//!   an intermediate step before native rendering, see [`crate::render`]).
//! - [`catalog::snapshot`] produces the `/styles.json` payload.

pub mod catalog;
pub mod store;

pub use catalog::{StyleInfo, snapshot};
pub use store::{LocalStyleStore, Style, StyleStore, rewrite_style_for_native, rewrite_urls};
