//! PDFAxe icon generator — std-only, deterministic.
//!
//! Composites the brand logo (the crossed axes artwork) onto a dark rounded
//! tile at every classic icon size and packs a multi-size .ico. Large sizes
//! carry the full logo (axes + PDF wordmark); small sizes crop to the
//! crossed axe heads, which stay legible at 16 px where the wordmark can't.
//!
//! Inputs are two raw BGRA dumps (1024x1024) of the logo rendered on black
//! and on white — the alpha channel is recovered by difference matting, so
//! the neon glow keeps its softness on any background. `make-icon.ps1`
//! produces the dumps (Chrome renders the SVG) and drives this program.
//!
//! Usage: gen-icon <logo-black.bgra> <logo-white.bgra> <out.ico> [matted.bmp]

const SRC: usize = 1024;
const TILE: [f32; 3] = [0x0F as f32, 0x17 as f32, 0x2A as f32]; // slate-900

/// The crossed axe heads inside the 1024 render — safely above the wordmark.
const AXES_CROP: (f32, f32, f32, f32) = (84.0, 124.0, 944.0, 545.0);
/// Center of the crossed pair — both heads and the X of the handles, outer
/// blade tips sacrificed. Near-square, so BOTH axes (the brand) stay present
/// even on the smallest tiles.
const AXES_CENTER: (f32, f32, f32, f32) = (150.0, 126.0, 700.0, 560.0);

struct Rgba {
    px: Vec<[f32; 4]>, // straight RGBA, 0..255
}

impl Rgba {
    fn at(&self, x: usize, y: usize) -> [f32; 4] {
        self.px[y * SRC + x]
    }
}

/// Difference matting: alpha from how much the white background bleeds
/// through, color recovered from the on-black render.
fn mat(black: &[u8], white: &[u8]) -> Rgba {
    let mut px = Vec::with_capacity(SRC * SRC);
    for i in 0..SRC * SRC {
        let o = i * 4;
        // dumps are BGRA
        let b = [black[o + 2] as f32, black[o + 1] as f32, black[o] as f32];
        let w = [white[o + 2] as f32, white[o + 1] as f32, white[o] as f32];
        let a = (0..3).map(|c| 255.0 - (w[c] - b[c])).sum::<f32>() / 3.0;
        let a = a.clamp(0.0, 255.0);
        if a < 1.0 {
            px.push([0.0, 0.0, 0.0, 0.0]);
        } else {
            let s = 255.0 / a;
            px.push([
                (b[0] * s).min(255.0),
                (b[1] * s).min(255.0),
                (b[2] * s).min(255.0),
                a,
            ]);
        }
    }
    Rgba { px }
}

/// Bounding box of visible pixels (alpha above a noise floor).
fn content_box(img: &Rgba) -> (f32, f32, f32, f32) {
    let (mut x0, mut y0, mut x1, mut y1) = (SRC, SRC, 0usize, 0usize);
    for y in 0..SRC {
        for x in 0..SRC {
            if img.at(x, y)[3] > 8.0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x0 >= x1 {
        (0.0, 0.0, SRC as f32, SRC as f32)
    } else {
        (x0 as f32, y0 as f32, (x1 + 1) as f32, (y1 + 1) as f32)
    }
}

/// Area-average the source rect down to dw x dh (premultiplied accumulation
/// so transparent pixels don't tint edges).
fn resample(img: &Rgba, rect: (f32, f32, f32, f32), dw: usize, dh: usize) -> Vec<[f32; 4]> {
    let (rx0, ry0, rx1, ry1) = rect;
    let (sw, sh) = (rx1 - rx0, ry1 - ry0);
    let mut out = Vec::with_capacity(dw * dh);
    for dy in 0..dh {
        for dx in 0..dw {
            let sx0 = (rx0 + dx as f32 / dw as f32 * sw).floor().max(0.0) as usize;
            let sx1 = ((rx0 + (dx + 1) as f32 / dw as f32 * sw).ceil() as usize).min(SRC);
            let sy0 = (ry0 + dy as f32 / dh as f32 * sh).floor().max(0.0) as usize;
            let sy1 = ((ry0 + (dy + 1) as f32 / dh as f32 * sh).ceil() as usize).min(SRC);
            let (mut r, mut g, mut b, mut a, mut n) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for y in sy0..sy1.max(sy0 + 1) {
                for x in sx0..sx1.max(sx0 + 1) {
                    let p = img.at(x.min(SRC - 1), y.min(SRC - 1));
                    r += p[0] * p[3];
                    g += p[1] * p[3];
                    b += p[2] * p[3];
                    a += p[3];
                    n += 1.0;
                }
            }
            if a > 0.0 {
                out.push([r / a, g / a, b / a, a / n]);
            } else {
                out.push([0.0, 0.0, 0.0, 0.0]);
            }
        }
    }
    out
}

fn sd_round_rect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let dx = (x - cx).abs() - (hw - r);
    let dy = (y - cy).abs() - (hh - r);
    let ax = dx.max(0.0);
    let ay = dy.max(0.0);
    (ax * ax + ay * ay).sqrt() + dx.max(dy).min(0.0) - r
}

/// How the logo art is treated at a given size — fine neon detail doesn't
/// survive small rasters, so it degrades to bolder forms.
#[derive(Clone, Copy)]
enum Mode {
    /// The artwork as-is.
    Art,
    /// Alpha-gamma boost: thickens the neon glow so lines stay visible.
    ArtBoost,
    /// Solid brand-red silhouette of the shape — the 16 px survival form.
    Silhouette,
}

const BRAND_RED: [f32; 3] = [0xED as f32, 0x3C as f32, 0x26 as f32];

/// Render one icon size: dark rounded tile + the given logo crop, centered.
fn render(img: &Rgba, crop: (f32, f32, f32, f32), size: u32, pad: f32, mode: Mode) -> Vec<u8> {
    let s = size as f32;
    let (cw, ch) = (crop.2 - crop.0, crop.3 - crop.1);
    // Fit the crop into the padded content box, preserving aspect.
    let avail = 1.0 - 2.0 * pad;
    let scale = (avail / cw).min(avail / ch);
    let (lw, lh) = ((cw * scale * s) as usize, (ch * scale * s) as usize);
    // Silhouette: binarize in SOURCE space (solid art only, glow dropped) so
    // the area average yields clean shape coverage instead of halo soup.
    let solid;
    let src = match mode {
        Mode::Silhouette => {
            solid = Rgba {
                px: img
                    .px
                    .iter()
                    .map(|p| {
                        let a = if p[3] > 200.0 { 255.0 } else { 0.0 };
                        [BRAND_RED[0], BRAND_RED[1], BRAND_RED[2], a]
                    })
                    .collect(),
            };
            &solid
        }
        _ => img,
    };
    let logo = resample(src, crop, lw.max(1), lh.max(1));
    let (ox, oy) = ((size as usize - lw) / 2, (size as usize - lh) / 2);

    let aa = 1.4 / s;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size as usize {
        for px in 0..size as usize {
            let (nx, ny) = ((px as f32 + 0.5) / s, (py as f32 + 0.5) / s);
            let d = sd_round_rect(nx, ny, 0.5, 0.5, 0.48, 0.48, 0.11);
            let tile_a = (0.5 - d / aa).clamp(0.0, 1.0);
            let mut rgb = TILE;
            if px >= ox && px < ox + lw && py >= oy && py < oy + lh {
                let l = logo[(py - oy) * lw + (px - ox)];
                let (color, t) = match mode {
                    Mode::Art => ([l[0], l[1], l[2]], l[3] / 255.0),
                    // Thicker glow + a pull toward brand red so the near-black
                    // blade fill still registers against the dark tile.
                    Mode::ArtBoost => (
                        [
                            l[0] + (BRAND_RED[0] - l[0]) * 0.3,
                            l[1] + (BRAND_RED[1] - l[1]) * 0.3,
                            l[2] + (BRAND_RED[2] - l[2]) * 0.3,
                        ],
                        (l[3] / 255.0).powf(0.6),
                    ),
                    // Source alpha is already binarized; a mild gamma just
                    // firms up the anti-aliased edges.
                    Mode::Silhouette => (BRAND_RED, (l[3] / 255.0).powf(0.8)),
                };
                rgb = [
                    rgb[0] + (color[0] - rgb[0]) * t,
                    rgb[1] + (color[1] - rgb[1]) * t,
                    rgb[2] + (color[2] - rgb[2]) * t,
                ];
            }
            out.extend_from_slice(&[
                rgb[2].round() as u8,
                rgb[1].round() as u8,
                rgb[0].round() as u8,
                (tile_a * 255.0).round() as u8,
            ]);
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: gen-icon <logo-black.bgra> <logo-white.bgra> <out.ico> [matted.bmp]");
        std::process::exit(2);
    }
    let black = std::fs::read(&args[1]).expect("couldn't read black dump");
    let white = std::fs::read(&args[2]).expect("couldn't read white dump");
    assert!(black.len() >= SRC * SRC * 4 && white.len() >= SRC * SRC * 4, "dumps must be 1024x1024 BGRA");
    let img = mat(&black, &white);
    let full = content_box(&img);

    // Optional: straight-alpha BMP of the matted logo (for logo.png export).
    if let Some(bmp_path) = args.get(4) {
        let mut bmp: Vec<u8> = Vec::new();
        let data_len = (SRC * SRC * 4) as u32;
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(14u32 + 40 + data_len).to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(SRC as i32).to_le_bytes());
        bmp.extend_from_slice(&(-(SRC as i32)).to_le_bytes()); // top-down
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&32u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&data_len.to_le_bytes());
        bmp.extend_from_slice(&[0; 16]);
        for p in &img.px {
            bmp.extend_from_slice(&[p[2] as u8, p[1] as u8, p[0] as u8, p[3] as u8]);
        }
        std::fs::write(bmp_path, &bmp).expect("couldn't write matted bmp");
    }

    // Both axes at every size — they are the brand. Full logo where it fits;
    // small tiles crop to the center of the crossed pair (both heads + the
    // X of the handles) with a thickened glow.
    let sizes: [(u32, (f32, f32, f32, f32), f32, Mode); 7] = [
        (16, AXES_CENTER, 0.02, Mode::ArtBoost),
        (24, AXES_CENTER, 0.03, Mode::ArtBoost),
        (32, AXES_CENTER, 0.05, Mode::ArtBoost),
        (48, AXES_CENTER, 0.06, Mode::ArtBoost),
        (64, full, 0.08, Mode::Art),
        (128, full, 0.08, Mode::Art),
        (256, full, 0.08, Mode::Art),
    ];

    let mut dir: Vec<u8> = Vec::new();
    dir.extend_from_slice(&0u16.to_le_bytes());
    dir.extend_from_slice(&1u16.to_le_bytes());
    dir.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

    let mut entries: Vec<u8> = Vec::new();
    let mut images: Vec<u8> = Vec::new();
    let data_start = 6 + sizes.len() * 16;

    for &(size, crop, pad, mode) in &sizes {
        let bgra = render(&img, crop, size, pad, mode);
        let and_stride = ((size + 31) / 32 * 4) as usize;
        let mut ic: Vec<u8> = Vec::new();
        ic.extend_from_slice(&40u32.to_le_bytes());
        ic.extend_from_slice(&(size as i32).to_le_bytes());
        ic.extend_from_slice(&((size * 2) as i32).to_le_bytes());
        ic.extend_from_slice(&1u16.to_le_bytes());
        ic.extend_from_slice(&32u16.to_le_bytes());
        ic.extend_from_slice(&0u32.to_le_bytes());
        ic.extend_from_slice(&(size * size * 4).to_le_bytes());
        ic.extend_from_slice(&[0u8; 16]);
        let row = (size * 4) as usize;
        for y in (0..size as usize).rev() {
            ic.extend_from_slice(&bgra[y * row..(y + 1) * row]);
        }
        ic.extend(std::iter::repeat(0u8).take(and_stride * size as usize));

        entries.push(if size == 256 { 0 } else { size as u8 });
        entries.push(if size == 256 { 0 } else { size as u8 });
        entries.extend_from_slice(&[0, 0]);
        entries.extend_from_slice(&1u16.to_le_bytes());
        entries.extend_from_slice(&32u16.to_le_bytes());
        entries.extend_from_slice(&(ic.len() as u32).to_le_bytes());
        entries.extend_from_slice(&((data_start + images.len()) as u32).to_le_bytes());
        images.extend_from_slice(&ic);
    }

    dir.extend_from_slice(&entries);
    dir.extend_from_slice(&images);
    std::fs::write(&args[3], &dir).expect("couldn't write icon");
    println!("wrote {} ({} bytes, {} sizes)", args[3], dir.len(), sizes.len());
}
