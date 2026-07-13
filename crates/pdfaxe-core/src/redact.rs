//! Redaction by raster burn: the page is rendered to pixels, the redaction
//! rectangles are burned black, and the page's content is *replaced* by that
//! bitmap. Unlike draw-a-black-box "redactions", nothing survives under the
//! ink — the covered text simply no longer exists in the file.
//!
//! The trade-off, stated honestly: the whole page becomes an image. Its text
//! is no longer selectable or extractable (that's the point for the covered
//! part, and the price for the rest of the page), and the file grows by the
//! bitmap's size. Annotations on the page are dropped too — they float above
//! page content and could otherwise carry the redacted text back in.

use anyhow::{anyhow, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use lopdf::{dictionary, Document, Object, Stream};
use std::io::Write;

use crate::ops::page_sizes;
use crate::render::render_page_rgba;

/// A redaction box in page points, origin bottom-left, as displayed.
#[derive(Clone, Copy, Debug)]
pub struct RedactRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Burn `rects` out of one page (1-based). `dpi` controls the raster
/// resolution of the replacement bitmap (200 reads well; clamped so the
/// bitmap stays between 300 and 5000 px wide).
pub fn redact_page(doc: &mut Document, page_no: u32, rects: &[RedactRect], dpi: f32) -> Result<()> {
    if rects.is_empty() {
        return Err(anyhow!("no redaction rectangles given"));
    }

    let sizes = page_sizes(doc);
    let &(pw, ph) = sizes
        .get(page_no as usize - 1)
        .ok_or_else(|| anyhow!("page {page_no} out of range"))?;
    let target = ((pw * dpi / 72.0).round() as u32).clamp(300, 5000);

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

    // Burn the boxes.
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
    // The bitmap already shows the page as displayed (rotation applied), so
    // the rebuilt page is unrotated with a fresh origin.
    let content = format!("q\n{pw} 0 0 {ph} 0 0 cm\n/PAXImg Do\nQ");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));

    let page_map = doc.get_pages();
    let &page_id = page_map.get(&page_no).ok_or_else(|| anyhow!("page {page_no} out of range"))?;
    let page = doc
        .get_object_mut(page_id)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("page dictionary unavailable: {e}"))?;
    let media: Vec<Object> = vec![0.into(), 0.into(), pw.into(), ph.into()];
    page.set("MediaBox", media.clone());
    page.set("CropBox", media);
    page.set(
        "Resources",
        dictionary! { "XObject" => dictionary! { "PAXImg" => Object::Reference(image_id) } },
    );
    page.set("Contents", Object::Reference(content_id));
    page.remove(b"Rotate");
    page.remove(b"Annots");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{extract_text, load};
    use crate::text::text_runs;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
    }

    #[test]
    fn redact_burns_text_out_of_the_file() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let runs = text_runs(&doc, 1).unwrap();
        let secret = runs.iter().find(|r| r.text.contains("SECRET")).unwrap().clone();
        let rect = RedactRect {
            x: secret.x - 2.0,
            y: secret.y - 4.0,
            w: secret.width.unwrap_or(200.0) + 6.0,
            h: secret.font_size + 10.0,
        };

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
}
