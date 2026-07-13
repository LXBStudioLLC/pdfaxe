//! Images → PDF: build a document with one page per image, each page exactly
//! the image's pixel size in points (72 dpi). JPEGs embed as-is (DCTDecode —
//! no recompression, no quality loss); PNGs decode and re-embed as Flate RGB,
//! alpha composited over white.

use anyhow::{anyhow, Context, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use lopdf::{dictionary, Document, Object, Stream};
use std::io::Write;
use std::path::Path;

struct PdfImage {
    width: u32,
    height: u32,
    /// (filter, colorspace, payload)
    filter: &'static str,
    colorspace: &'static str,
    data: Vec<u8>,
}

/// Parse a JPEG's SOF header for dimensions and component count.
fn jpeg_info(bytes: &[u8]) -> Option<(u32, u32, u8)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    while pos + 3 < bytes.len() {
        if bytes[pos] != 0xFF {
            return None;
        }
        let marker = bytes[pos + 1];
        match marker {
            // Standalone markers without a length field.
            0x01 | 0xD0..=0xD7 => {
                pos += 2;
                continue;
            }
            0xD9 => return None, // EOI before SOF
            _ => {}
        }
        let len = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
        if matches!(marker, 0xC0 | 0xC1 | 0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            if pos + 9 >= bytes.len() {
                return None;
            }
            let height = u16::from_be_bytes([bytes[pos + 5], bytes[pos + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[pos + 7], bytes[pos + 8]]) as u32;
            let ncomp = bytes[pos + 9];
            return Some((width, height, ncomp));
        }
        pos += 2 + len;
    }
    None
}

fn load_image(path: &Path) -> Result<PdfImage> {
    let bytes = std::fs::read(path).with_context(|| format!("couldn't read {}", path.display()))?;

    if let Some((width, height, ncomp)) = jpeg_info(&bytes) {
        let colorspace = match ncomp {
            1 => "DeviceGray",
            3 => "DeviceRGB",
            4 => "DeviceCMYK",
            n => return Err(anyhow!("unsupported JPEG component count {n} in {}", path.display())),
        };
        return Ok(PdfImage { width, height, filter: "DCTDecode", colorspace, data: bytes });
    }

    // Not a JPEG: try PNG.
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes.as_slice()));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| anyhow!("{} is neither a JPEG nor a readable PNG: {e}", path.display()))?;
    let buf_size = reader
        .output_buffer_size()
        .ok_or_else(|| anyhow!("PNG dimensions overflow in {}", path.display()))?;
    let mut buf = vec![0u8; buf_size];
    let info = reader.next_frame(&mut buf).map_err(|e| anyhow!("couldn't decode {}: {e}", path.display()))?;
    buf.truncate(info.buffer_size());

    // Normalize every variant to RGB, alpha composited over white.
    let rgb: Vec<u8> = match info.color_type {
        png::ColorType::Rgb => buf,
        png::ColorType::Rgba => buf
            .chunks_exact(4)
            .flat_map(|p| {
                let a = p[3] as u32;
                [
                    ((p[0] as u32 * a + 255 * (255 - a)) / 255) as u8,
                    ((p[1] as u32 * a + 255 * (255 - a)) / 255) as u8,
                    ((p[2] as u32 * a + 255 * (255 - a)) / 255) as u8,
                ]
            })
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| {
                let a = p[1] as u32;
                let g = ((p[0] as u32 * a + 255 * (255 - a)) / 255) as u8;
                [g, g, g]
            })
            .collect(),
        other => return Err(anyhow!("unsupported PNG color type {other:?} in {}", path.display())),
    };

    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&rgb)?;
    Ok(PdfImage {
        width: info.width,
        height: info.height,
        filter: "FlateDecode",
        colorspace: "DeviceRGB",
        data: enc.finish()?,
    })
}

/// Build a PDF with one page per image file, each page the image's pixel size
/// in points.
pub fn images_to_pdf(paths: &[impl AsRef<Path>]) -> Result<Document> {
    if paths.is_empty() {
        return Err(anyhow!("no images given"));
    }
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::with_capacity(paths.len());

    for path in paths {
        let img = load_image(path.as_ref())?;
        let (w, h) = (img.width as i64, img.height as i64);
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => w,
                "Height" => h,
                "ColorSpace" => img.colorspace,
                "BitsPerComponent" => 8,
                "Filter" => img.filter,
            },
            img.data,
        ));
        let content = format!("q\n{w} 0 0 {h} 0 0 cm\n/PAXImg Do\nQ");
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "PAXImg" => Object::Reference(image_id) },
            },
            "Contents" => Object::Reference(content_id),
        });
        kids.push(Object::Reference(page_id));
    }

    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => count,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", catalog_id);
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png(rgba: [u8; 4], w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            let data: Vec<u8> = (0..w * h).flat_map(|_| rgba).collect();
            writer.write_image_data(&data).unwrap();
        }
        out
    }

    #[test]
    fn png_becomes_a_page_and_renders_back() {
        let dir = std::env::temp_dir().join("pdfaxe-img-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("red.png");
        let p2 = dir.join("blue.png");
        std::fs::write(&p1, tiny_png([200, 10, 10, 255], 40, 30)).unwrap();
        std::fs::write(&p2, tiny_png([10, 10, 200, 255], 20, 50)).unwrap();

        let mut doc = images_to_pdf(&[&p1, &p2]).unwrap();
        assert_eq!(crate::ops::info(&doc).pages, 2);
        let sizes = crate::ops::page_sizes(&doc);
        assert_eq!(sizes[0], (40.0, 30.0));
        assert_eq!(sizes[1], (20.0, 50.0));

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let img = crate::render::render_page_rgba(&buf, 1, 400).unwrap();
        let o = ((img.height / 2) * img.width + img.width / 2) as usize * 4;
        let (r, g, b) = (img.pixels[o], img.pixels[o + 1], img.pixels[o + 2]);
        assert!(r > 150 && g < 60 && b < 60, "page 1 should render red, got {r},{g},{b}");
    }

    #[test]
    fn jpeg_header_parses() {
        // Fabricated minimal stream: SOI, APP0 (skipped), SOF0 with 120x80, 3 components.
        let mut j = vec![0xFF, 0xD8];
        j.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]); // APP0 len 4
        j.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x50, 0x00, 0x78, 0x03, 0x00]);
        assert_eq!(jpeg_info(&j), Some((120, 80, 3)));
        assert_eq!(jpeg_info(b"not a jpeg"), None);
    }
}
