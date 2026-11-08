//! PDFAxe visual theme — a native clone of the website's design language:
//! slate neutrals, orange "axe" accent, white cards on a soft page background.
//! Light is the default; dark mode mirrors the same hierarchy.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

pub const AXE_ORANGE: Color32 = Color32::from_rgb(0xEA, 0x58, 0x0C); // axe-600
pub const AXE_ORANGE_BRIGHT: Color32 = Color32::from_rgb(0xF9, 0x73, 0x16); // axe-500
pub const AXE_ORANGE_DEEP: Color32 = Color32::from_rgb(0xC2, 0x41, 0x0C); // axe-700

pub struct Palette {
    pub page_bg: Color32,
    pub card_bg: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_weak: Color32,
    pub hover_bg: Color32,
    pub selected_bg: Color32,
    pub field_bg: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            page_bg: Color32::from_rgb(0x0F, 0x17, 0x2A),  // slate-900
            card_bg: Color32::from_rgb(0x1E, 0x29, 0x3B),  // slate-800
            border: Color32::from_rgb(0x33, 0x41, 0x55),   // slate-700
            text: Color32::from_rgb(0xF1, 0xF5, 0xF9),     // slate-100
            text_weak: Color32::from_rgb(0x94, 0xA3, 0xB8), // slate-400
            hover_bg: Color32::from_rgb(0x33, 0x41, 0x55),
            selected_bg: Color32::from_rgb(0x7C, 0x2D, 0x12), // orange-900
            field_bg: Color32::from_rgb(0x0B, 0x12, 0x20),
        }
    } else {
        Palette {
            page_bg: Color32::from_rgb(0xF8, 0xFA, 0xFC),  // slate-50
            card_bg: Color32::WHITE,
            border: Color32::from_rgb(0xE2, 0xE8, 0xF0),   // slate-200
            text: Color32::from_rgb(0x0F, 0x17, 0x2A),     // slate-900
            text_weak: Color32::from_rgb(0x64, 0x74, 0x8B), // slate-500
            hover_bg: Color32::from_rgb(0xFF, 0xF7, 0xED), // axe-50
            selected_bg: Color32::from_rgb(0xFF, 0xED, 0xD5), // axe-100
            field_bg: Color32::WHITE,
        }
    }
}

pub fn apply(ctx: &egui::Context, dark: bool) {
    let p = palette(dark);
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };

    v.panel_fill = p.page_bg;
    v.window_fill = p.card_bg;
    v.extreme_bg_color = p.field_bg;
    v.faint_bg_color = p.hover_bg;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = AXE_ORANGE;
    v.selection.bg_fill = p.selected_bg;
    v.selection.stroke = Stroke::new(1.0, AXE_ORANGE);

    let radius = CornerRadius::same(6);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = radius;
        w.fg_stroke = Stroke::new(1.0, p.text);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text_weak);

    v.widgets.inactive.weak_bg_fill = p.card_bg;
    v.widgets.inactive.bg_fill = p.card_bg;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);

    v.widgets.hovered.weak_bg_fill = p.hover_bg;
    v.widgets.hovered.bg_fill = p.hover_bg;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, AXE_ORANGE_BRIGHT);

    v.widgets.active.weak_bg_fill = p.selected_bg;
    v.widgets.active.bg_fill = p.selected_bg;
    v.widgets.active.bg_stroke = Stroke::new(1.0, AXE_ORANGE);

    v.widgets.open.weak_bg_fill = p.hover_bg;
    v.widgets.open.bg_fill = p.card_bg;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.border);

    ctx.set_visuals(v);
}

/// White-on-orange primary action button, like the website's CTAs.
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.to_owned()).color(Color32::WHITE).strong())
        .fill(AXE_ORANGE)
}

/// Deeper orange for actions that destroy content (burning redactions).
pub fn danger_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.to_owned()).color(Color32::WHITE).strong())
        .fill(AXE_ORANGE_DEEP)
}

/// The PDF**Axe** wordmark as a two-color layout job.
pub fn wordmark(size: f32, text_color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let base = egui::TextFormat {
        font_id: egui::FontId::proportional(size),
        color: text_color,
        ..Default::default()
    };
    let mut orange = base.clone();
    orange.color = AXE_ORANGE;
    job.append("PDF", 0.0, base);
    job.append("Axe", 0.0, orange);
    job
}
