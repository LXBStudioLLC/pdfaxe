//! Rendering. Backend: **hayro** — a pure-Rust PDF rasterizer (typst
//! ecosystem). The engine choice stays isolated in this module: everything
//! else in PDFAxe talks to [`render_page_rgba`] / [`render_page_png`] only,
//! so the backend can be swapped without touching callers.

use anyhow::{anyhow, Result};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::{RenderCache, RenderSettings};

/// RGBA8 bitmap of a rendered page (premultiplied alpha, row-major).
pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA, width*height*4 bytes.
    pub pixels: Vec<u8>,
    /// Page size in PDF points (pre-scale), for mapping text coordinates.
    pub point_width: f32,
    pub point_height: f32,
}

fn open_page_scale(pdf_bytes: &[u8], page_no: u32, target_width: u32) -> Result<(hayro::hayro_syntax::Pdf, f32)> {
    let pdf = hayro::hayro_syntax::Pdf::new(pdf_bytes.to_vec())
        .map_err(|e| anyhow!("renderer couldn't parse this PDF: {e:?}"))?;
    let pages_len = pdf.pages().len();
    if page_no < 1 || page_no as usize > pages_len {
        return Err(anyhow!("page {page_no} out of range (document has {pages_len} pages)"));
    }
    let (w_pt, _) = pdf.pages()[page_no as usize - 1].render_dimensions();
    let scale = if w_pt > 0.0 { target_width as f32 / w_pt } else { 1.0 };
    Ok((pdf, scale))
}

/// Render one page (1-based) at roughly `target_width` px. Premultiplied RGBA.
pub fn render_page_rgba(pdf_bytes: &[u8], page_no: u32, target_width: u32) -> Result<RenderedPage> {
    let (pdf, scale) = open_page_scale(pdf_bytes, page_no, target_width)?;
    let pages = pdf.pages();
    let page = &pages[page_no as usize - 1];
    let (point_width, point_height) = page.render_dimensions();
    let cache = RenderCache::new();
    let settings = RenderSettings {
        x_scale: scale,
        y_scale: scale,
        ..Default::default()
    };
    let pixmap = hayro::render(page, &cache, &InterpreterSettings::default(), &settings);
    Ok(RenderedPage {
        width: pixmap.width() as u32,
        height: pixmap.height() as u32,
        pixels: pixmap.data_as_u8_slice().to_vec(),
        point_width,
        point_height,
    })
}

/// Render one page to PNG bytes, composited over white paper.
pub fn render_page_png(pdf_bytes: &[u8], page_no: u32, target_width: u32) -> Result<Vec<u8>> {
    let page = render_page_rgba(pdf_bytes, page_no, target_width)?;
    // Un-premultiply and composite over white so PNGs look like paper.
    let mut rgb = Vec::with_capacity((page.width * page.height * 3) as usize);
    for px in page.pixels.chunks_exact(4) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        // premultiplied over white: out = c + (255 - a)
        rgb.push((r + (255 - a)).min(255) as u8);
        rgb.push((g + (255 - a)).min(255) as u8);
        rgb.push((b + (255 - a)).min(255) as u8);
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, page.width, page.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&rgb)?;
    }
    Ok(out)
}
