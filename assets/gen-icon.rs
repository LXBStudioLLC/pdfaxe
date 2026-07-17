//! PDFAxe icon generator — std-only, deterministic.
//!
//! Renders the mark (white page, slate fold, orange axe-cut slash on a
//! slate-900 rounded tile) at every classic icon size with 4x supersampling
//! and packs them into a multi-size .ico (uncompressed 32-bit BGRA entries).
//!
//! Regenerate with:
//!   rustc -O assets/gen-icon.rs -o %TEMP%/gen-icon.exe && %TEMP%/gen-icon.exe assets/pdfaxe.ico

const BG: [f32; 3] = [0x0F as f32, 0x17 as f32, 0x2A as f32]; // slate-900
const PAGE: [f32; 3] = [0xF8 as f32, 0xFA as f32, 0xFC as f32]; // slate-50
const FOLD: [f32; 3] = [0x94 as f32, 0xA3 as f32, 0xB8 as f32]; // slate-400
const RULE: [f32; 3] = [0xCB as f32, 0xD5 as f32, 0xE1 as f32]; // slate-300
const AXE: [f32; 3] = [0xEA as f32, 0x58 as f32, 0x0C as f32]; // axe-600

/// Signed distance to a rounded rectangle centered at (cx, cy).
fn sd_round_rect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let dx = (x - cx).abs() - (hw - r);
    let dy = (y - cy).abs() - (hh - r);
    let ax = dx.max(0.0);
    let ay = dy.max(0.0);
    (ax * ax + ay * ay).sqrt() + dx.max(dy).min(0.0) - r
}

/// Signed distance to an infinite band of half-width `hw` through (px, py)
/// with direction (dx, dy) (normalized).
fn sd_band(x: f32, y: f32, px: f32, py: f32, dx: f32, dy: f32, hw: f32) -> f32 {
    // perpendicular distance to the center line
    ((x - px) * dy - (y - py) * dx).abs() - hw
}

/// 0..1 coverage from a signed distance, with `aa` feather (in the same
/// normalized units).
fn cov(d: f32, aa: f32) -> f32 {
    (0.5 - d / aa).clamp(0.0, 1.0)
}

/// Shade one normalized point (0..1, y down). Returns straight RGBA 0..255.
fn shade(x: f32, y: f32, aa: f32) -> [f32; 4] {
    // Tile: full-bleed rounded square.
    let tile = cov(sd_round_rect(x, y, 0.5, 0.5, 0.48, 0.48, 0.11), aa);
    if tile <= 0.0 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let mut rgb = BG;

    // The axe cut runs diagonally (up-right) through (ccx, ccy); the page is
    // cleaved along it, each half pushed apart along the cut's normal.
    let inv = 1.0 / (2.0f32).sqrt();
    let (ccx, ccy) = (0.5, 0.60);
    let side = ((x - ccx) * inv + (y - ccy) * inv).signum();
    let shift = 0.026;
    let (sx, sy) = (x - side * shift * inv, y - side * shift * inv);

    // Page group (page, dog-ear fold, text rules), sampled at the shifted
    // point so the two halves land offset — visibly split paper.
    let (px0, py0, px1, py1) = (0.26, 0.17, 0.74, 0.83);
    let (pcx, pcy) = ((px0 + px1) / 2.0, (py0 + py1) / 2.0);
    let (phw, phh) = ((px1 - px0) / 2.0, (py1 - py0) / 2.0);
    let page = cov(sd_round_rect(sx, sy, pcx, pcy, phw, phh, 0.03), aa);
    // Dog-ear: corner cut along the line through (px1-ear, py0), (px1, py0+ear).
    let ear = 0.15;
    let f = (sx - px1 + ear) - (sy - py0); // > 0 beyond the fold line
    let page_c = page * cov(f, aa);
    rgb = mix(rgb, PAGE, page_c);
    // Fold flap: the mirrored corner triangle inside the cut.
    let fold = cov(-f, aa) * cov((px1 - ear) - sx, aa) * cov(sy - (py0 + ear), aa) * page;
    rgb = mix(rgb, FOLD, fold);

    // Text rules.
    for (ry, rw) in [(0.36, 0.28), (0.47, 0.32), (0.70, 0.24)] {
        let bar = cov(sd_round_rect(sx, sy, px0 + 0.08 + rw / 2.0, ry, rw / 2.0, 0.024, 0.02), aa);
        rgb = mix(rgb, RULE, bar * page_c);
    }

    // The cut itself: an orange blade-line over the gap, clipped to the page
    // area (grown a little so it bleeds past the paper edge).
    let grow = shift + 0.03;
    let page_zone = cov(sd_round_rect(x, y, pcx, pcy, phw + grow, phh + grow, 0.04), aa);
    let hw = if 0.8 * aa > 0.038 { 0.8 * aa } else { 0.038 }; // keep visible at 16px
    let band = cov(sd_band(x, y, ccx, ccy, inv, -inv, hw), aa) * page_zone;
    rgb = mix(rgb, AXE, band);

    [rgb[0], rgb[1], rgb[2], tile * 255.0]
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Render one size, straight BGRA rows top-down.
fn render(size: u32) -> Vec<u8> {
    let s = size as f32;
    let aa = 1.6 / s; // feather ~1.6px in normalized units
    let ss = 4; // supersamples per axis
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..ss {
                for sx in 0..ss {
                    let x = (px as f32 + (sx as f32 + 0.5) / ss as f32) / s;
                    let y = (py as f32 + (sy as f32 + 0.5) / ss as f32) / s;
                    let c = shade(x, y, aa);
                    r += c[0];
                    g += c[1];
                    b += c[2];
                    a += c[3];
                }
            }
            let n = (ss * ss) as f32;
            out.extend_from_slice(&[
                (b / n).round() as u8,
                (g / n).round() as u8,
                (r / n).round() as u8,
                (a / n).round() as u8,
            ]);
        }
    }
    out
}

fn main() {
    let out_path = std::env::args().nth(1).unwrap_or_else(|| "pdfaxe.ico".into());
    let sizes: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

    let mut dir: Vec<u8> = Vec::new();
    dir.extend_from_slice(&0u16.to_le_bytes()); // reserved
    dir.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    dir.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

    let mut entries: Vec<u8> = Vec::new();
    let mut images: Vec<u8> = Vec::new();
    let data_start = 6 + sizes.len() * 16;

    for &size in &sizes {
        let bgra = render(size);
        // BITMAPINFOHEADER + XOR (bottom-up) + AND mask (1bpp, zeroed).
        let and_stride = ((size + 31) / 32 * 4) as usize;
        let mut img: Vec<u8> = Vec::new();
        img.extend_from_slice(&40u32.to_le_bytes());
        img.extend_from_slice(&(size as i32).to_le_bytes());
        img.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // XOR + AND
        img.extend_from_slice(&1u16.to_le_bytes());
        img.extend_from_slice(&32u16.to_le_bytes());
        img.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        img.extend_from_slice(&(size * size * 4).to_le_bytes());
        img.extend_from_slice(&[0u8; 16]); // ppm x/y, colors used/important
        let row = (size * 4) as usize;
        for y in (0..size as usize).rev() {
            img.extend_from_slice(&bgra[y * row..(y + 1) * row]);
        }
        img.extend(std::iter::repeat(0u8).take(and_stride * size as usize));

        entries.push(if size == 256 { 0 } else { size as u8 });
        entries.push(if size == 256 { 0 } else { size as u8 });
        entries.extend_from_slice(&[0, 0]); // colors, reserved
        entries.extend_from_slice(&1u16.to_le_bytes());
        entries.extend_from_slice(&32u16.to_le_bytes());
        entries.extend_from_slice(&(img.len() as u32).to_le_bytes());
        entries.extend_from_slice(&((data_start + images.len()) as u32).to_le_bytes());
        images.extend_from_slice(&img);
    }

    dir.extend_from_slice(&entries);
    dir.extend_from_slice(&images);
    std::fs::write(&out_path, &dir).expect("couldn't write icon");
    println!("wrote {out_path} ({} bytes, {} sizes)", dir.len(), sizes.len());
}
