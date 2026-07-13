//! Glyph width metrics for measuring text advances.
//!
//! Widths come from the font's /Widths array when present (the common case for
//! any modern generator). Fonts without /Widths fall back to built-in Adobe
//! AFM tables for the standard-14 families (Helvetica, Times, Courier — ASCII
//! range). Fonts we can't measure return `None` from [`advance`], and callers
//! degrade gracefully (no reflow shift, centering approximation).

use std::collections::HashMap;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::ops::resolve_inherited;

/// Per-byte-code glyph widths for one simple font, in 1/1000 text-space units.
pub(crate) struct GlyphWidths {
    by_code: [Option<f32>; 256],
    /// /MissingWidth (or the family average for built-in tables).
    missing: Option<f32>,
}

impl GlyphWidths {
    fn code_width(&self, code: u8) -> Option<f32> {
        self.by_code[code as usize].or(self.missing)
    }
}

/// Text state parameters that scale a string's advance.
#[derive(Clone, Copy)]
pub(crate) struct AdvanceState {
    pub font_size: f32,
    /// Tc — character spacing.
    pub char_spacing: f32,
    /// Tw — word spacing (applies to byte 0x20 in simple fonts).
    pub word_spacing: f32,
    /// Tz — horizontal scaling, percent (100 = normal).
    pub h_scale: f32,
}

impl Default for AdvanceState {
    fn default() -> Self {
        Self { font_size: 0.0, char_spacing: 0.0, word_spacing: 0.0, h_scale: 100.0 }
    }
}

/// Advance (text-space units) of a byte string, or `None` when any code's
/// width is unknown.
pub(crate) fn advance(gw: &GlyphWidths, bytes: &[u8], st: &AdvanceState) -> Option<f32> {
    let mut w = 0.0f32;
    for &b in bytes {
        w += gw.code_width(b)? / 1000.0 * st.font_size + st.char_spacing;
        if b == 0x20 {
            w += st.word_spacing;
        }
    }
    Some(w * st.h_scale / 100.0)
}

/// Advance contributed by a TJ number element (thousandths of text space).
pub(crate) fn tj_number_advance(n: f32, st: &AdvanceState) -> f32 {
    -n / 1000.0 * st.font_size * st.h_scale / 100.0
}

fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    match obj {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(obj),
        _ => obj,
    }
}

fn num(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r as f32),
        _ => None,
    }
}

/// Width tables for every measurable font in the page's resources.
pub(crate) fn page_font_widths(doc: &Document, page_id: ObjectId) -> HashMap<String, GlyphWidths> {
    let mut out = HashMap::new();
    let Some(res_obj) = resolve_inherited(doc, page_id, b"Resources") else { return out };
    let res_resolved = resolve(doc, &res_obj).clone();
    let Ok(res_dict) = res_resolved.as_dict() else { return out };
    let Some(fonts_obj) = res_dict.get(b"Font").ok().cloned() else { return out };
    let fonts_resolved = resolve(doc, &fonts_obj).clone();
    let Ok(fonts) = fonts_resolved.as_dict() else { return out };

    for (name, font_obj) in fonts.iter() {
        let name = String::from_utf8_lossy(name).into_owned();
        let font_resolved = resolve(doc, font_obj).clone();
        let Ok(font) = font_resolved.as_dict() else { continue };
        if let Some(gw) = widths_for_font(doc, font) {
            out.insert(name, gw);
        }
    }
    out
}

fn widths_for_font(doc: &Document, font: &Dictionary) -> Option<GlyphWidths> {
    let subtype = font.get(b"Subtype").ok().and_then(|o| o.as_name().ok()).unwrap_or_default();
    if subtype == b"Type0" {
        // Composite fonts need CID-keyed /W handling — not measured yet.
        return None;
    }

    let missing = font
        .get(b"FontDescriptor")
        .ok()
        .map(|o| resolve(doc, o))
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"MissingWidth").ok())
        .and_then(num);

    if let (Ok(fc), Ok(w_obj)) = (font.get(b"FirstChar"), font.get(b"Widths")) {
        let first = resolve(doc, fc).as_i64().ok()?.clamp(0, 255) as usize;
        if let Object::Array(arr) = resolve(doc, w_obj) {
            let mut by_code = [None; 256];
            for (i, item) in arr.iter().enumerate() {
                let Some(code) = first.checked_add(i).filter(|&c| c < 256) else { break };
                by_code[code] = num(resolve(doc, item));
            }
            return Some(GlyphWidths { by_code, missing });
        }
    }

    // No /Widths: standard-14 built-ins by (aliased) base font name.
    let base = font.get(b"BaseFont").ok().and_then(|o| o.as_name().ok()).unwrap_or_default();
    std14_widths(base)
}

/// Built-in table for a standard-14 base font name (subset prefixes and the
/// usual Arial/TimesNewRoman aliases included). ASCII range only.
pub(crate) fn std14_widths(base_font: &[u8]) -> Option<GlyphWidths> {
    let name = String::from_utf8_lossy(base_font);
    // Strip subset prefix "ABCDEF+".
    let name = match name.split_once('+') {
        Some((p, rest)) if p.len() == 6 => rest.to_string(),
        _ => name.into_owned(),
    };
    let (table, missing): (&[u16; 95], f32) = match name.as_str() {
        "Helvetica" | "Helvetica-Oblique" | "Arial" | "ArialMT" | "Arial-ItalicMT" => (&HELVETICA, 556.0),
        "Helvetica-Bold" | "Helvetica-BoldOblique" | "Arial-BoldMT" | "Arial-BoldItalicMT" => {
            (&HELVETICA_BOLD, 556.0)
        }
        "Times-Roman" | "TimesNewRoman" | "TimesNewRomanPSMT" => (&TIMES_ROMAN, 500.0),
        "Times-Bold" | "TimesNewRomanPS-BoldMT" => (&TIMES_BOLD, 500.0),
        "Times-Italic" | "TimesNewRomanPS-ItalicMT" => (&TIMES_ITALIC, 500.0),
        "Times-BoldItalic" | "TimesNewRomanPS-BoldItalicMT" => (&TIMES_BOLD_ITALIC, 500.0),
        n if n.starts_with("Courier") => {
            let mut by_code = [None; 256];
            for c in 32..=126usize {
                by_code[c] = Some(600.0);
            }
            return Some(GlyphWidths { by_code, missing: Some(600.0) });
        }
        _ => return None,
    };
    let mut by_code = [None; 256];
    for (i, &w) in table.iter().enumerate() {
        by_code[32 + i] = Some(w as f32);
    }
    Some(GlyphWidths { by_code, missing: Some(missing) })
}

// Adobe Core-14 AFM widths, glyph codes 32..=126 (space..asciitilde).

#[rustfmt::skip]
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556,
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556,
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556,
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

#[rustfmt::skip]
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611,
    975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584, 556,
    333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611,
    611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

#[rustfmt::skip]
const TIMES_ROMAN: [u16; 95] = [
    250, 333, 408, 500, 500, 833, 778, 180, 333, 333, 500, 564, 250, 333, 250, 278,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 278, 278, 564, 564, 564, 444,
    921, 722, 667, 667, 722, 611, 556, 722, 722, 333, 389, 722, 611, 889, 722, 722,
    556, 722, 667, 556, 611, 722, 722, 944, 722, 722, 611, 333, 278, 333, 469, 500,
    333, 444, 500, 444, 500, 444, 333, 500, 500, 278, 278, 500, 278, 778, 500, 500,
    500, 500, 333, 389, 278, 500, 500, 722, 500, 500, 444, 480, 200, 480, 541,
];

#[rustfmt::skip]
const TIMES_BOLD: [u16; 95] = [
    250, 333, 555, 500, 500, 1000, 833, 278, 333, 333, 500, 570, 250, 333, 250, 278,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 333, 333, 570, 570, 570, 500,
    930, 722, 667, 722, 722, 667, 611, 778, 778, 389, 500, 778, 667, 944, 722, 778,
    611, 778, 722, 556, 667, 722, 722, 1000, 722, 722, 667, 333, 278, 333, 581, 500,
    333, 500, 556, 444, 556, 444, 333, 500, 556, 278, 333, 556, 278, 833, 556, 500,
    556, 556, 444, 389, 333, 556, 500, 722, 500, 500, 444, 394, 220, 394, 520,
];

#[rustfmt::skip]
const TIMES_ITALIC: [u16; 95] = [
    250, 333, 420, 500, 500, 833, 778, 214, 333, 333, 500, 675, 250, 333, 250, 278,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 333, 333, 675, 675, 675, 500,
    920, 611, 611, 667, 722, 611, 611, 722, 722, 333, 444, 667, 556, 833, 667, 722,
    611, 722, 611, 500, 556, 722, 611, 833, 611, 556, 556, 389, 278, 389, 422, 500,
    333, 500, 500, 444, 500, 444, 278, 500, 500, 278, 278, 444, 278, 722, 500, 500,
    500, 500, 389, 389, 278, 500, 444, 667, 444, 444, 389, 400, 275, 400, 541,
];

#[rustfmt::skip]
const TIMES_BOLD_ITALIC: [u16; 95] = [
    250, 389, 555, 500, 500, 833, 778, 278, 333, 333, 500, 570, 250, 333, 250, 278,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 333, 333, 570, 570, 570, 500,
    832, 667, 667, 667, 722, 667, 667, 722, 778, 389, 500, 667, 611, 889, 722, 722,
    611, 722, 667, 556, 611, 722, 667, 889, 667, 611, 611, 333, 278, 333, 570, 500,
    333, 500, 500, 444, 500, 444, 333, 500, 556, 278, 278, 500, 278, 778, 556, 500,
    500, 500, 389, 389, 278, 556, 444, 667, 500, 444, 389, 348, 220, 348, 570,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helvetica_measures_ascii() {
        let gw = std14_widths(b"Helvetica").unwrap();
        let st = AdvanceState { font_size: 10.0, ..Default::default() };
        // "Hi" = H(722) + i(222) = 944/1000 * 10
        let w = advance(&gw, b"Hi", &st).unwrap();
        assert!((w - 9.44).abs() < 1e-4, "got {w}");
    }

    #[test]
    fn subset_prefix_and_aliases_resolve() {
        assert!(std14_widths(b"ABCDEF+Helvetica-Bold").is_some());
        assert!(std14_widths(b"ArialMT").is_some());
        assert!(std14_widths(b"TimesNewRomanPSMT").is_some());
        assert!(std14_widths(b"CourierNewPSMT").is_some());
        assert!(std14_widths(b"SomeCustomFont").is_none());
    }

    #[test]
    fn char_and_word_spacing_apply() {
        let gw = std14_widths(b"Courier").unwrap();
        let st = AdvanceState { font_size: 10.0, char_spacing: 1.0, word_spacing: 2.0, h_scale: 100.0 };
        // "a b" = 3 glyphs * (600/1000*10 + 1) + one space * 2 = 21 + 2
        let w = advance(&gw, b"a b", &st).unwrap();
        assert!((w - 23.0).abs() < 1e-4, "got {w}");
    }

    #[test]
    fn unknown_code_without_missing_width_bails() {
        let gw = std14_widths(b"Helvetica").unwrap();
        // Built-ins carry a family-average missing width, so high bytes still measure.
        let st = AdvanceState { font_size: 12.0, ..Default::default() };
        assert!(advance(&gw, &[0xE9], &st).is_some());
        let no_missing = GlyphWidths { by_code: [None; 256], missing: None };
        assert!(advance(&no_missing, b"A", &st).is_none());
    }
}
