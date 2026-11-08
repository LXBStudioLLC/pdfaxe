//! Redaction by raster burn: the page is rendered to pixels, the redaction
//! rectangles are burned black, and the page is *replaced* by that bitmap.
//! Unlike draw-a-black-box "redactions", nothing survives under the ink —
//! the covered text simply no longer exists in the file.
//!
//! "No longer exists" is enforced at the object level, not just the page
//! tree: the page dictionary is rebuilt from scratch (dropping annotations,
//! the embedded thumbnail, structure references and everything else that
//! could carry a copy of the content), form fields whose widgets lived on
//! the page leave the AcroForm tree so their values go too, the tagged-PDF
//! structure tree is dropped, and unreachable objects are pruned before the
//! function returns — so the old content streams never reach disk, whatever
//! save path the caller uses.
//!
//! The trade-off, stated honestly: the whole page becomes an image. Its text
//! is no longer selectable or extractable (that's the point for the covered
//! part, and the price for the rest of the page), and the file grows by the
//! bitmap's size.

use anyhow::{anyhow, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use std::io::Write;

use crate::ops::{page_view, PageView};
use crate::render::render_page_rgba;
use crate::scrub::{self, StripReport};

/// A redaction box in page points — *display space*: origin at the
/// bottom-left of the page as the viewer shows it, rotation and CropBox
/// already applied. This is what a mouse drag on a rendered page yields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RedactRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Document-level choices for a redaction job.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RedactOpts {
    /// Raster resolution of the replacement bitmap (200 reads well; the
    /// bitmap is clamped to 300..5000 px wide).
    pub dpi: f32,
    /// Also remove the document's Info dictionary, XMP metadata and
    /// application private data — the classic places a title, author or
    /// revision history leaks from.
    pub strip_metadata: bool,
    /// Also remove bookmarks whose destination is a redacted page: their
    /// titles tend to say what the page was about.
    pub prune_bookmarks: bool,
}

impl Default for RedactOpts {
    fn default() -> Self {
        Self { dpi: 200.0, strip_metadata: true, prune_bookmarks: true }
    }
}

/// What a redaction job did beyond burning pixels.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RedactReport {
    pub pages: Vec<u32>,
    pub boxes: usize,
    /// Form-field nodes detached because their widgets sat on a burned page.
    pub fields_detached: usize,
    /// Whether a tagged-PDF structure tree was dropped.
    pub structure_dropped: bool,
    /// Metadata removed (when `strip_metadata` was set).
    pub metadata: StripReport,
    /// Bookmarks removed (when `prune_bookmarks` was set).
    pub bookmarks_removed: usize,
}

/// Burn `rects` out of one page (1-based) at `dpi`. Page-level cleanup
/// (annotations, thumbnail, form fields, structure tree, orphan pruning) is
/// part of this call; document-level metadata/bookmark scrubbing is
/// [`redact_pages`]'s job.
pub fn redact_page(doc: &mut Document, page_no: u32, rects: &[RedactRect], dpi: f32) -> Result<()> {
    redact_page_inner(doc, page_no, rects, dpi).map(|_| ())
}

struct PageOutcome {
    fields_detached: usize,
    structure_dropped: bool,
}

fn redact_page_inner(doc: &mut Document, page_no: u32, rects: &[RedactRect], dpi: f32) -> Result<PageOutcome> {
    if rects.is_empty() {
        return Err(anyhow!("no redaction rectangles given"));
    }
    let view: PageView = page_view(doc, page_no)?;
    let (dw, _) = view.display_size();
    let target = ((dw * dpi / 72.0).round() as u32).clamp(300, 5000);

    // Render the page exactly as displayed (CropBox, rotation).
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes)
        .map_err(|e| anyhow!("couldn't serialize document for rendering: {e}"))?;
    let r = render_page_rgba(&bytes, page_no, target)?;

    // Composite over white paper (premultiplied alpha: out = c + (255 - a)).
    let mut rgb = Vec::with_capacity((r.width * r.height * 3) as usize);
    for px in r.pixels.chunks_exact(4) {
        let a = px[3] as u32;
        rgb.push((px[0] as u32 + (255 - a)).min(255) as u8);
        rgb.push((px[1] as u32 + (255 - a)).min(255) as u8);
        rgb.push((px[2] as u32 + (255 - a)).min(255) as u8);
    }

    // Burn the boxes. Display space maps straight onto the bitmap.
    let sx = r.width as f32 / r.point_width.max(1.0);
    let sy = r.height as f32 / r.point_height.max(1.0);
    for rect in rects {
        let x0 = (rect.x * sx).floor().clamp(0.0, r.width as f32) as usize;
        let x1 = ((rect.x + rect.w) * sx).ceil().clamp(0.0, r.width as f32) as usize;
        let y0 = ((r.point_height - rect.y - rect.h) * sy).floor().clamp(0.0, r.height as f32) as usize;
        let y1 = ((r.point_height - rect.y) * sy).ceil().clamp(0.0, r.height as f32) as usize;
        for py in y0..y1 {
            let row = py * r.width as usize * 3;
            for px in x0..x1 {
                let o = row + px * 3;
                rgb[o] = 0;
                rgb[o + 1] = 0;
                rgb[o + 2] = 0;
            }
        }
    }

    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&rgb).map_err(|e| anyhow!("couldn't compress bitmap: {e}"))?;
    let compressed = enc.finish().map_err(|e| anyhow!("couldn't compress bitmap: {e}"))?;

    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => r.width as i64,
            "Height" => r.height as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "FlateDecode",
        },
        compressed,
    ));
    // The bitmap shows the page as displayed (rotation and crop applied), so
    // the rebuilt page is unrotated, uncropped, with a fresh origin and the
    // renderer's exact display size.
    let (pw, ph) = (r.point_width, r.point_height);
    let content = format!("q\n{pw} 0 0 {ph} 0 0 cm\n/PAXImg Do\nQ");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));

    let page_map = doc.get_pages();
    let &page_id = page_map.get(&page_no).ok_or_else(|| anyhow!("page {page_no} out of range"))?;
    let old = doc
        .get_dictionary(page_id)
        .map_err(|e| anyhow!("page dictionary unavailable: {e}"))?
        .clone();

    // Annotations that were on this page: remembered so their form fields
    // can be detached below.
    let old_annots: Vec<ObjectId> = match old.get(b"Annots") {
        Ok(Object::Array(a)) => a.iter().filter_map(|o| o.as_reference().ok()).collect(),
        Ok(Object::Reference(id)) => match doc.get_object(*id) {
            Ok(Object::Array(a)) => a.iter().filter_map(|o| o.as_reference().ok()).collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };

    // Rebuild the page dictionary from nothing. Only the tree link and the
    // bitmap survive; /Thumb, /Annots, /StructParents, /PieceInfo,
    // /Metadata, /AA, /B, art/trim/bleed boxes and anything else from the
    // old page are gone with it.
    let media: Vec<Object> = vec![0.into(), 0.into(), pw.into(), ph.into()];
    let mut fresh = dictionary! {
        "Type" => "Page",
        "MediaBox" => media,
        "Resources" => dictionary! { "XObject" => dictionary! { "PAXImg" => Object::Reference(image_id) } },
        "Contents" => Object::Reference(content_id),
    };
    if let Ok(parent) = old.get(b"Parent") {
        fresh.set("Parent", parent.clone());
    }
    if let Ok(uu) = old.get(b"UserUnit") {
        fresh.set("UserUnit", uu.clone());
    }
    doc.objects.insert(page_id, Object::Dictionary(fresh));

    let fields_detached = scrub::detach_form_fields(doc, &old_annots);
    let structure_dropped = scrub::drop_structure_tree(doc);

    // Whatever save path follows, the old content must not be in the file:
    // prune everything that just became unreachable (old content streams,
    // annotations and their popups, the thumbnail, detached fields).
    doc.prune_objects();
    Ok(PageOutcome { fields_detached, structure_dropped })
}

/// Burn boxes on several pages and apply the document-level scrubbing the
/// options ask for. `jobs` pairs each 1-based page with its boxes.
pub fn redact_pages(doc: &mut Document, jobs: &[(u32, Vec<RedactRect>)], opts: &RedactOpts) -> Result<RedactReport> {
    if jobs.is_empty() {
        return Err(anyhow!("no pages to redact"));
    }
    let mut report = RedactReport::default();
    for (page, rects) in jobs {
        let outcome = redact_page_inner(doc, *page, rects, opts.dpi)?;
        report.pages.push(*page);
        report.boxes += rects.len();
        report.fields_detached += outcome.fields_detached;
        report.structure_dropped |= outcome.structure_dropped;
    }
    if opts.prune_bookmarks {
        report.bookmarks_removed = scrub::drop_outline_items_for_pages(doc, &report.pages);
    }
    if opts.strip_metadata {
        report.metadata = scrub::strip_metadata(doc);
    }
    doc.prune_objects();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{extract_text, load};
    use crate::text::text_runs;
    use lopdf::dictionary;
    use std::path::PathBuf;

    const SECRET: &[u8] = b"SECRET-CODE-42";
    /// The fixture writes its strings as hex, so scan for that form too.
    const SECRET_HEX: &[u8] = b"5345435245542D434F44452D3432";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
    }

    fn secret_rect(doc: &Document, page: u32) -> RedactRect {
        let runs = text_runs(doc, page).unwrap();
        let s = runs.iter().find(|r| r.text.contains("SECRET")).unwrap().clone();
        RedactRect { x: s.x - 2.0, y: s.y - 4.0, w: s.width.unwrap_or(200.0) + 6.0, h: s.font_size + 10.0 }
    }

    /// Every object in a saved file whose stream (decoded) contains the
    /// marker — orphans included, exactly as an attacker would look.
    fn streams_containing(saved: &[u8], needles: &[&[u8]]) -> usize {
        let re = Document::load_mem(saved).unwrap();
        re.objects
            .values()
            .filter(|o| match o {
                Object::Stream(s) => {
                    let data = s.decompressed_content().unwrap_or_else(|_| s.content.clone());
                    needles.iter().any(|n| data.windows(n.len()).any(|w| w == *n))
                }
                _ => false,
            })
            .count()
    }

    #[test]
    fn redact_burns_text_out_of_the_file() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let rect = secret_rect(&doc, 1);
        let runs = text_runs(&doc, 1).unwrap();
        let secret = runs.iter().find(|r| r.text.contains("SECRET")).unwrap().clone();

        redact_page(&mut doc, 1, &[rect], 100.0).unwrap();

        // The page is a bitmap now — nothing extractable, SECRET least of all.
        let t1 = extract_text(&doc, 1).unwrap_or_default();
        assert!(!t1.contains("SECRET"), "text survived redaction: {t1}");
        // Other pages keep their text.
        assert!(extract_text(&doc, 2).unwrap().contains("SECRET"));

        // Save, reload, render: burn area black, margins still paper-white.
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let img = crate::render::render_page_rgba(&buf, 1, 600).unwrap();
        let sx = img.width as f32 / img.point_width;
        let sy = img.height as f32 / img.point_height;
        let sample = |x_pt: f32, y_pt: f32| -> (u8, u8, u8, u8) {
            let px = (x_pt * sx) as usize;
            let py = ((img.point_height - y_pt) * sy) as usize;
            let o = (py * img.width as usize + px) * 4;
            (img.pixels[o], img.pixels[o + 1], img.pixels[o + 2], img.pixels[o + 3])
        };
        let (r, g, b, _) = sample(secret.x + 10.0, secret.y + 4.0);
        assert!(r < 30 && g < 30 && b < 30, "burn area should be black, got {r},{g},{b}");
        let (r, g, b, _) = sample(img.point_width / 2.0, 30.0);
        assert!(r > 220 && g > 220 && b > 220, "margin should stay white, got {r},{g},{b}");
    }

    /// The regression test for the CLI leak: a plain `save_to` (no assemble,
    /// no prune by the caller) must not ship the original page content as an
    /// orphan object.
    #[test]
    fn redacted_file_holds_no_copy_of_the_original_page() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let mut before = Vec::new();
        doc.save_to(&mut before).unwrap();
        assert_eq!(streams_containing(&before, &[SECRET, SECRET_HEX]), 5, "fixture: one marker per page");

        let rect = secret_rect(&doc, 1);
        redact_page(&mut doc, 1, &[rect], 100.0).unwrap();
        let mut after = Vec::new();
        doc.save_to(&mut after).unwrap();
        assert_eq!(
            streams_containing(&after, &[SECRET, SECRET_HEX]),
            4,
            "only the four untouched pages may still carry the marker — no orphan copy of page 1"
        );
    }

    #[test]
    fn redaction_detaches_form_fields_whose_widgets_were_on_the_page() {
        let mut doc = load(fixture("sample-form.pdf")).unwrap();
        // Give one field a value worth protecting.
        let widget = {
            let p1 = doc.get_pages()[&1];
            let annots = doc.get_dictionary(p1).unwrap().get(b"Annots").unwrap().clone();
            let arr = match annots {
                Object::Array(a) => a,
                Object::Reference(id) => doc.get_object(id).unwrap().as_array().unwrap().clone(),
                _ => panic!(),
            };
            arr[0].as_reference().unwrap()
        };
        doc.get_dictionary_mut(widget).unwrap().set("V", Object::string_literal("123-45-6789"));

        let (dw, dh) = page_view(&doc, 1).unwrap().display_size();
        let rect = RedactRect { x: 10.0, y: 10.0, w: dw - 20.0, h: dh - 20.0 };
        let mut report = redact_pages(&mut doc, &[(1, vec![rect])], &RedactOpts { dpi: 72.0, ..Default::default() }).unwrap();
        assert!(report.fields_detached >= 3, "all three fields lived on page 1: {report:?}");
        report.metadata = StripReport::default();

        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        assert!(doc.get_dictionary(root).unwrap().get(b"AcroForm").is_err(), "no fields left → no AcroForm");
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        assert!(!buf.windows(11).any(|w| w == b"123-45-6789"), "field value leaked into the saved file");
        let re = Document::load_mem(&buf).unwrap();
        assert!(!re.objects.values().any(|o| o.as_dict().is_ok_and(|d| d.get(b"FT").is_ok())), "no field dictionaries may remain");
    }

    #[test]
    fn redaction_drops_the_pages_thumbnail_and_extras() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let p1 = doc.get_pages()[&1];
        let thumb = doc.add_object(Stream::new(
            dictionary! { "Width" => 2, "Height" => 2, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
            b"THUMB-OF-THE-ORIGINAL".to_vec(),
        ));
        let page = doc.get_dictionary_mut(p1).unwrap();
        page.set("Thumb", Object::Reference(thumb));
        page.set("PieceInfo", dictionary! { "App" => dictionary! { "Private" => Object::string_literal("PIECE-PRIVATE") } });
        page.set("TrimBox", vec![10.into(), 10.into(), 500.into(), 800.into()]);

        let rect = secret_rect(&doc, 1);
        redact_page(&mut doc, 1, &[rect], 72.0).unwrap();
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        assert!(!buf.windows(21).any(|w| w == b"THUMB-OF-THE-ORIGINAL"), "thumbnail leaked");
        assert!(!buf.windows(13).any(|w| w == b"PIECE-PRIVATE"), "page PieceInfo leaked");
        let page = doc.get_dictionary(p1).unwrap();
        assert!(page.get(b"Thumb").is_err() && page.get(b"TrimBox").is_err() && page.get(b"Annots").is_err());
    }

    #[test]
    fn redaction_on_a_rotated_cropped_page_burns_where_the_viewer_shows_it() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let runs = text_runs(&doc, 1).unwrap();
        let secret = runs.iter().find(|r| r.text.contains("SECRET")).unwrap().clone();
        let p1 = doc.get_pages()[&1];
        {
            let page = doc.get_dictionary_mut(p1).unwrap();
            page.set("CropBox", vec![20.into(), 30.into(), 575.into(), 820.into()]);
            page.set("Rotate", 90);
        }
        // The box is specified in display space — what a user drags on the
        // rotated, cropped rendering — via the same mapping the GUI uses.
        let view = page_view(&doc, 1).unwrap();
        let (bx, by, bw, bh) = view.rect_to_display(
            secret.x - 2.0,
            secret.y - 4.0,
            secret.width.unwrap_or(200.0) + 6.0,
            secret.font_size + 10.0,
        );
        redact_page(&mut doc, 1, &[RedactRect { x: bx, y: by, w: bw, h: bh }], 100.0).unwrap();

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let img = crate::render::render_page_rgba(&buf, 1, 800).unwrap();
        let (dw, dh) = view.display_size();
        assert!((img.point_width - dw).abs() < 0.5 && (img.point_height - dh).abs() < 0.5, "rebuilt page must keep the displayed size: {}x{} vs {dw}x{dh}", img.point_width, img.point_height);
        let sx = img.width as f32 / img.point_width;
        let sy = img.height as f32 / img.point_height;
        let sample = |dx: f32, dy: f32| -> (u8, u8, u8) {
            let px = ((dx * sx) as usize).min(img.width as usize - 1);
            let py = (((img.point_height - dy) * sy) as usize).min(img.height as usize - 1);
            let o = (py * img.width as usize + px) * 4;
            (img.pixels[o], img.pixels[o + 1], img.pixels[o + 2])
        };
        // Centre of where the text is displayed: black.
        let (tx, ty) = view.to_display(secret.x + 10.0, secret.y + 4.0);
        let (r, g, b) = sample(tx, ty);
        assert!(r < 30 && g < 30 && b < 30, "burn must land on the displayed text, got {r},{g},{b} at ({tx},{ty})");
        // A spot far from the text (displayed bottom-left region): white.
        let (r, g, b) = sample(dw * 0.5, 12.0);
        assert!(r > 200 && g > 200 && b > 200, "far from the box must stay paper, got {r},{g},{b}");
    }

    #[test]
    fn redact_pages_scrubs_metadata_and_bookmarks_for_burned_pages() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let pages = doc.get_pages();
        let (p1, p2) = (pages[&1], pages[&2]);
        let info = doc.add_object(dictionary! { "Title" => Object::string_literal("TOP-SECRET-TITLE") });
        doc.trailer.set("Info", Object::Reference(info));
        let root = doc.new_object_id();
        let i1 = doc.new_object_id();
        let i2 = doc.new_object_id();
        doc.objects.insert(i1, Object::Dictionary(dictionary! {
            "Title" => Object::string_literal("Salary table"), "Parent" => Object::Reference(root), "Next" => Object::Reference(i2),
            "Dest" => vec![Object::Reference(p1), Object::Name(b"Fit".to_vec())],
        }));
        doc.objects.insert(i2, Object::Dictionary(dictionary! {
            "Title" => Object::string_literal("Appendix"), "Parent" => Object::Reference(root), "Prev" => Object::Reference(i1),
            "Dest" => vec![Object::Reference(p2), Object::Name(b"Fit".to_vec())],
        }));
        doc.objects.insert(root, Object::Dictionary(dictionary! {
            "Type" => "Outlines", "First" => Object::Reference(i1), "Last" => Object::Reference(i2), "Count" => 2,
        }));
        let cat = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(cat).unwrap().set("Outlines", Object::Reference(root));

        let rect = secret_rect(&doc, 1);
        let report = redact_pages(&mut doc, &[(1, vec![rect])], &RedactOpts { dpi: 72.0, ..Default::default() }).unwrap();
        assert_eq!(report.pages, vec![1]);
        assert_eq!(report.bookmarks_removed, 1);
        assert!(report.metadata.info);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        assert!(!buf.windows(16).any(|w| w == b"TOP-SECRET-TITLE"));
        assert!(!buf.windows(12).any(|w| w == b"Salary table"), "bookmark to the burned page must go");
        assert!(buf.windows(8).any(|w| w == b"Appendix"), "bookmarks to other pages stay");

        // Opting out keeps both.
        let mut doc2 = load(fixture("sample-a.pdf")).unwrap();
        let info2 = doc2.add_object(dictionary! { "Title" => Object::string_literal("KEEP-ME") });
        doc2.trailer.set("Info", Object::Reference(info2));
        let rect2 = secret_rect(&doc2, 1);
        let opts = RedactOpts { dpi: 72.0, strip_metadata: false, prune_bookmarks: false };
        let rep2 = redact_pages(&mut doc2, &[(1, vec![rect2])], &opts).unwrap();
        assert!(rep2.metadata.is_empty());
        assert!(doc2.trailer.get(b"Info").is_ok());
    }
}
