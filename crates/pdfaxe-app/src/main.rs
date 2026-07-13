#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod theme;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use eframe::egui;
use pdfaxe_core::lopdf::Document;
use pdfaxe_core::render::render_page_rgba;
use pdfaxe_core::text::{edit_run, text_runs, TextRun};
use pdfaxe_core::{
    assemble, info, load, page_numbers, parse_ranges, save, watermark, PageSel, StampOpts,
};

/// Thumbnail raster width, pixels.
const THUMB_WIDTH: u32 = 150;
/// The main view re-rasterizes in steps of this many pixels as zoom/window
/// size change, so a resize drag doesn't re-render every frame.
const RENDER_STEP_PX: f32 = 300.0;
const MIN_RENDER_PX: u32 = 600;
const MAX_RENDER_PX: u32 = 3000;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1250.0, 800.0])
            .with_min_inner_size([820.0, 520.0])
            .with_title("PDFAxe — your files never leave this machine"),
        ..Default::default()
    };
    eframe::run_native(
        "PDFAxe",
        options,
        Box::new(|_cc| {
            let mut app = PdfaxeApp::default();
            let args: Vec<String> = std::env::args().skip(1).collect();
            app.dark = args.iter().any(|a| a == "--dark");
            app.edit_mode = args.iter().any(|a| a == "--edit");
            app.wm_open = args.iter().any(|a| a == "--watermark");
            app.pn_open = args.iter().any(|a| a == "--page-numbers");
            if let Some(path) = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from) {
                if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) {
                    app.open_path(path);
                }
            }
            if let Some(ix) = args.iter().position(|a| a == "--page") {
                if let Some(n) = args.get(ix + 1).and_then(|v| v.parse::<usize>().ok()) {
                    if let Some(open) = app.open.as_mut() {
                        let i = n.saturating_sub(1).min(open.rows.len().saturating_sub(1));
                        open.selected = i;
                        open.scroll_to = Some(i);
                    }
                }
            }
            Ok(Box::new(app))
        }),
    )
}

struct Row {
    src_page: u32,
    rotation: i64,
}

struct OpenDoc {
    path: PathBuf,
    /// Serialized current state — feeds the renderer; refreshed after edits.
    bytes: Vec<u8>,
    doc: Document,
    rows: Vec<Row>,
    selected: usize,
    /// Main-view texture per page, tagged with the pixel width it was rendered at.
    textures: HashMap<u32, (egui::TextureHandle, u32)>,
    thumbs: HashMap<u32, egui::TextureHandle>,
    /// Thumbnails the page list wants rendered (refilled every frame).
    thumb_wants: Vec<u32>,
    thumb_failed: HashSet<u32>,
    page_points: HashMap<u32, (f32, f32)>,
    render_errors: HashMap<u32, String>,
    runs_cache: HashMap<u32, Vec<TextRun>>,
    selected_run: Option<usize>,
    edit_buffer: String,
    edited: bool,
    /// Row the view should scroll to (set by thumbnail clicks, one frame).
    scroll_to: Option<usize>,
}

impl OpenDoc {
    fn invalidate_page(&mut self, page: u32) {
        self.textures.remove(&page);
        self.thumbs.remove(&page);
        self.thumb_failed.remove(&page);
        self.render_errors.remove(&page);
        self.runs_cache.remove(&page);
    }

    fn refresh_bytes(&mut self) {
        let mut buf = Vec::new();
        if self.doc.save_to(&mut buf).is_ok() {
            self.bytes = buf;
        }
    }

    /// Drop every cached raster/run — for operations that touch many pages.
    fn invalidate_all(&mut self) {
        self.textures.clear();
        self.thumbs.clear();
        self.thumb_failed.clear();
        self.render_errors.clear();
        self.runs_cache.clear();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Zoom {
    /// Track the viewport width (the default).
    FitWidth,
    /// Fixed scale — 1.0 shows one PDF point per egui point.
    Scale(f32),
}

const ZOOM_LEVELS: [f32; 10] = [0.5, 0.67, 0.8, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0, 4.0];

fn zoom_in(cur: f32) -> f32 {
    ZOOM_LEVELS.iter().copied().find(|&z| z > cur * 1.01).unwrap_or(4.0)
}

fn zoom_out(cur: f32) -> f32 {
    ZOOM_LEVELS.iter().rev().copied().find(|&z| z < cur * 0.99).unwrap_or(0.5)
}

struct PdfaxeApp {
    open: Option<OpenDoc>,
    status: String,
    dark: bool,
    theme_applied: Option<bool>,
    edit_mode: bool,
    zoom: Zoom,
    /// Effective scale of fit-width as of the last frame — the anchor when
    /// stepping from Fit to an explicit zoom level.
    last_fit_zoom: f32,
    /// Selection as of the last frame — the page list scrolls to follow changes.
    last_selected: Option<usize>,
    // Watermark dialog state.
    wm_open: bool,
    wm_text: String,
    wm_pages: String,
    wm_size: f32,
    wm_gray: f32,
    wm_alpha: f32,
    wm_angle: f32,
    // Page-numbers dialog state.
    pn_open: bool,
    pn_template: String,
    pn_size: f32,
    pn_gray: f32,
}

impl Default for PdfaxeApp {
    fn default() -> Self {
        Self {
            open: None,
            status: String::new(),
            dark: false,
            theme_applied: None,
            edit_mode: false,
            zoom: Zoom::FitWidth,
            last_fit_zoom: 1.0,
            last_selected: None,
            wm_open: false,
            wm_text: "CONFIDENTIAL".into(),
            wm_pages: String::new(),
            wm_size: 48.0,
            wm_gray: 0.6,
            wm_alpha: 0.35,
            wm_angle: 45.0,
            pn_open: false,
            pn_template: "{page} / {total}".into(),
            pn_size: 10.0,
            pn_gray: 0.35,
        }
    }
}

impl PdfaxeApp {
    fn open_path(&mut self, path: PathBuf) {
        let loaded = load(&path).and_then(|doc| {
            let bytes = std::fs::read(&path)?;
            Ok((doc, bytes))
        });
        match loaded {
            Ok((doc, bytes)) => {
                let pages = info(&doc).pages;
                // MediaBox dims up front so the scroll layout is stable before
                // any page has rendered.
                let page_points: HashMap<u32, (f32, f32)> = pdfaxe_core::page_sizes(&doc)
                    .into_iter()
                    .enumerate()
                    .map(|(i, wh)| (i as u32 + 1, wh))
                    .collect();
                self.status = format!("Opened {} ({pages} pages)", path.display());
                self.open = Some(OpenDoc {
                    path,
                    bytes,
                    doc,
                    rows: (1..=pages).map(|p| Row { src_page: p, rotation: 0 }).collect(),
                    selected: 0,
                    textures: HashMap::new(),
                    thumbs: HashMap::new(),
                    thumb_wants: Vec::new(),
                    thumb_failed: HashSet::new(),
                    page_points,
                    render_errors: HashMap::new(),
                    runs_cache: HashMap::new(),
                    selected_run: None,
                    edit_buffer: String::new(),
                    edited: false,
                    scroll_to: None,
                });
            }
            Err(e) => self.status = format!("Couldn't open: {e:#}"),
        }
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_file() {
            self.open_path(path);
        }
    }

    fn save_as(&mut self) {
        let Some(open) = &self.open else { return };
        if open.rows.is_empty() {
            self.status = "All pages are deleted — nothing to save.".into();
            return;
        }
        let Some(out) = rfd::FileDialog::new()
            .add_filter("PDF", &["pdf"])
            .set_file_name("pdfaxe-output.pdf")
            .save_file()
        else {
            return;
        };
        // Rebuild from the *current* (possibly text-edited) document state.
        let result = Document::load_mem(&open.bytes)
            .map_err(anyhow::Error::from)
            .and_then(|fresh| {
                let sels: Vec<PageSel> = open
                    .rows
                    .iter()
                    .map(|r| PageSel { page: r.src_page, extra_rotation: r.rotation })
                    .collect();
                assemble(vec![(fresh, sels)])
            });
        match result {
            Ok(mut doc) => match save(&mut doc, &out) {
                Ok(()) => self.status = format!("Saved {} ({} pages)", out.display(), info(&doc).pages),
                Err(e) => self.status = format!("Save failed: {e:#}"),
            },
            Err(e) => self.status = format!("Save failed: {e:#}"),
        }
    }
}

impl eframe::App for PdfaxeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.theme_applied != Some(self.dark) {
            theme::apply(&ctx, self.dark);
            self.theme_applied = Some(self.dark);
        }
        let pal = theme::palette(self.dark);

        // Drag & drop a PDF anywhere onto the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
                .collect()
        });
        if let Some(path) = dropped.into_iter().next() {
            self.open_path(path);
        }

        let card_frame = egui::Frame::new()
            .fill(pal.card_bg)
            .inner_margin(egui::Margin::symmetric(12, 8));

        egui::Panel::top("top").resizable(false).frame(card_frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::wordmark(22.0, pal.text));
                ui.add_space(10.0);
                ui.separator();
                if ui.add(theme::primary_button("Open…")).clicked() {
                    self.open_dialog();
                }
                let can_save = self.open.as_ref().is_some_and(|o| !o.rows.is_empty());
                if ui.add_enabled(can_save, egui::Button::new("Save as…")).clicked() {
                    self.save_as();
                }
                if self.open.is_some() {
                    let label = if self.edit_mode { "✏ Editing text" } else { "Edit text" };
                    if ui.selectable_label(self.edit_mode, label).clicked() {
                        self.edit_mode = !self.edit_mode;
                    }
                    if ui.button("Watermark…").clicked() {
                        self.wm_open = true;
                    }
                    if ui.button("Page numbers…").clicked() {
                        self.pn_open = true;
                    }
                    ui.separator();
                    let cur = match self.zoom {
                        Zoom::FitWidth => self.last_fit_zoom,
                        Zoom::Scale(z) => z,
                    };
                    if ui.small_button("−").on_hover_text("Zoom out (or Ctrl+scroll)").clicked() {
                        self.zoom = Zoom::Scale(zoom_out(cur));
                    }
                    let zoom_label = match self.zoom {
                        Zoom::FitWidth => format!("Fit · {:.0}%", cur * 100.0),
                        Zoom::Scale(z) => format!("{:.0}%", z * 100.0),
                    };
                    if ui
                        .selectable_label(self.zoom == Zoom::FitWidth, zoom_label)
                        .on_hover_text("Click to fit the page to the window")
                        .clicked()
                    {
                        self.zoom = Zoom::FitWidth;
                    }
                    if ui.small_button("+").on_hover_text("Zoom in (or Ctrl+scroll)").clicked() {
                        self.zoom = Zoom::Scale(zoom_in(cur));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(if self.dark { "Light mode" } else { "Dark mode" })
                        .on_hover_text("Toggle theme")
                        .clicked()
                    {
                        self.dark = !self.dark;
                    }
                    ui.label(
                        egui::RichText::new("local-only · no uploads · free forever")
                            .color(pal.text_weak),
                    );
                });
            });
        });

        egui::Panel::bottom("status").resizable(false).frame(card_frame).show(ui, |ui| {
            let text = if self.status.is_empty() {
                "Drop a PDF anywhere, or click Open…".to_owned()
            } else {
                self.status.clone()
            };
            ui.label(egui::RichText::new(text).color(pal.text_weak).small());
        });

        let Some(open) = &mut self.open else {
            egui::Frame::central_panel(ui.style()).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(150.0);
                    ui.label(theme::wordmark(34.0, pal.text));
                    ui.add_space(10.0);
                    ui.heading("The PDF app that doesn't upload your files");
                    ui.label(
                        egui::RichText::new("No accounts. No cloud. No watermarks. Free forever — in writing.")
                            .color(pal.text_weak),
                    );
                    ui.add_space(16.0);
                    if ui.add(theme::primary_button("Open a PDF…")).clicked() {
                        self.open_dialog();
                    }
                });
            });
            return;
        };

        // ----- dialogs: watermark & page numbers -----
        if self.wm_open {
            let mut keep_open = true;
            let mut apply = false;
            egui::Window::new("Watermark")
                .open(&mut keep_open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    egui::Grid::new("wm_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                        ui.label("Text");
                        ui.text_edit_singleline(&mut self.wm_text);
                        ui.end_row();
                        ui.label("Pages");
                        ui.add(egui::TextEdit::singleline(&mut self.wm_pages).hint_text("all — or e.g. 1-3,5"));
                        ui.end_row();
                    });
                    ui.add_space(4.0);
                    ui.add(egui::Slider::new(&mut self.wm_size, 12.0..=144.0).text("size (pt)"));
                    ui.add(egui::Slider::new(&mut self.wm_angle, -90.0..=90.0).text("angle (°)"));
                    ui.add(egui::Slider::new(&mut self.wm_alpha, 0.05..=1.0).text("opacity"));
                    ui.add(egui::Slider::new(&mut self.wm_gray, 0.0..=1.0).text("gray"));
                    ui.add_space(6.0);
                    apply = ui.add(theme::primary_button("Stamp watermark")).clicked();
                });
            self.wm_open = keep_open;
            if apply && !self.wm_text.trim().is_empty() {
                let total = info(&open.doc).pages;
                let pages: anyhow::Result<Vec<u32>> = if self.wm_pages.trim().is_empty() {
                    Ok((1..=total).collect())
                } else {
                    parse_ranges(&self.wm_pages, total)
                };
                let opts = StampOpts {
                    size: self.wm_size,
                    gray: self.wm_gray,
                    alpha: self.wm_alpha,
                    angle: self.wm_angle,
                };
                let stamped = pages.and_then(|p| {
                    watermark(&mut open.doc, &p, &self.wm_text, &opts)?;
                    Ok(p.len())
                });
                match stamped {
                    Ok(n) => {
                        open.refresh_bytes();
                        open.invalidate_all();
                        open.edited = true;
                        self.wm_open = false;
                        self.status = format!("Watermarked {n} page(s)");
                    }
                    Err(e) => self.status = format!("Watermark failed: {e:#}"),
                }
            }
        }
        if self.pn_open {
            let mut keep_open = true;
            let mut apply = false;
            egui::Window::new("Page numbers")
                .open(&mut keep_open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Format");
                        ui.text_edit_singleline(&mut self.pn_template);
                    });
                    ui.label(
                        egui::RichText::new("{page} = page number · {total} = page count")
                            .color(pal.text_weak)
                            .small(),
                    );
                    ui.add_space(4.0);
                    ui.add(egui::Slider::new(&mut self.pn_size, 6.0..=24.0).text("size (pt)"));
                    ui.add(egui::Slider::new(&mut self.pn_gray, 0.0..=1.0).text("gray"));
                    ui.add_space(6.0);
                    apply = ui.add(theme::primary_button("Stamp page numbers")).clicked();
                });
            self.pn_open = keep_open;
            if apply && !self.pn_template.trim().is_empty() {
                let opts = StampOpts { size: self.pn_size, gray: self.pn_gray, ..Default::default() };
                match page_numbers(&mut open.doc, &self.pn_template, &opts) {
                    Ok(n) => {
                        open.refresh_bytes();
                        open.invalidate_all();
                        open.edited = true;
                        self.pn_open = false;
                        self.status = format!("Numbered {n} pages (bottom center)");
                    }
                    Err(e) => self.status = format!("Page numbers failed: {e:#}"),
                }
            }
        }

        // Rasterize at most a couple of queued thumbnails per frame so large
        // documents never freeze the UI; the page list re-queues what's missing.
        let wants = std::mem::take(&mut open.thumb_wants);
        let mut thumb_budget = 2usize;
        for tp in wants {
            if open.thumbs.contains_key(&tp) || open.thumb_failed.contains(&tp) || thumb_budget == 0 {
                continue;
            }
            thumb_budget -= 1;
            match render_page_rgba(&open.bytes, tp, THUMB_WIDTH) {
                Ok(r) => {
                    let image = egui::ColorImage::from_rgba_premultiplied(
                        [r.width as usize, r.height as usize],
                        &r.pixels,
                    );
                    let tex = ctx.load_texture(format!("thumb-{tp}"), image, egui::TextureOptions::LINEAR);
                    open.thumbs.insert(tp, tex);
                    open.page_points.entry(tp).or_insert((r.point_width, r.point_height));
                }
                Err(_) => {
                    open.thumb_failed.insert(tp);
                }
            }
        }

        // ----- left: page list -----
        let mut move_up: Option<usize> = None;
        let mut move_down: Option<usize> = None;
        let mut rotate: Option<usize> = None;
        let mut delete: Option<usize> = None;
        let mut want_thumbs: Vec<u32> = Vec::new();

        egui::Panel::left("pages")
            .resizable(true)
            .default_size(240.0)
            .frame(card_frame)
            .show(ui, |ui| {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(format!("{} PAGES", open.rows.len())).color(pal.text_weak).small().strong());
                ui.add_space(4.0);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for i in 0..open.rows.len() {
                        let row = &open.rows[i];
                        let page = row.src_page;
                        let is_sel = open.selected == i;
                        let card = egui::Frame::new()
                            .fill(if is_sel { pal.selected_bg } else { egui::Color32::TRANSPARENT })
                            .corner_radius(egui::CornerRadius::same(6))
                            .inner_margin(egui::Margin::same(6));
                        let card_resp = card.show(ui, |ui| {
                            let thumb_w = ui.available_width().clamp(40.0, 150.0);
                            let (pw, ph) = open.page_points.get(&page).copied().unwrap_or((595.0, 842.0));
                            let size = egui::vec2(thumb_w, thumb_w * ph / pw.max(1.0));
                            let clicked = if let Some(tex) = open.thumbs.get(&page) {
                                let resp = ui.add(
                                    egui::Image::new(tex)
                                        .fit_to_exact_size(size)
                                        .corner_radius(2.0)
                                        .sense(egui::Sense::click()),
                                );
                                let stroke = if is_sel {
                                    egui::Stroke::new(2.0, theme::AXE_ORANGE)
                                } else {
                                    egui::Stroke::new(1.0, pal.border)
                                };
                                ui.painter().rect_stroke(resp.rect, 2.0, stroke, egui::StrokeKind::Outside);
                                resp.clicked()
                            } else {
                                let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                                if ui.is_rect_visible(rect) {
                                    ui.painter().rect_filled(rect, 2.0, pal.page_bg);
                                    ui.painter().rect_stroke(
                                        rect,
                                        2.0,
                                        egui::Stroke::new(1.0, pal.border),
                                        egui::StrokeKind::Outside,
                                    );
                                    ui.painter().text(
                                        rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        format!("page {page}"),
                                        egui::FontId::proportional(11.0),
                                        pal.text_weak,
                                    );
                                    if !open.thumb_failed.contains(&page) {
                                        want_thumbs.push(page);
                                    }
                                }
                                resp.clicked()
                            };
                            if clicked {
                                open.selected = i;
                                open.selected_run = None;
                                open.scroll_to = Some(i);
                            }
                            let label = if row.rotation != 0 {
                                format!("#{} · p{} · +{}° on save", i + 1, page, row.rotation)
                            } else {
                                format!("#{} · p{}", i + 1, page)
                            };
                            ui.label(
                                egui::RichText::new(label)
                                    .small()
                                    .color(if is_sel { pal.text } else { pal.text_weak }),
                            );
                            ui.horizontal(|ui| {
                                if ui.small_button("^").on_hover_text("Move up").clicked() {
                                    move_up = Some(i);
                                }
                                if ui.small_button("v").on_hover_text("Move down").clicked() {
                                    move_down = Some(i);
                                }
                                if ui.small_button("R").on_hover_text("Rotate 90° (applied on save)").clicked() {
                                    rotate = Some(i);
                                }
                                if ui.small_button("X").on_hover_text("Delete").clicked() {
                                    delete = Some(i);
                                }
                            });
                        });
                        // Follow selection changes (scroll tracking, --page).
                        if is_sel && self.last_selected != Some(i) {
                            ui.scroll_to_rect(card_resp.response.rect, Some(egui::Align::Center));
                        }
                        ui.add_space(4.0);
                    }
                });
            });

        open.thumb_wants = want_thumbs;
        if !open.thumb_wants.is_empty() {
            // Thumbnails still pending — keep frames coming until they're in.
            ctx.request_repaint();
        }

        if let Some(i) = move_up {
            if i > 0 {
                open.rows.swap(i, i - 1);
                open.selected = i - 1;
            }
        }
        if let Some(i) = move_down {
            if i + 1 < open.rows.len() {
                open.rows.swap(i, i + 1);
                open.selected = i + 1;
            }
        }
        if let Some(i) = rotate {
            open.rows[i].rotation = (open.rows[i].rotation + 90) % 360;
        }
        if let Some(i) = delete {
            open.rows.remove(i);
            if open.selected >= open.rows.len() && open.selected > 0 {
                open.selected = open.rows.len() - 1;
            }
        }

        let Some(current_row) = open.rows.get(open.selected) else {
            return;
        };
        let page = current_row.src_page;

        // ----- right: text editor -----
        if self.edit_mode {
            if !open.runs_cache.contains_key(&page) {
                match text_runs(&open.doc, page) {
                    Ok(runs) => {
                        open.runs_cache.insert(page, runs);
                    }
                    Err(e) => {
                        open.runs_cache.insert(page, Vec::new());
                        self.status = format!("Couldn't read text: {e:#}");
                    }
                }
            }
            let mut apply_edit: Option<TextRun> = None;

            egui::Panel::right("editor")
                .resizable(true)
                .default_size(320.0)
                .frame(card_frame)
                .show(ui, |ui| {
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("TEXT ON THIS PAGE").color(pal.text_weak).small().strong());
                    ui.label(
                        egui::RichText::new("Same font · the line reflows to fit. Paragraph re-wrap is on the roadmap.")
                            .color(pal.text_weak)
                            .small(),
                    );
                    ui.add_space(6.0);

                    let runs = open.runs_cache.get(&page).cloned().unwrap_or_default();
                    egui::ScrollArea::vertical().max_height(ui.available_height() * 0.55).show(ui, |ui| {
                        for (i, run) in runs.iter().enumerate() {
                            let mut label = run.text.clone();
                            if label.len() > 42 {
                                label.truncate(42);
                                label.push('…');
                            }
                            if label.trim().is_empty() {
                                label = "(spaces)".into();
                            }
                            if !run.editable {
                                label = format!("🔒 {label}");
                            }
                            if ui.selectable_label(open.selected_run == Some(i), label).clicked() {
                                open.selected_run = Some(i);
                                open.edit_buffer = run.text.clone();
                            }
                        }
                        if runs.is_empty() {
                            ui.label(egui::RichText::new("(no text on this page)").color(pal.text_weak));
                        }
                    });

                    ui.separator();
                    if let Some(ri) = open.selected_run {
                        if let Some(run) = runs.get(ri) {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} · {:.0}pt {}",
                                    run.font_res,
                                    run.font_size,
                                    if run.editable { "" } else { "· read-only (complex font)" }
                                ))
                                .color(pal.text_weak)
                                .small(),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut open.edit_buffer)
                                    .desired_width(f32::INFINITY)
                                    .interactive(run.editable),
                            );
                            ui.add_space(4.0);
                            if run.editable
                                && ui.add(theme::primary_button("Apply edit")).clicked()
                                && open.edit_buffer != run.text
                            {
                                apply_edit = Some(run.clone());
                            }
                        }
                    } else {
                        ui.label(
                            egui::RichText::new("Select a line here or click text on the page.")
                                .color(pal.text_weak),
                        );
                    }
                });

            if let Some(run) = apply_edit {
                match edit_run(&mut open.doc, page, &run, &open.edit_buffer) {
                    Ok(()) => {
                        open.refresh_bytes();
                        open.invalidate_page(page);
                        open.selected_run = None;
                        open.edited = true;
                        self.status = "Edited — rendering the result…".into();
                    }
                    Err(e) => self.status = format!("Edit failed: {e:#}"),
                }
            }
        }

        // ----- center: continuous scroll over every page -----
        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            let file_name = open
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui.label(
                egui::RichText::new(format!(
                    "{file_name}{} · PDF {} · {} pages · page {page}",
                    if open.edited { " (edited)" } else { "" },
                    info(&open.doc).version,
                    open.rows.len(),
                ))
                .color(pal.text_weak)
                .small(),
            );
            ui.add_space(4.0);

            // Ctrl+scroll (or pinch) zooms the page view.
            if ui.ui_contains_pointer() {
                let zd = ctx.input(|i| i.zoom_delta());
                if (zd - 1.0).abs() > 0.001 {
                    let cur = match self.zoom {
                        Zoom::FitWidth => self.last_fit_zoom,
                        Zoom::Scale(z) => z,
                    };
                    self.zoom = Zoom::Scale((cur * zd).clamp(0.25, 5.0));
                }
            }

            // Leave room for the vertical scrollbar so fit-width never
            // triggers a horizontal one.
            let avail = (ui.available_width() - 14.0).max(200.0);
            let (pw_sel, _) = open.page_points.get(&page).copied().unwrap_or((595.0, 842.0));
            self.last_fit_zoom = avail / pw_sel.max(1.0);

            // At most one page rasterization per frame keeps scrolling fluid;
            // pending pages request another frame.
            let mut render_budget = 1usize;
            let mut pending_render = false;
            let mut keep_textures: HashSet<u32> = HashSet::new();
            let mut topmost: Option<usize> = None;
            let scroll_to = open.scroll_to.take();

            egui::ScrollArea::both().show(ui, |ui| {
                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                    let clip = ui.clip_rect();
                    // Pre-render one viewport-height around the visible band.
                    let near = clip.expand2(egui::vec2(0.0, clip.height().max(400.0)));

                    for i in 0..open.rows.len() {
                        let pg = open.rows[i].src_page;
                        let (pw, ph) = open.page_points.get(&pg).copied().unwrap_or((595.0, 842.0));
                        let display_w = match self.zoom {
                            Zoom::FitWidth => avail,
                            Zoom::Scale(z) => pw * z,
                        };
                        let size = egui::vec2(display_w, display_w * ph / pw.max(1.0));
                        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

                        if scroll_to == Some(i) {
                            ui.scroll_to_rect(rect, Some(egui::Align::Min));
                        }
                        if topmost.is_none() && rect.bottom() > clip.top() + 4.0 {
                            topmost = Some(i);
                        }
                        if !rect.intersects(near) {
                            continue;
                        }
                        keep_textures.insert(pg);

                        let want_px = (((display_w * ctx.pixels_per_point() / RENDER_STEP_PX).ceil()
                            * RENDER_STEP_PX) as u32)
                            .clamp(MIN_RENDER_PX, MAX_RENDER_PX);
                        let has_current = open.textures.get(&pg).is_some_and(|(_, w)| *w == want_px);
                        if !has_current && !open.render_errors.contains_key(&pg) {
                            if render_budget > 0 {
                                render_budget -= 1;
                                match render_page_rgba(&open.bytes, pg, want_px) {
                                    Ok(r) => {
                                        let image = egui::ColorImage::from_rgba_premultiplied(
                                            [r.width as usize, r.height as usize],
                                            &r.pixels,
                                        );
                                        let tex = ctx.load_texture(
                                            format!("page-{pg}"),
                                            image,
                                            egui::TextureOptions::LINEAR,
                                        );
                                        open.textures.insert(pg, (tex, want_px));
                                        open.page_points.insert(pg, (r.point_width, r.point_height));
                                    }
                                    Err(e) => {
                                        open.render_errors.insert(pg, format!("{e:#}"));
                                    }
                                }
                            } else {
                                pending_render = true;
                            }
                        }

                        if let Some((tex, _)) = open.textures.get(&pg) {
                            egui::Image::new(tex).corner_radius(3.0).paint_at(ui, rect);
                        } else if let Some(err) = open.render_errors.get(&pg) {
                            ui.painter().rect_filled(rect, 3.0, pal.page_bg);
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                format!("Couldn't render p{pg}: {err}"),
                                egui::FontId::proportional(12.0),
                                egui::Color32::from_rgb(0xB9, 0x1C, 0x1C),
                            );
                        } else {
                            ui.painter().rect_filled(rect, 3.0, pal.page_bg);
                            ui.painter().rect_stroke(
                                rect,
                                3.0,
                                egui::Stroke::new(1.0, pal.border),
                                egui::StrokeKind::Outside,
                            );
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                format!("page {pg}"),
                                egui::FontId::proportional(12.0),
                                pal.text_weak,
                            );
                        }

                        // Click text on any page to select it for editing.
                        let s = rect.width() / pw.max(1.0);
                        if self.edit_mode {
                            if let (true, Some(pos)) = (response.clicked(), response.interact_pointer_pos()) {
                                let px = (pos.x - rect.min.x) / s;
                                let py = ph - (pos.y - rect.min.y) / s;
                                if !open.runs_cache.contains_key(&pg) {
                                    let runs = text_runs(&open.doc, pg).unwrap_or_default();
                                    open.runs_cache.insert(pg, runs);
                                }
                                if let Some(runs) = open.runs_cache.get(&pg) {
                                    let mut best: Option<(usize, f32)> = None;
                                    for (ri, run) in runs.iter().enumerate() {
                                        let width_est = run
                                            .width
                                            .unwrap_or_else(|| run.text.chars().count() as f32 * run.font_size * 0.55);
                                        let within_x = px >= run.x - 4.0 && px <= run.x + width_est + 8.0;
                                        let dy = (py - run.y - run.font_size * 0.3).abs();
                                        if within_x && dy < run.font_size * 1.2 {
                                            if best.is_none_or(|(_, d)| dy < d) {
                                                best = Some((ri, dy));
                                            }
                                        }
                                    }
                                    if let Some((ri, _)) = best {
                                        open.selected = i;
                                        open.selected_run = Some(ri);
                                        open.edit_buffer = runs[ri].text.clone();
                                    }
                                }
                            }

                            // Highlight the selected run on its page.
                            if i == open.selected {
                                if let Some(ri) = open.selected_run {
                                    if let Some(run) = open.runs_cache.get(&pg).and_then(|r| r.get(ri)) {
                                        let width_est = run
                                            .width
                                            .unwrap_or_else(|| run.text.chars().count() as f32 * run.font_size * 0.55)
                                            .max(12.0);
                                        let h = run.font_size * 1.3;
                                        let min = egui::pos2(
                                            rect.min.x + run.x * s,
                                            rect.min.y + (ph - run.y - run.font_size) * s,
                                        );
                                        let hi_rect = egui::Rect::from_min_size(
                                            min,
                                            egui::vec2(width_est * s, h * s),
                                        );
                                        ui.painter().rect_stroke(
                                            hi_rect,
                                            2.0,
                                            egui::Stroke::new(2.0, theme::AXE_ORANGE_BRIGHT),
                                            egui::StrokeKind::Outside,
                                        );
                                    }
                                }
                            }
                        }
                    }
                });
            });

            // Free textures far from the viewport so huge documents stay lean.
            open.textures.retain(|p, _| keep_textures.contains(p));
            if pending_render {
                ctx.request_repaint();
            }
            // Outside edit mode the page list follows the scroll position.
            if scroll_to.is_none() && !self.edit_mode {
                if let Some(t) = topmost {
                    open.selected = t;
                }
            }
        });

        self.last_selected = self.open.as_ref().map(|o| o.selected);
    }
}
