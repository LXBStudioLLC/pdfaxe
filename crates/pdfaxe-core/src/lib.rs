//! pdfaxe-core — the pure-Rust document engine behind PDFAxe.
//!
//! Structural operations (merge / extract / reorder / rotate) are all built on
//! one primitive, [`assemble`], which rebuilds a document from an ordered list
//! of source pages. Rendering lives behind the [`render::PageRenderer`] trait
//! so the pure-Rust-renderer-vs-PDFium decision stays swappable without
//! touching anything else.

pub(crate) mod fonts;
pub mod images;
pub(crate) mod metrics;
pub mod ops;
pub mod ranges;
pub mod redact;
pub mod render;
pub mod stamp;
pub mod text;

/// Re-exported so downstream crates use the same lopdf version as the core.
pub use lopdf;

pub use ops::{
    assemble, extract_text, info, load, page_sizes, rotate_pages, save, DocInfo, PageSel,
};
pub use images::images_to_pdf;
pub use ranges::parse_ranges;
pub use redact::{redact_page, RedactRect};
pub use stamp::{page_numbers, watermark, StampOpts};
