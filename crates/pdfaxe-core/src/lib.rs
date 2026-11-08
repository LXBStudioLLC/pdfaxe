//! pdfaxe-core — the pure-Rust document engine behind PDFAxe.
//!
//! Structural operations (merge / extract / reorder / rotate) are all built on
//! one primitive, [`assemble`], which rebuilds a document from an ordered list
//! of source pages and carries the document-level structures (forms,
//! bookmarks, attachments, metadata) along. Rendering lives behind
//! [`render`] so the renderer choice stays swappable without touching
//! anything else.

pub(crate) mod carry;
pub(crate) mod fonts;
pub mod images;
pub(crate) mod metrics;
pub mod ops;
pub mod ranges;
pub mod redact;
pub mod render;
pub mod scrub;
pub mod stamp;
pub mod text;

/// Re-exported so downstream crates use the same lopdf version as the core.
pub use lopdf;

pub use ops::{
    assemble, extract_text, info, load, page_sizes, page_view, page_views, rotate_pages, save, DocInfo,
    PageSel, PageView,
};
pub use images::images_to_pdf;
pub use ranges::parse_ranges;
pub use redact::{redact_page, redact_pages, RedactOpts, RedactRect, RedactReport};
pub use scrub::{drop_outline_items_for_pages, strip_metadata, StripReport};
pub use stamp::{page_numbers, watermark, StampOpts};
