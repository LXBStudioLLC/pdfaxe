//! Stamping: draw new text onto existing pages (watermarks, page numbers).
//!
//! The original content is wrapped in its own `q … Q` sandwich so leftover
//! graphics state (transforms, clips) can't displace the stamp, and each stamp
//! is isolated the same way. Text uses Helvetica (standard-14, no embedding
//! needed) with WinAnsi bytes; non-encodable characters degrade to '?'.

use anyhow::{anyhow, Result};
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream, StringFormat};

use crate::ops::resolve_inherited;

const STAMP_FONT_RES: &str = "PAXHelv";
const STAMP_GS_RES: &str = "PAXGs";

#[derive(Clone, Copy)]
pub struct StampOpts {
    pub size: f32,
    /// Fill gray: 0.0 black .. 1.0 white.
    pub gray: f32,
    /// Fill alpha 0.0..1.0 (via ExtGState; 1.0 skips the gstate entirely).
    pub alpha: f32,
    /// Counter-clockwise rotation in degrees.
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
        let (x0, y0, x1, y1) = page_media_box(doc, page_id);
        // Baseline start: half the text length back from the page center,
        // along the rotated baseline direction.
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let x = cx - (w / 2.0) * rad.cos();
        let y = cy - (w / 2.0) * rad.sin();
        append_stamp(doc, page_id, stamp_ops(bytes.clone(), x, y, opts, gs_id.is_some()))?;
    }
    Ok(())
}

/// Stamp a label on every page, bottom center. `template` may use `{page}`
/// and `{total}`. Returns the number of pages stamped.
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
        let (x0, y0, x1, _) = page_media_box(doc, page_id);
        let x = (x0 + x1) / 2.0 - approx_width(&label, opts.size) / 2.0;
        let y = y0 + 24.0;
        append_stamp(doc, page_id, stamp_ops(winansi_bytes(&label), x, y, &opts, false))?;
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

fn num(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r as f32),
        _ => None,
    }
}

fn page_media_box(doc: &Document, page_id: ObjectId) -> (f32, f32, f32, f32) {
    let nums: Vec<f32> = resolve_inherited(doc, page_id, b"MediaBox")
        .and_then(|o| match o {
            Object::Array(a) => Some(a),
            Object::Reference(id) => match doc.get_object(id) {
                Ok(Object::Array(a)) => Some(a.clone()),
                _ => None,
            },
            _ => None,
        })
        .map(|a| a.iter().filter_map(num).collect())
        .unwrap_or_default();
    if nums.len() == 4 {
        (nums[0], nums[1], nums[2], nums[3])
    } else {
        (0.0, 0.0, 612.0, 792.0)
    }
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
}
