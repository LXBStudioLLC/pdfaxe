use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pdfaxe_core::lopdf::Document;
use pdfaxe_core::ops::all_pages;
use pdfaxe_core::{
    assemble, extract_text, images_to_pdf, info, load, page_numbers, parse_ranges, redact_pages,
    rotate_pages, save, strip_metadata, watermark, PageSel, RedactOpts, RedactRect, StampOpts, StripReport,
};

#[derive(Parser)]
#[command(name = "pdfaxe", version, about = "PDFAxe — local, private PDF surgery. Nothing leaves your machine.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show page count, PDF version and encryption status
    Info { file: PathBuf },
    /// Merge PDFs into one, in argument order (forms, bookmarks and attachments ride along)
    Merge {
        /// Output file
        #[arg(short, long)]
        out: PathBuf,
        /// Input files, in order
        inputs: Vec<PathBuf>,
    },
    /// Extract pages into a new PDF (e.g. --pages "1-3,5,8-")
    Extract {
        #[arg(short, long)]
        out: PathBuf,
        input: PathBuf,
        #[arg(short, long)]
        pages: String,
    },
    /// Rotate pages clockwise by 90/180/270 degrees
    Rotate {
        #[arg(short, long)]
        out: PathBuf,
        input: PathBuf,
        #[arg(short, long, value_parser = clap::value_parser!(i64))]
        degrees: i64,
        /// Pages to rotate (default: all), e.g. "2,5-7"
        #[arg(short, long)]
        pages: Option<String>,
    },
    /// Extract the text of one page (reconstruction pipeline starts here)
    Text {
        input: PathBuf,
        #[arg(short, long, default_value_t = 1)]
        page: u32,
    },
    /// Render one page — or a range of pages — to PNG (pure-Rust rasterizer)
    Render {
        input: PathBuf,
        /// Output file; with --pages it becomes a pattern (out.png -> out-3.png)
        #[arg(short, long)]
        out: PathBuf,
        #[arg(short, long, default_value_t = 1)]
        page: u32,
        /// Multiple pages, e.g. "1-3,5" or "1-" (overrides --page)
        #[arg(long)]
        pages: Option<String>,
        #[arg(short, long, default_value_t = 1200)]
        width: u32,
    },
    /// Build a PDF from images — one page per image (JPEG kept lossless)
    Images {
        #[arg(short, long)]
        out: PathBuf,
        /// PNG or JPEG files, in page order
        inputs: Vec<PathBuf>,
    },
    /// List the text runs of a page (font, size, position, editability)
    Runs {
        input: PathBuf,
        #[arg(short, long, default_value_t = 1)]
        page: u32,
    },
    /// Find & replace text in place — same font, same position
    EditText {
        input: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long)]
        find: String,
        #[arg(long)]
        replace: String,
        /// Page to edit (default: all pages)
        #[arg(short, long)]
        page: Option<u32>,
    },
    /// Stamp a translucent diagonal watermark across pages
    Watermark {
        input: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(short, long)]
        text: String,
        /// Pages to stamp (default: all), e.g. "1-3,5"
        #[arg(short, long)]
        pages: Option<String>,
        #[arg(long, default_value_t = 48.0)]
        size: f32,
        #[arg(long, default_value_t = 0.35)]
        alpha: f32,
        #[arg(long, default_value_t = 45.0)]
        angle: f32,
        #[arg(long, default_value_t = 0.6)]
        gray: f32,
    },
    /// Add page numbers at the bottom ("{page}" and "{total}" placeholders)
    PageNumbers {
        input: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(short, long, default_value = "{page} / {total}")]
        format: String,
        #[arg(long, default_value_t = 10.0)]
        size: f32,
    },
    /// Redact by raster burn — the covered content is destroyed, not hidden.
    /// The whole page becomes a bitmap; its text stops being selectable, the
    /// page's annotations, form fields and thumbnail go with it, and the
    /// original content is gone from the file — not just from the page tree.
    Redact {
        input: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        /// Page to redact (1-based)
        #[arg(short, long, conflicts_with = "pages")]
        page: Option<u32>,
        /// Several pages, e.g. "1-3,7" — the same boxes are burned on each
        #[arg(long)]
        pages: Option<String>,
        /// Box to burn, "x,y,w,h" in points from the bottom-left of the page as
        /// displayed (repeatable)
        #[arg(long = "rect", value_name = "X,Y,W,H", required = true)]
        rects: Vec<String>,
        #[arg(long, default_value_t = 200.0)]
        dpi: f32,
        /// Keep the document's Info dictionary and XMP metadata (stripped by default)
        #[arg(long)]
        keep_metadata: bool,
        /// Keep bookmarks that point at redacted pages (removed by default)
        #[arg(long)]
        keep_bookmarks: bool,
    },
    /// Strip document metadata: Info dictionary (title, author, producer…),
    /// XMP, and application private data
    Scrub {
        input: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
    },
}

fn parse_rect(s: &str) -> Result<RedactRect> {
    let parts: Vec<f32> = s.split(',').map(|p| p.trim().parse::<f32>()).collect::<Result<_, _>>()?;
    anyhow::ensure!(parts.len() == 4, "--rect wants exactly x,y,w,h (got {s:?})");
    anyhow::ensure!(parts[2] > 0.0 && parts[3] > 0.0, "--rect width/height must be positive");
    Ok(RedactRect { x: parts[0], y: parts[1], w: parts[2], h: parts[3] })
}

fn describe_strip(rep: &StripReport) -> String {
    let mut parts = Vec::new();
    if rep.info {
        parts.push("Info dictionary".to_owned());
    }
    if rep.xmp {
        parts.push("XMP metadata".to_owned());
    }
    if rep.piece_info > 0 {
        parts.push(format!("{} PieceInfo dict(s)", rep.piece_info));
    }
    if rep.page_metadata > 0 {
        parts.push(format!("{} page metadata stream(s)", rep.page_metadata));
    }
    if parts.is_empty() { "nothing to strip".to_owned() } else { parts.join(", ") }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Info { file } => {
            let doc = load(&file)?;
            let i = info(&doc);
            println!("file:      {}", file.display());
            println!("pages:     {}", i.pages);
            println!("version:   {}", i.version);
            println!("encrypted: {}", if i.encrypted { "yes" } else { "no" });
        }
        Command::Merge { out, inputs } => {
            anyhow::ensure!(inputs.len() >= 2, "need at least two input files");
            let mut parts = Vec::new();
            for path in &inputs {
                let doc = load(path)?;
                let sels = all_pages(&doc);
                parts.push((doc, sels));
            }
            let mut merged = assemble(parts)?;
            save(&mut merged, &out)?;
            println!("merged {} files -> {} ({} pages)", inputs.len(), out.display(), info(&merged).pages);
        }
        Command::Extract { out, input, pages } => {
            let doc = load(&input)?;
            let count = info(&doc).pages;
            let selected = parse_ranges(&pages, count)?;
            let sels: Vec<PageSel> = selected.iter().map(|&p| PageSel::plain(p)).collect();
            let n = sels.len();
            let mut result = assemble(vec![(doc, sels)])?;
            save(&mut result, &out)?;
            println!("extracted {n} pages -> {}", out.display());
        }
        Command::Rotate { out, input, degrees, pages } => {
            anyhow::ensure!(degrees % 90 == 0, "degrees must be a multiple of 90");
            let mut doc = load(&input)?;
            let count = info(&doc).pages;
            let selected = match pages {
                Some(expr) => parse_ranges(&expr, count)?,
                None => (1..=count).collect(),
            };
            rotate_pages(&mut doc, &selected, degrees)?;
            save(&mut doc, &out)?;
            println!("rotated {} pages by {degrees}° -> {}", selected.len(), out.display());
        }
        Command::Text { input, page } => {
            let doc = load(&input)?;
            println!("{}", extract_text(&doc, page)?);
        }
        Command::Render { input, out, page, pages, width } => {
            // One read serves both the renderer and the page count.
            let bytes = std::fs::read(&input)?;
            match pages {
                None => {
                    let png = pdfaxe_core::render::render_page_png(&bytes, page, width)?;
                    std::fs::write(&out, png)?;
                    println!("rendered page {page} at {width}px -> {}", out.display());
                }
                Some(expr) => {
                    let count = info(&Document::load_mem(&bytes)?).pages;
                    let selected = parse_ranges(&expr, count)?;
                    let stem = out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "page".into());
                    let dir = out.parent().map(PathBuf::from).unwrap_or_default();
                    for &p in &selected {
                        let png = pdfaxe_core::render::render_page_png(&bytes, p, width)?;
                        let file = dir.join(format!("{stem}-{p}.png"));
                        std::fs::write(&file, png)?;
                        println!("rendered page {p} -> {}", file.display());
                    }
                    println!("{} pages at {width}px", selected.len());
                }
            }
        }
        Command::Images { out, inputs } => {
            anyhow::ensure!(!inputs.is_empty(), "need at least one image");
            let mut doc = images_to_pdf(&inputs)?;
            save(&mut doc, &out)?;
            println!("{} image(s) -> {} ({} pages)", inputs.len(), out.display(), info(&doc).pages);
        }
        Command::Runs { input, page } => {
            let doc = load(&input)?;
            for (i, run) in pdfaxe_core::text::text_runs(&doc, page)?.iter().enumerate() {
                let state = if run.editable {
                    "editable"
                } else if run.in_form {
                    "read-only (form XObject)"
                } else {
                    "read-only"
                };
                println!(
                    "[{i:3}] {} {:>5.1}pt @({:.0},{:.0}) {} {:?}",
                    run.font_res, run.display_size, run.x, run.y, state, run.text,
                );
            }
        }
        Command::EditText { input, out, find, replace, page } => {
            let mut doc = load(&input)?;
            let pages: Vec<u32> = match page {
                Some(p) => vec![p],
                None => (1..=info(&doc).pages).collect(),
            };
            let mut total = 0;
            for p in pages {
                total += pdfaxe_core::text::find_replace(&mut doc, p, &find, &replace)?;
            }
            save(&mut doc, &out)?;
            println!("replaced {total} occurrence(s) -> {}", out.display());
        }
        Command::Watermark { input, out, text, pages, size, alpha, angle, gray } => {
            let mut doc = load(&input)?;
            let count = info(&doc).pages;
            let selected = match pages {
                Some(expr) => parse_ranges(&expr, count)?,
                None => (1..=count).collect(),
            };
            let opts = StampOpts { size, gray, alpha, angle };
            watermark(&mut doc, &selected, &text, &opts)?;
            save(&mut doc, &out)?;
            println!("watermarked {} pages -> {}", selected.len(), out.display());
        }
        Command::PageNumbers { input, out, format, size } => {
            let mut doc = load(&input)?;
            let opts = StampOpts { size, gray: 0.33, ..Default::default() };
            let total = page_numbers(&mut doc, &format, &opts)?;
            save(&mut doc, &out)?;
            println!("numbered {total} pages -> {}", out.display());
        }
        Command::Redact { input, out, page, pages, rects, dpi, keep_metadata, keep_bookmarks } => {
            let boxes: Vec<RedactRect> = rects.iter().map(|s| parse_rect(s)).collect::<Result<_>>()?;
            let mut doc = load(&input)?;
            let count = info(&doc).pages;
            let targets: Vec<u32> = match (page, pages) {
                (Some(p), _) => vec![p],
                (None, Some(expr)) => parse_ranges(&expr, count)?,
                (None, None) => anyhow::bail!("say which page to redact: --page N or --pages \"1-3\""),
            };
            let jobs: Vec<(u32, Vec<RedactRect>)> = targets.iter().map(|&p| (p, boxes.clone())).collect();
            let opts = RedactOpts { dpi, strip_metadata: !keep_metadata, prune_bookmarks: !keep_bookmarks };
            let report = redact_pages(&mut doc, &jobs, &opts)?;
            save(&mut doc, &out)?;
            let pages_txt: Vec<String> = report.pages.iter().map(|p| p.to_string()).collect();
            println!(
                "burned {} box(es) on page(s) {} -> {} (those pages are bitmaps now)",
                report.boxes,
                pages_txt.join(","),
                out.display()
            );
            if report.fields_detached > 0 {
                println!("  detached {} form field node(s) whose widgets were on the burned page(s)", report.fields_detached);
            }
            if report.structure_dropped {
                println!("  dropped the tagged-PDF structure tree (it can carry text alternates)");
            }
            if opts.prune_bookmarks {
                println!("  removed {} bookmark(s) pointing at the burned page(s)", report.bookmarks_removed);
            }
            if opts.strip_metadata {
                println!("  metadata: {}", describe_strip(&report.metadata));
            }
        }
        Command::Scrub { input, out } => {
            let mut doc = load(&input)?;
            let rep = strip_metadata(&mut doc);
            save(&mut doc, &out)?;
            println!("stripped {} -> {}", describe_strip(&rep), out.display());
        }
    }
    Ok(())
}
