//! Stamping: draw new text onto existing pages (watermarks, page numbers).
//!
//! The original content is wrapped in its own `q … Q` sandwich so leftover
//! graphics state (transforms, clips) can't displace the stamp, and each stamp
//! is isolated the same way. Text uses Helvetica (standard-14, no embedding
//! needed) with WinAnsi bytes; non-encodable characters degrade to '?'.
//!
//! Placement is computed in *display space* — the page as the viewer shows
//! it, CropBox and /Rotate applied — and mapped back into user space, so a
//! "bottom centre" page number lands at the displayed bottom of a rotated or
//! cropped page and reads upright.

use anyhow::{anyhow, Result};
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream, StringFormat};

use crate::ops::{page_view_for, resolve_inherited};

const STAMP_FONT_RES: &str = "PAXHelv";
const STAMP_GS_RES: &str = "PAXGs";

#[derive(Clone, Copy)]
pub struct StampOpts {
    pub size: f32,
    /// Fill gray: 0.0 black .. 1.0 white.
    pub gray: f32,
    /// Fill alpha 0.0..1.0 (via ExtGState; 1.0 skips the gstate entirely).
    pub alpha: f32,
    /// Counter-clockwise rotation in degrees, as seen on screen.
    pub angle: f32,
}

impl Default for StampOpts {
    fn default() -> Self {
        Self { size: 48.0, gray: 0.6, alpha: 0.35, angle: 45.0 }
    }
}

/// Stamp `text` diagonally across the center of the given pages (1-based).
pub fn watermark(doc: &mut Document, pages: &[u32], text: &str, opts: &StampOpts) -> Result<()> {
    let page_map = doc.get_pages();
    let mut targets = Vec::with_capacity(pages.len());
    for p in pages {
        let &id = page_map.get(p).ok_or_else(|| anyhow!("page {p} out of range"))?;
        targets.push(id);
    }
    let font_id = add_helvetica(doc);
    let gs_id = (opts.alpha < 1.0).then(|| {
        doc.add_object(dictionary! {
            "Type" => "ExtGState",
            "ca" => opts.alpha,
            "CA" => opts.alpha,
        })
    });
    let bytes = winansi_bytes(text);
    let w = approx_width(text, opts.size);
    let rad = opts.angle.to_radians();
    for page_id in targets {
        ensure_stamp_resources(doc, page_id, font_id, gs_id)?;
        let view = page_view_for(doc, page_id);
        let (dw, dh) = view.display_size();
        // Baseline start in display space: half the text length back from
        // the displayed centre along the baseline direction; then into user
        // space, counter-rotating the text by the page's /Rotate so it reads
        // at the requested angle on screen.
        let (cx, cy) = (dw / 2.0, dh / 2.0);
        let dx = cx - (w / 2.0) * rad.cos();
        let dy = cy - (w / 2.0) * rad.sin();
        let (x, y) = view.from_display(dx, dy);
        let placed = StampOpts { angle: opts.angle + view.rotate as f32, ..*opts };
        append_stamp(doc, page_id, stamp_ops(bytes.clone(), x, y, &placed, gs_id.is_some()))?;
    }
    Ok(())
}

/// Stamp a label on every page, bottom center as displayed. `template` may
/// use `{page}` and `{total}`. Returns the number of pages stamped.
pub fn page_numbers(doc: &mut Document, template: &str, opts: &StampOpts) -> Result<u32> {
    let page_map = doc.get_pages();
    let total = page_map.len() as u32;
    let targets: Vec<(u32, ObjectId)> = page_map.iter().map(|(&n, &id)| (n, id)).collect();
    let font_id = add_helvetica(doc);
    let opts = StampOpts { angle: 0.0, alpha: 1.0, ..*opts };
    for (n, page_id) in targets {
        let label = template
            .replace("{page}", &n.to_string())
            .replace("{total}", &total.to_string());
        ensure_stamp_resources(doc, page_id, font_id, None)?;
        let view = page_view_for(doc, page_id);
        let (dw, _) = view.display_size();
        let dx = dw / 2.0 - approx_width(&label, opts.size) / 2.0;
        let dy = 24.0;
        let (x, y) = view.from_display(dx, dy);
        let placed = StampOpts { angle: view.rotate as f32, ..opts };
        append_stamp(doc, page_id, stamp_ops(winansi_bytes(&label), x, y, &placed, false))?;
    }
    Ok(total)
}

fn add_helvetica(doc: &mut Document) -> ObjectId {
    doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    })
}

fn resolved_dict(doc: &Document, obj: &Object) -> Option<Dictionary> {
    let resolved = match obj {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    resolved.as_dict().ok().cloned()
}

/// Give the page its own /Resources carrying the stamp font (and gstate),
/// materializing inherited resources if needed.
fn ensure_stamp_resources(
    doc: &mut Document,
    page_id: ObjectId,
    font_id: ObjectId,
    gs_id: Option<ObjectId>,
) -> Result<()> {
    let mut res = resolve_inherited(doc, page_id, b"Resources")
        .and_then(|o| resolved_dict(doc, &o))
        .unwrap_or_default();

    let mut fonts = res
        .get(b"Font")
        .ok()
        .and_then(|o| resolved_dict(doc, o))
        .unwrap_or_default();
    fonts.set(STAMP_FONT_RES, font_id);
    res.set("Font", Object::Dictionary(fonts));

    if let Some(gs_id) = gs_id {
        let mut gstates = res
            .get(b"ExtGState")
            .ok()
            .and_then(|o| resolved_dict(doc, o))
            .unwrap_or_default();
        gstates.set(STAMP_GS_RES, gs_id);
        res.set("ExtGState", Object::Dictionary(gstates));
    }

    let page = doc
        .get_object_mut(page_id)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("page dictionary unavailable: {e}"))?;
    page.set("Resources", Object::Dictionary(res));
    Ok(())
}

/// Helvetica line width from the built-in standard-14 metrics.
fn approx_width(text: &str, size: f32) -> f32 {
    use crate::metrics::{advance, std14_widths, AdvanceState};
    let st = AdvanceState { font_size: size, ..Default::default() };
    std14_widths(b"Helvetica")
        .and_then(|gw| advance(&gw, &winansi_bytes(text), &st))
        .unwrap_or_else(|| text.chars().count() as f32 * size * 0.5)
}

fn winansi_bytes(text: &str) -> Vec<u8> {
    text.chars()
        .map(|ch| match ch {
            '€' => 0x80, '‚' => 0x82, 'ƒ' => 0x83, '„' => 0x84, '…' => 0x85,
            '†' => 0x86, '‡' => 0x87, 'ˆ' => 0x88, '‰' => 0x89, 'Š' => 0x8A,
            '‹' => 0x8B, 'Œ' => 0x8C, 'Ž' => 0x8E, '‘' => 0x91, '’' => 0x92,
            '“' => 0x93, '”' => 0x94, '•' => 0x95, '–' => 0x96, '—' => 0x97,
            '˜' => 0x98, '™' => 0x99, 'š' => 0x9A, '›' => 0x9B, 'œ' => 0x9C,
            'ž' => 0x9E, 'Ÿ' => 0x9F,
            c if (c as u32) < 0x100 => c as u8,
            _ => b'?',
        })
        .collect()
}

/// Ops for one stamp: closes the wrapper `q`, then draws inside its own q…Q.
fn stamp_ops(text_bytes: Vec<u8>, x: f32, y: f32, opts: &StampOpts, use_gs: bool) -> Content {
    let rad = opts.angle.to_radians();
    let (c, s) = (rad.cos(), rad.sin());
    let mut ops = vec![Operation::new("Q", vec![]), Operation::new("q", vec![])];
    if use_gs {
        ops.push(Operation::new("gs", vec![Object::Name(STAMP_GS_RES.as_bytes().to_vec())]));
    }
    ops.extend([
        Operation::new("BT", vec![]),
        Operation::new(
            "Tf",
            vec![Object::Name(STAMP_FONT_RES.as_bytes().to_vec()), opts.size.into()],
        ),
        Operation::new("g", vec![opts.gray.into()]),
        Operation::new("Tm", vec![c.into(), s.into(), (-s).into(), c.into(), x.into(), y.into()]),
        Operation::new("Tj", vec![Object::String(text_bytes, StringFormat::Literal)]),
        Operation::new("ET", vec![]),
        Operation::new("Q", vec![]),
    ]);
    Content { operations: ops }
}

/// Contents := [ "q", …original…, stamp ] so original state can't leak in.
fn append_stamp(doc: &mut Document, page_id: ObjectId, content: Content) -> Result<()> {
    let original: Vec<Object> = {
        let page = doc
            .get_dictionary(page_id)
            .map_err(|e| anyhow!("page dictionary unavailable: {e}"))?;
        match page.get(b"Contents") {
            Ok(Object::Reference(id)) => match doc.get_object(*id) {
                Ok(Object::Array(a)) => a.clone(),
                _ => vec![Object::Reference(*id)],
            },
            Ok(Object::Array(a)) => a.clone(),
            _ => Vec::new(),
        }
    };
    let encoded = content.encode().map_err(|e| anyhow!("couldn't encode stamp: {e}"))?;
    let pre_id = doc.add_object(Stream::new(dictionary! {}, b"q\n".to_vec()));
    let post_id = doc.add_object(Stream::new(dictionary! {}, encoded));

    let mut arr = Vec::with_capacity(original.len() + 2);
    arr.push(Object::Reference(pre_id));
    arr.extend(original);
    arr.push(Object::Reference(post_id));

    let page = doc
        .get_object_mut(page_id)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("page dictionary unavailable: {e}"))?;
    page.set("Contents", Object::Array(arr));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{extract_text, load};
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
    }

    #[test]
    fn watermark_stamps_selected_pages_only() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        watermark(&mut doc, &[1, 3], "CONFIDENTIAL", &StampOpts::default()).unwrap();
        assert!(extract_text(&doc, 1).unwrap().contains("CONFIDENTIAL"));
        assert!(!extract_text(&doc, 2).unwrap().contains("CONFIDENTIAL"));
        assert!(extract_text(&doc, 3).unwrap().contains("CONFIDENTIAL"));
        // Original content must survive the wrap.
        assert!(extract_text(&doc, 1).unwrap().contains("SECRET-CODE-42"));
    }

    #[test]
    fn page_numbers_hit_every_page() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let opts = StampOpts { size: 10.0, gray: 0.33, ..Default::default() };
        let total = page_numbers(&mut doc, "{page} / {total}", &opts).unwrap();
        assert_eq!(total, 5);
        assert!(extract_text(&doc, 3).unwrap().contains("3 / 5"));
        assert!(extract_text(&doc, 5).unwrap().contains("5 / 5"));
    }

    #[test]
    fn stamped_doc_saves_and_reloads() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        watermark(&mut doc, &[1], "DRAFT", &StampOpts::default()).unwrap();
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let reloaded = Document::load_mem(&buf).unwrap();
        let text = crate::ops::extract_text(&reloaded, 1).unwrap();
        assert!(text.contains("DRAFT"));
    }

    #[test]
    fn page_numbers_land_at_the_displayed_bottom_of_rotated_and_cropped_pages() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        crate::ops::rotate_pages(&mut doc, &[2], 90).unwrap();
        {
            let p3 = doc.get_pages()[&3];
            let page = doc.get_dictionary_mut(p3).unwrap();
            page.set("CropBox", vec![40.into(), 60.into(), 500.into(), 800.into()]);
        }
        let opts = StampOpts { size: 10.0, gray: 0.33, ..Default::default() };
        page_numbers(&mut doc, "{page}", &opts).unwrap();
        for (page, label) in [(2u32, "2"), (3u32, "3")] {
            let view = crate::ops::page_view(&doc, page).unwrap();
            let (dw, _) = view.display_size();
            let runs = crate::text::text_runs(&doc, page).unwrap();
            let stamp = runs.iter().find(|r| r.text == label && r.font_res == STAMP_FONT_RES).expect("stamp run");
            let (dx, dy) = view.to_display(stamp.x, stamp.y);
            assert!((dy - 24.0).abs() < 0.5, "page {page}: baseline must sit 24pt above the displayed bottom, got {dy}");
            let want_x = dw / 2.0 - approx_width(label, 10.0) / 2.0;
            assert!((dx - want_x).abs() < 0.5, "page {page}: centred on the displayed width, got {dx} want {want_x}");
        }
        // Rendering still works after the rotation-aware stamp.
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        crate::render::render_page_rgba(&buf, 2, 300).unwrap();
    }
}
