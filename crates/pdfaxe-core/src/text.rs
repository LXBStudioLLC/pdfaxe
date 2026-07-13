//! Text editing, step 1 of the wedge: extract positioned text runs from page
//! content streams, decode them through the font's encoding, re-encode edited
//! strings, and patch the content stream in place.
//!
//! v1 scope (stated honestly, enforced in code):
//! - Simple single-byte fonts (Type1/TrueType with WinAnsi/Standard encodings
//!   or a ToUnicode CMap) are editable. Composite (Type0/CID) fonts are
//!   displayed but marked non-editable unless their ToUnicode map inverts.
//! - Same font, same baseline: an edit keeps the run's start position. When
//!   the font's glyph widths are known (/Widths or a standard-14 face), the
//!   rest of the line shifts to fit the new width and the shift is undone at
//!   the next line break — same-line reflow. Paragraph re-wrap (moving words
//!   across lines) is not attempted yet.
//! - Positions are the text-matrix origin at the start of each show op;
//!   within-line advances are not simulated yet (good enough for hitboxes).

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::metrics::{advance, page_font_widths, tj_number_advance, AdvanceState, GlyphWidths};
use crate::ops::resolve_inherited;

/// One show-text operation (or one string element inside a TJ array).
#[derive(Debug, Clone)]
pub struct TextRun {
    /// Index of the operation in the page's decoded content stream.
    pub op_index: usize,
    /// For TJ: index of the string element inside the operand array.
    pub tj_item: Option<usize>,
    /// Font resource name (e.g. "F0").
    pub font_res: String,
    pub font_size: f32,
    /// Text-space origin (baseline) at the start of the run, PDF points.
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub editable: bool,
}

/// Byte-level decoder/encoder for one font resource.
struct FontCodec {
    /// code bytes -> unicode string
    to_uni: HashMap<Vec<u8>, String>,
    /// unicode char -> code bytes (only when invertible)
    from_uni: HashMap<char, Vec<u8>>,
    code_len: usize,
    invertible: bool,
}

impl FontCodec {
    fn decode(&self, bytes: &[u8]) -> String {
        let mut out = String::new();
        for chunk in bytes.chunks(self.code_len) {
            match self.to_uni.get(chunk) {
                Some(s) => out.push_str(s),
                None => out.push('\u{FFFD}'),
            }
        }
        out
    }

    fn encode(&self, text: &str) -> Option<Vec<u8>> {
        if !self.invertible {
            return None;
        }
        let mut out = Vec::new();
        for ch in text.chars() {
            out.extend_from_slice(self.from_uni.get(&ch)?);
        }
        Some(out)
    }
}

/// WinAnsiEncoding (cp1252). Identity to Latin-1 except 0x80..=0x9F.
fn winansi_char(b: u8) -> Option<char> {
    const HIGH: [Option<char>; 32] = [
        Some('€'), None, Some('‚'), Some('ƒ'), Some('„'), Some('…'), Some('†'), Some('‡'),
        Some('ˆ'), Some('‰'), Some('Š'), Some('‹'), Some('Œ'), None, Some('Ž'), None,
        None, Some('‘'), Some('’'), Some('“'), Some('”'), Some('•'), Some('–'), Some('—'),
        Some('˜'), Some('™'), Some('š'), Some('›'), Some('œ'), None, Some('ž'), Some('Ÿ'),
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => Some(b as char), // Latin-1 range maps 1:1 to Unicode
    }
}

/// One simple-font encoding: code byte -> char.
type EncTable = [Option<char>; 256];

fn ascii_table() -> EncTable {
    let mut t = [None; 256];
    for b in 0x20..=0x7Eu8 {
        t[b as usize] = Some(b as char);
    }
    t
}

fn winansi_table() -> EncTable {
    let mut t = [None; 256];
    for b in 0x20..=0xFFu16 {
        t[b as usize] = winansi_char(b as u8);
    }
    t
}

/// The subset of the Adobe Glyph List that covers Latin text, punctuation and
/// the names used by the standard PDF encodings. Everything else falls back to
/// the uniXXXX / uXXXXXX conventions or single-letter names.
const AGL_PAIRS: &[(&str, char)] = &[
    ("space", ' '), ("exclam", '!'), ("quotedbl", '"'), ("numbersign", '#'),
    ("dollar", '$'), ("percent", '%'), ("ampersand", '&'), ("quotesingle", '\''),
    ("parenleft", '('), ("parenright", ')'), ("asterisk", '*'), ("plus", '+'),
    ("comma", ','), ("hyphen", '-'), ("period", '.'), ("slash", '/'),
    ("zero", '0'), ("one", '1'), ("two", '2'), ("three", '3'), ("four", '4'),
    ("five", '5'), ("six", '6'), ("seven", '7'), ("eight", '8'), ("nine", '9'),
    ("colon", ':'), ("semicolon", ';'), ("less", '<'), ("equal", '='),
    ("greater", '>'), ("question", '?'), ("at", '@'),
    ("bracketleft", '['), ("backslash", '\\'), ("bracketright", ']'),
    ("asciicircum", '^'), ("underscore", '_'), ("grave", '`'),
    ("braceleft", '{'), ("bar", '|'), ("braceright", '}'), ("asciitilde", '~'),
    ("quoteleft", '\u{2018}'), ("quoteright", '\u{2019}'),
    ("quotedblleft", '\u{201C}'), ("quotedblright", '\u{201D}'),
    ("quotesinglbase", '\u{201A}'), ("quotedblbase", '\u{201E}'),
    ("guillemotleft", '\u{AB}'), ("guillemotright", '\u{BB}'),
    ("guilsinglleft", '\u{2039}'), ("guilsinglright", '\u{203A}'),
    ("endash", '\u{2013}'), ("emdash", '\u{2014}'), ("bullet", '\u{2022}'),
    ("ellipsis", '\u{2026}'), ("dagger", '\u{2020}'), ("daggerdbl", '\u{2021}'),
    ("perthousand", '\u{2030}'), ("periodcentered", '\u{B7}'),
    ("exclamdown", '\u{A1}'), ("questiondown", '\u{BF}'),
    ("fraction", '\u{2044}'), ("minus", '\u{2212}'),
    ("fi", '\u{FB01}'), ("fl", '\u{FB02}'),
    ("cent", '¢'), ("sterling", '£'), ("yen", '¥'), ("florin", 'ƒ'),
    ("currency", '¤'), ("Euro", '€'), ("euro", '€'),
    ("section", '§'), ("paragraph", '¶'), ("copyright", '©'), ("registered", '®'),
    ("trademark", '™'), ("degree", '°'), ("plusminus", '±'), ("logicalnot", '¬'),
    ("mu", 'µ'), ("brokenbar", '¦'),
    ("ordfeminine", 'ª'), ("ordmasculine", 'º'),
    ("onequarter", '¼'), ("onehalf", '½'), ("threequarters", '¾'),
    ("onesuperior", '¹'), ("twosuperior", '²'), ("threesuperior", '³'),
    ("multiply", '×'), ("divide", '÷'),
    ("acute", '´'), ("dieresis", '¨'), ("circumflex", 'ˆ'), ("tilde", '˜'),
    ("macron", '¯'), ("breve", '˘'), ("dotaccent", '˙'), ("ring", '˚'),
    ("cedilla", '¸'), ("hungarumlaut", '˝'), ("ogonek", '˛'), ("caron", 'ˇ'),
    ("AE", 'Æ'), ("Eth", 'Ð'), ("Thorn", 'Þ'), ("OE", 'Œ'),
    ("Oslash", 'Ø'), ("Lslash", 'Ł'),
    ("Agrave", 'À'), ("Aacute", 'Á'), ("Acircumflex", 'Â'), ("Atilde", 'Ã'),
    ("Adieresis", 'Ä'), ("Aring", 'Å'), ("Ccedilla", 'Ç'),
    ("Egrave", 'È'), ("Eacute", 'É'), ("Ecircumflex", 'Ê'), ("Edieresis", 'Ë'),
    ("Igrave", 'Ì'), ("Iacute", 'Í'), ("Icircumflex", 'Î'), ("Idieresis", 'Ï'),
    ("Ntilde", 'Ñ'),
    ("Ograve", 'Ò'), ("Oacute", 'Ó'), ("Ocircumflex", 'Ô'), ("Otilde", 'Õ'),
    ("Odieresis", 'Ö'),
    ("Ugrave", 'Ù'), ("Uacute", 'Ú'), ("Ucircumflex", 'Û'), ("Udieresis", 'Ü'),
    ("Yacute", 'Ý'), ("Ydieresis", 'Ÿ'),
    ("Scaron", 'Š'), ("Zcaron", 'Ž'),
    ("ae", 'æ'), ("eth", 'ð'), ("thorn", 'þ'), ("oe", 'œ'),
    ("oslash", 'ø'), ("lslash", 'ł'), ("dotlessi", 'ı'), ("germandbls", 'ß'),
    ("agrave", 'à'), ("aacute", 'á'), ("acircumflex", 'â'), ("atilde", 'ã'),
    ("adieresis", 'ä'), ("aring", 'å'), ("ccedilla", 'ç'),
    ("egrave", 'è'), ("eacute", 'é'), ("ecircumflex", 'ê'), ("edieresis", 'ë'),
    ("igrave", 'ì'), ("iacute", 'í'), ("icircumflex", 'î'), ("idieresis", 'ï'),
    ("ntilde", 'ñ'),
    ("ograve", 'ò'), ("oacute", 'ó'), ("ocircumflex", 'ô'), ("otilde", 'õ'),
    ("odieresis", 'ö'),
    ("ugrave", 'ù'), ("uacute", 'ú'), ("ucircumflex", 'û'), ("udieresis", 'ü'),
    ("yacute", 'ý'), ("ydieresis", 'ÿ'),
    ("scaron", 'š'), ("zcaron", 'ž'),
    ("notequal", '≠'), ("infinity", '∞'), ("lessequal", '≤'), ("greaterequal", '≥'),
    ("partialdiff", '∂'), ("summation", '∑'), ("product", '∏'), ("pi", 'π'),
    ("integral", '∫'), ("Omega", 'Ω'), ("radical", '√'), ("approxequal", '≈'),
    ("Delta", '∆'), ("lozenge", '◊'),
];

fn glyph_to_char(name: &str) -> Option<char> {
    static MAP: OnceLock<HashMap<&'static str, char>> = OnceLock::new();
    let map = MAP.get_or_init(|| AGL_PAIRS.iter().copied().collect());
    if let Some(&c) = map.get(name) {
        return Some(c);
    }
    if let Some(hex) = name.strip_prefix("uni") {
        if hex.len() == 4 {
            if let Ok(n) = u32::from_str_radix(hex, 16) {
                return char::from_u32(n);
            }
        }
    }
    if let Some(hex) = name.strip_prefix('u') {
        if (4..=6).contains(&hex.len()) {
            if let Ok(n) = u32::from_str_radix(hex, 16) {
                return char::from_u32(n);
            }
        }
    }
    // AGL maps single-letter names to themselves ("A" -> 'A').
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Some(c);
        }
    }
    None
}

/// MacRomanEncoding, high range (PDF spec Annex D). 0xF0 (Apple logo) omitted.
const MACROMAN_HIGH: &[(u8, &str)] = &[
    (0x80, "Adieresis"), (0x81, "Aring"), (0x82, "Ccedilla"), (0x83, "Eacute"),
    (0x84, "Ntilde"), (0x85, "Odieresis"), (0x86, "Udieresis"), (0x87, "aacute"),
    (0x88, "agrave"), (0x89, "acircumflex"), (0x8A, "adieresis"), (0x8B, "atilde"),
    (0x8C, "aring"), (0x8D, "ccedilla"), (0x8E, "eacute"), (0x8F, "egrave"),
    (0x90, "ecircumflex"), (0x91, "edieresis"), (0x92, "iacute"), (0x93, "igrave"),
    (0x94, "icircumflex"), (0x95, "idieresis"), (0x96, "ntilde"), (0x97, "oacute"),
    (0x98, "ograve"), (0x99, "ocircumflex"), (0x9A, "odieresis"), (0x9B, "otilde"),
    (0x9C, "uacute"), (0x9D, "ugrave"), (0x9E, "ucircumflex"), (0x9F, "udieresis"),
    (0xA0, "dagger"), (0xA1, "degree"), (0xA2, "cent"), (0xA3, "sterling"),
    (0xA4, "section"), (0xA5, "bullet"), (0xA6, "paragraph"), (0xA7, "germandbls"),
    (0xA8, "registered"), (0xA9, "copyright"), (0xAA, "trademark"), (0xAB, "acute"),
    (0xAC, "dieresis"), (0xAD, "notequal"), (0xAE, "AE"), (0xAF, "Oslash"),
    (0xB0, "infinity"), (0xB1, "plusminus"), (0xB2, "lessequal"), (0xB3, "greaterequal"),
    (0xB4, "yen"), (0xB5, "mu"), (0xB6, "partialdiff"), (0xB7, "summation"),
    (0xB8, "product"), (0xB9, "pi"), (0xBA, "integral"), (0xBB, "ordfeminine"),
    (0xBC, "ordmasculine"), (0xBD, "Omega"), (0xBE, "ae"), (0xBF, "oslash"),
    (0xC0, "questiondown"), (0xC1, "exclamdown"), (0xC2, "logicalnot"), (0xC3, "radical"),
    (0xC4, "florin"), (0xC5, "approxequal"), (0xC6, "Delta"), (0xC7, "guillemotleft"),
    (0xC8, "guillemotright"), (0xC9, "ellipsis"), (0xCA, "space"), (0xCB, "Agrave"),
    (0xCC, "Atilde"), (0xCD, "Otilde"), (0xCE, "OE"), (0xCF, "oe"),
    (0xD0, "endash"), (0xD1, "emdash"), (0xD2, "quotedblleft"), (0xD3, "quotedblright"),
    (0xD4, "quoteleft"), (0xD5, "quoteright"), (0xD6, "divide"), (0xD7, "lozenge"),
    (0xD8, "ydieresis"), (0xD9, "Ydieresis"), (0xDA, "fraction"), (0xDB, "currency"),
    (0xDC, "guilsinglleft"), (0xDD, "guilsinglright"), (0xDE, "fi"), (0xDF, "fl"),
    (0xE0, "daggerdbl"), (0xE1, "periodcentered"), (0xE2, "quotesinglbase"),
    (0xE3, "quotedblbase"), (0xE4, "perthousand"), (0xE5, "Acircumflex"),
    (0xE6, "Ecircumflex"), (0xE7, "Aacute"), (0xE8, "Edieresis"), (0xE9, "Egrave"),
    (0xEA, "Iacute"), (0xEB, "Icircumflex"), (0xEC, "Idieresis"), (0xED, "Igrave"),
    (0xEE, "Oacute"), (0xEF, "Ocircumflex"), (0xF1, "Ograve"), (0xF2, "Uacute"),
    (0xF3, "Ucircumflex"), (0xF4, "Ugrave"), (0xF5, "dotlessi"), (0xF6, "circumflex"),
    (0xF7, "tilde"), (0xF8, "macron"), (0xF9, "breve"), (0xFA, "dotaccent"),
    (0xFB, "ring"), (0xFC, "cedilla"), (0xFD, "hungarumlaut"), (0xFE, "ogonek"),
    (0xFF, "caron"),
];

/// StandardEncoding's deviations from ASCII (PDF spec Annex D).
const STANDARD_DIFFS: &[(u8, &str)] = &[
    (0x27, "quoteright"), (0x60, "quoteleft"),
    (0xA1, "exclamdown"), (0xA2, "cent"), (0xA3, "sterling"), (0xA4, "fraction"),
    (0xA5, "yen"), (0xA6, "florin"), (0xA7, "section"), (0xA8, "currency"),
    (0xA9, "quotesingle"), (0xAA, "quotedblleft"), (0xAB, "guillemotleft"),
    (0xAC, "guilsinglleft"), (0xAD, "guilsinglright"), (0xAE, "fi"), (0xAF, "fl"),
    (0xB1, "endash"), (0xB2, "dagger"), (0xB3, "daggerdbl"), (0xB4, "periodcentered"),
    (0xB6, "paragraph"), (0xB7, "bullet"), (0xB8, "quotesinglbase"),
    (0xB9, "quotedblbase"), (0xBA, "quotedblright"), (0xBB, "guillemotright"),
    (0xBC, "ellipsis"), (0xBD, "perthousand"), (0xBF, "questiondown"),
    (0xC1, "grave"), (0xC2, "acute"), (0xC3, "circumflex"), (0xC4, "tilde"),
    (0xC5, "macron"), (0xC6, "breve"), (0xC7, "dotaccent"), (0xC8, "dieresis"),
    (0xCA, "ring"), (0xCB, "cedilla"), (0xCD, "hungarumlaut"), (0xCE, "ogonek"),
    (0xCF, "caron"), (0xD0, "emdash"),
    (0xE1, "AE"), (0xE3, "ordfeminine"), (0xE8, "Lslash"), (0xE9, "Oslash"),
    (0xEA, "OE"), (0xEB, "ordmasculine"),
    (0xF1, "ae"), (0xF5, "dotlessi"), (0xF8, "lslash"), (0xF9, "oslash"),
    (0xFA, "oe"), (0xFB, "germandbls"),
];

fn macroman_table() -> EncTable {
    let mut t = ascii_table();
    for &(b, name) in MACROMAN_HIGH {
        t[b as usize] = glyph_to_char(name);
    }
    t
}

fn standard_table() -> EncTable {
    let mut t = ascii_table();
    for &(b, name) in STANDARD_DIFFS {
        t[b as usize] = glyph_to_char(name);
    }
    t
}

fn named_table(name: &[u8]) -> Option<EncTable> {
    match name {
        b"WinAnsiEncoding" => Some(winansi_table()),
        b"MacRomanEncoding" => Some(macroman_table()),
        b"StandardEncoding" => Some(standard_table()),
        _ => None,
    }
}

fn table_codec(table: &EncTable, invertible: bool) -> FontCodec {
    let mut to_uni = HashMap::new();
    let mut from_uni = HashMap::new();
    for (b, ch) in table.iter().enumerate() {
        if let Some(ch) = ch {
            to_uni.insert(vec![b as u8], ch.to_string());
            from_uni.entry(*ch).or_insert_with(|| vec![b as u8]);
        }
    }
    FontCodec { to_uni, from_uni, code_len: 1, invertible }
}

/// Parse hex tokens like <0041> from a CMap byte stream, starting at `pos`.
fn next_hex(data: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    while *pos < data.len() && data[*pos] != b'<' {
        // stop scanning at section keywords
        if data[*pos] == b'e'
            && (data[*pos..].starts_with(b"endbfchar") || data[*pos..].starts_with(b"endbfrange"))
        {
            return None;
        }
        *pos += 1;
    }
    if *pos >= data.len() {
        return None;
    }
    *pos += 1; // skip '<'
    let start = *pos;
    while *pos < data.len() && data[*pos] != b'>' {
        *pos += 1;
    }
    let hex: Vec<u8> = data[start..*pos]
        .iter()
        .copied()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    *pos += 1; // skip '>'
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.chunks(2) {
        let s = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(bytes)
}

fn utf16be_to_string(bytes: &[u8]) -> Option<String> {
    if bytes.len() % 2 != 0 {
        // single byte destination: treat as Latin-1
        return Some(bytes.iter().map(|&b| b as char).collect());
    }
    let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16(&units).ok()
}

/// Parse a ToUnicode CMap's bfchar/bfrange sections into a codec.
fn tounicode_codec(cmap: &[u8]) -> Option<FontCodec> {
    let mut to_uni: HashMap<Vec<u8>, String> = HashMap::new();
    let mut code_len = 1usize;

    let mut pos = 0usize;
    while let Some(found) = find_from(cmap, pos, b"beginbfchar") {
        pos = found + b"beginbfchar".len();
        loop {
            let Some(src) = next_hex(cmap, &mut pos) else { break };
            let Some(dst) = next_hex(cmap, &mut pos) else { break };
            code_len = code_len.max(src.len());
            if let Some(s) = utf16be_to_string(&dst) {
                to_uni.insert(src, s);
            }
        }
    }

    pos = 0;
    while let Some(found) = find_from(cmap, pos, b"beginbfrange") {
        pos = found + b"beginbfrange".len();
        loop {
            let Some(lo) = next_hex(cmap, &mut pos) else { break };
            let Some(hi) = next_hex(cmap, &mut pos) else { break };
            let Some(dst) = next_hex(cmap, &mut pos) else { break };
            code_len = code_len.max(lo.len());
            let (lo_n, hi_n) = (be_num(&lo), be_num(&hi));
            let width = lo.len();
            if hi_n < lo_n || hi_n - lo_n > 65_535 {
                break;
            }
            for (offset, code) in (lo_n..=hi_n).enumerate() {
                let src = num_be(code, width);
                if let Some(mut s) = utf16be_to_string(&dst) {
                    // increment the last scalar by offset
                    if let Some(last) = s.pop() {
                        if let Some(bumped) = char::from_u32(last as u32 + offset as u32) {
                            s.push(bumped);
                            to_uni.insert(src, s);
                        }
                    }
                }
            }
        }
    }

    if to_uni.is_empty() {
        return None;
    }
    // uniform code length is assumed; mixed-width CMaps are rare in practice
    let uniform = to_uni.keys().all(|k| k.len() == code_len);
    let mut from_uni: HashMap<char, Vec<u8>> = HashMap::new();
    let mut invertible = uniform;
    if uniform {
        for (bytes, s) in &to_uni {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => {
                    if from_uni.insert(ch, bytes.clone()).is_some() {
                        // many-to-one keeps first mapping; still usable
                    }
                }
                _ => invertible = false, // multi-char destination: display-only
            }
        }
    }
    Some(FontCodec { to_uni, from_uni, code_len, invertible })
}

fn find_from(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn be_num(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32)
}

fn num_be(n: u32, width: usize) -> Vec<u8> {
    (0..width).rev().map(|i| ((n >> (8 * i)) & 0xFF) as u8).collect()
}

fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    match obj {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(obj),
        _ => obj,
    }
}

/// Build codecs for every font in the page's resources.
fn page_font_codecs(doc: &Document, page_id: ObjectId) -> HashMap<String, FontCodec> {
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
        out.insert(name, codec_for_font(doc, font));
    }
    out
}

fn codec_for_font(doc: &Document, font: &Dictionary) -> FontCodec {
    // Prefer ToUnicode when present.
    if let Ok(tu) = font.get(b"ToUnicode") {
        let tu = resolve(doc, tu);
        if let Object::Stream(stream) = tu {
            if let Ok(data) = stream.decompressed_content() {
                if let Some(codec) = tounicode_codec(&data) {
                    return codec;
                }
            }
        }
    }
    let subtype = font
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        .map(|n| n.to_vec())
        .unwrap_or_default();
    if subtype == b"Type0" {
        // Composite font without a usable ToUnicode: display-only, 2-byte codes.
        return FontCodec { to_uni: HashMap::new(), from_uni: HashMap::new(), code_len: 2, invertible: false };
    }

    // Simple font. Honor /Encoding (a name, or a dict with /BaseEncoding and
    // /Differences). Symbolic fonts use their built-in encoding, which we
    // can't read from font programs yet — decode best-effort, refuse edits.
    let symbolic = font
        .get(b"FontDescriptor")
        .ok()
        .map(|o| resolve(doc, o))
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"Flags").ok())
        .and_then(|f| f.as_i64().ok())
        .is_some_and(|flags| flags & 4 != 0);

    match font.get(b"Encoding").ok().map(|o| resolve(doc, o)) {
        Some(Object::Name(n)) => {
            if let Some(t) = named_table(n) {
                return table_codec(&t, true);
            }
            // MacExpertEncoding or a custom name we don't know: display-only.
            table_codec(&winansi_table(), false)
        }
        Some(Object::Dictionary(enc)) => {
            let base = enc
                .get(b"BaseEncoding")
                .ok()
                .and_then(|o| resolve(doc, o).as_name().ok())
                .and_then(named_table);
            let mut table = base.unwrap_or_else(|| {
                // Without a BaseEncoding, the base is the font's built-in
                // encoding: unknowable for symbolic fonts, ~WinAnsi otherwise.
                if symbolic { [None; 256] } else { winansi_table() }
            });
            if let Ok(Object::Array(diffs)) = enc.get(b"Differences").map(|o| resolve(doc, o)) {
                let mut code = 0usize;
                for item in diffs {
                    match item {
                        Object::Integer(n) => code = (*n).clamp(0, 255) as usize,
                        Object::Name(g) => {
                            if code < 256 {
                                table[code] = glyph_to_char(&String::from_utf8_lossy(g));
                                code += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            table_codec(&table, true)
        }
        // No /Encoding at all: non-symbolic fonts are effectively WinAnsi in
        // the wild; symbolic ones get best-effort display but no edits.
        _ => table_codec(&winansi_table(), !symbolic),
    }
}

fn num(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r as f32),
        _ => None,
    }
}

/// Extract text runs from one page (1-based).
pub fn text_runs(doc: &Document, page_no: u32) -> Result<Vec<TextRun>> {
    let pages = doc.get_pages();
    let &page_id = pages.get(&page_no).ok_or_else(|| anyhow!("page {page_no} out of range"))?;
    let content_bytes = doc.get_page_content(page_id);
    let content = Content::decode(&content_bytes).map_err(|e| anyhow!("couldn't parse page content: {e}"))?;
    let codecs = page_font_codecs(doc, page_id);

    let mut runs = Vec::new();
    let mut font_res = String::new();
    let mut font_size = 0.0f32;
    // text matrix origin (e,f) and line matrix origin
    let mut tm = (0.0f32, 0.0f32);
    let mut tlm = (0.0f32, 0.0f32);
    let mut leading = 0.0f32;

    let decode = |font: &str, bytes: &[u8]| -> (String, bool) {
        match codecs.get(font) {
            Some(c) => (c.decode(bytes), c.invertible),
            None => (String::from_utf8_lossy(bytes).into_owned(), false),
        }
    };

    for (op_index, op) in content.operations.iter().enumerate() {
        match op.operator.as_str() {
            "BT" => {
                tm = (0.0, 0.0);
                tlm = (0.0, 0.0);
            }
            "Tf" => {
                if let Some(Object::Name(n)) = op.operands.first() {
                    font_res = String::from_utf8_lossy(n).into_owned();
                }
                if let Some(sz) = op.operands.get(1).and_then(num) {
                    font_size = sz;
                }
            }
            "Tm" => {
                if op.operands.len() == 6 {
                    let e = num(&op.operands[4]).unwrap_or(0.0);
                    let f = num(&op.operands[5]).unwrap_or(0.0);
                    tm = (e, f);
                    tlm = (e, f);
                }
            }
            "Td" | "TD" => {
                let tx = op.operands.first().and_then(num).unwrap_or(0.0);
                let ty = op.operands.get(1).and_then(num).unwrap_or(0.0);
                if op.operator == "TD" {
                    leading = -ty;
                }
                tlm = (tlm.0 + tx, tlm.1 + ty);
                tm = tlm;
            }
            "TL" => {
                leading = op.operands.first().and_then(num).unwrap_or(0.0);
            }
            "T*" => {
                tlm = (tlm.0, tlm.1 - leading);
                tm = tlm;
            }
            "Tj" | "'" | "\"" => {
                if op.operator != "Tj" {
                    tlm = (tlm.0, tlm.1 - leading);
                    tm = tlm;
                }
                let string_operand = op.operands.iter().rev().find_map(|o| match o {
                    Object::String(bytes, _) => Some(bytes.clone()),
                    _ => None,
                });
                if let Some(bytes) = string_operand {
                    let (text, editable) = decode(&font_res, &bytes);
                    runs.push(TextRun {
                        op_index,
                        tj_item: None,
                        font_res: font_res.clone(),
                        font_size,
                        x: tm.0,
                        y: tm.1,
                        text,
                        editable,
                    });
                }
            }
            "TJ" => {
                if let Some(Object::Array(items)) = op.operands.first() {
                    for (tj_item, item) in items.iter().enumerate() {
                        if let Object::String(bytes, _) = item {
                            let (text, editable) = decode(&font_res, bytes);
                            runs.push(TextRun {
                                op_index,
                                tj_item: Some(tj_item),
                                font_res: font_res.clone(),
                                font_size,
                                x: tm.0,
                                y: tm.1,
                                text,
                                editable,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(runs)
}

/// Replace the text of one run (same font, same start position) and patch the
/// page's content stream. When the font's glyph widths are known and the edit
/// changes the run's width, the rest of the line is shifted to fit and the
/// shift is undone at the next line break (same-line reflow); otherwise the
/// edit degrades to a pure in-place patch. Content-stream op indices may
/// change — re-list runs after every edit.
pub fn edit_run(doc: &mut Document, page_no: u32, run: &TextRun, new_text: &str) -> Result<()> {
    let pages = doc.get_pages();
    let &page_id = pages.get(&page_no).ok_or_else(|| anyhow!("page {page_no} out of range"))?;
    let codecs = page_font_codecs(doc, page_id);
    let codec = codecs
        .get(&run.font_res)
        .ok_or_else(|| anyhow!("font {} not found in page resources", run.font_res))?;
    let new_bytes = codec.encode(new_text).ok_or_else(|| {
        anyhow!(
            "this font's encoding can't represent that text yet — try characters that already \
             appear in the document (full font re-embedding is on the roadmap)"
        )
    })?;
    let widths = page_font_widths(doc, page_id);

    let content_bytes = doc.get_page_content(page_id);
    let mut content = Content::decode(&content_bytes).map_err(|e| anyhow!("couldn't parse page content: {e}"))?;

    let op: &mut Operation = content
        .operations
        .get_mut(run.op_index)
        .ok_or_else(|| anyhow!("content changed since runs were listed — reload the page"))?;

    let target: Option<&mut Object> = match run.tj_item {
        None => op.operands.iter_mut().rev().find(|o| matches!(o, Object::String(..))),
        Some(idx) => match op.operands.first_mut() {
            Some(Object::Array(items)) => items.get_mut(idx),
            _ => None,
        },
    };
    let old_bytes = match target {
        Some(Object::String(bytes, _format)) => std::mem::replace(bytes, new_bytes.clone()),
        _ => return Err(anyhow!("run no longer matches the content stream — reload the page")),
    };

    reflow_line(&mut content.operations, run.op_index, run.tj_item, &old_bytes, &new_bytes, &widths);

    let encoded = content.encode().map_err(|e| anyhow!("couldn't re-encode page content: {e}"))?;
    doc.change_page_content(page_id, encoded)
        .map_err(|e| anyhow!("couldn't write page content: {e}"))?;
    Ok(())
}

/// After an in-place string edit changed a run's advance, shift what remains
/// of its line so neighbors don't overlap, and cancel the shift at the next
/// line break so following lines stay put.
///
/// Positioning model: `Td`/`TD` translate the line matrix, so patching one
/// shifts everything after it in the block — hence the compensating `-delta`
/// on the first op that leaves the baseline (or an inserted `Td` before
/// operand-less breaks like `T*`). Strings later in the same `TJ` array flow
/// from the new width automatically and need no patch.
///
/// Best-effort by design: bails without touching anything when widths are
/// unknown or a rotated/scaled text matrix is involved.
fn reflow_line(
    ops: &mut Vec<Operation>,
    edit_op: usize,
    edit_tj: Option<usize>,
    old_bytes: &[u8],
    new_bytes: &[u8],
    widths: &HashMap<String, GlyphWidths>,
) {
    const EPS: f32 = 0.05;
    let opnum = |op: &Operation, i: usize| op.operands.get(i).and_then(num).unwrap_or(0.0);

    // Pass 1: reconstruct text state and the edit point's baseline position.
    let mut font_res = String::new();
    let mut st = AdvanceState::default();
    let mut tlm = (0.0f32, 0.0f32);
    let mut leading = 0.0f32;
    let mut pure = true; // no rotated/scaled Tm in the current BT block
    let mut edit_pos = None;

    for (i, op) in ops.iter().enumerate() {
        match op.operator.as_str() {
            "BT" => {
                tlm = (0.0, 0.0);
                pure = true;
            }
            "Tf" => {
                if let Some(Object::Name(n)) = op.operands.first() {
                    font_res = String::from_utf8_lossy(n).into_owned();
                }
                if let Some(sz) = op.operands.get(1).and_then(num) {
                    st.font_size = sz;
                }
            }
            "Tc" => st.char_spacing = opnum(op, 0),
            "Tw" => st.word_spacing = opnum(op, 0),
            "Tz" => st.h_scale = opnum(op, 0),
            "TL" => leading = opnum(op, 0),
            "Tm" => {
                if op.operands.len() == 6 {
                    let (a, b, c, d) = (opnum(op, 0), opnum(op, 1), opnum(op, 2), opnum(op, 3));
                    if (a - 1.0).abs() > EPS || b.abs() > EPS || c.abs() > EPS || (d - 1.0).abs() > EPS {
                        pure = false;
                    }
                    tlm = (opnum(op, 4), opnum(op, 5));
                }
            }
            "Td" | "TD" => {
                let (tx, ty) = (opnum(op, 0), opnum(op, 1));
                if op.operator == "TD" {
                    leading = -ty;
                }
                tlm = (tlm.0 + tx, tlm.1 + ty);
            }
            "T*" | "'" => tlm.1 -= leading,
            "\"" => {
                st.word_spacing = opnum(op, 0);
                st.char_spacing = opnum(op, 1);
                tlm.1 -= leading;
            }
            _ => {}
        }
        if i == edit_op {
            let mut x = tlm.0;
            if let Some(k) = edit_tj {
                // Advance across the TJ items before the edited one.
                let Some(gw) = widths.get(&font_res) else { return };
                let Some(Object::Array(items)) = op.operands.first() else { return };
                for item in items.iter().take(k) {
                    match item {
                        Object::String(bytes, _) => match advance(gw, bytes, &st) {
                            Some(a) => x += a,
                            None => return,
                        },
                        other => {
                            if let Some(n) = num(other) {
                                x += tj_number_advance(n, &st);
                            }
                        }
                    }
                }
            }
            edit_pos = Some((x, tlm.1));
            break;
        }
    }

    let Some((x_e, y_e)) = edit_pos else { return };
    if !pure {
        return;
    }
    let Some(gw) = widths.get(&font_res) else { return };
    let (Some(old_adv), Some(new_adv)) = (advance(gw, old_bytes, &st), advance(gw, new_bytes, &st))
    else {
        return;
    };
    let delta = new_adv - old_adv;
    if delta.abs() < 0.005 {
        return;
    }

    // Pass 2: walk the rest of the block, shifting same-line positioning ops
    // and cancelling the shift at the first op that leaves the baseline.
    let mut shift_active = false;
    let mut inserts: Vec<(usize, Operation)> = Vec::new();

    for i in (edit_op + 1)..ops.len() {
        match ops[i].operator.clone().as_str() {
            "BT" | "ET" => break,
            "TL" => leading = opnum(&ops[i], 0),
            "Tm" => {
                if ops[i].operands.len() != 6 {
                    break;
                }
                let (a, b, c, d) = (opnum(&ops[i], 0), opnum(&ops[i], 1), opnum(&ops[i], 2), opnum(&ops[i], 3));
                if (a - 1.0).abs() > EPS || b.abs() > EPS || c.abs() > EPS || (d - 1.0).abs() > EPS {
                    // Rotated/scaled reset: absolute, so nothing to undo — stop.
                    break;
                }
                let (e, f) = (opnum(&ops[i], 4), opnum(&ops[i], 5));
                tlm = (e, f);
                if (f - y_e).abs() < EPS && e >= x_e - EPS {
                    ops[i].operands[4] = (e + delta).into();
                    shift_active = true;
                } else {
                    shift_active = false;
                }
            }
            "Td" | "TD" => {
                let (tx, ty) = (opnum(&ops[i], 0), opnum(&ops[i], 1));
                if ops[i].operator == "TD" {
                    leading = -ty;
                }
                let land = (tlm.0 + tx, tlm.1 + ty);
                if shift_active {
                    if (land.1 - y_e).abs() > EPS && !ops[i].operands.is_empty() {
                        ops[i].operands[0] = (tx - delta).into();
                        shift_active = false;
                    }
                } else if (land.1 - y_e).abs() < EPS && land.0 >= x_e - EPS && !ops[i].operands.is_empty() {
                    ops[i].operands[0] = (tx + delta).into();
                    shift_active = true;
                }
                tlm = land;
            }
            "T*" | "'" | "\"" => {
                let land = (tlm.0, tlm.1 - leading);
                if shift_active && (land.1 - y_e).abs() > EPS {
                    inserts.push((i, Operation::new("Td", vec![(-delta).into(), 0.0f32.into()])));
                    shift_active = false;
                } else if !shift_active && (land.1 - y_e).abs() < EPS && land.0 >= x_e - EPS {
                    inserts.push((i, Operation::new("Td", vec![delta.into(), 0.0f32.into()])));
                    shift_active = true;
                }
                tlm = land;
            }
            _ => {}
        }
    }
    for (idx, op) in inserts.into_iter().rev() {
        ops.insert(idx, op);
    }
}

/// Find & replace across a page's editable runs. Returns replacement count.
pub fn find_replace(doc: &mut Document, page_no: u32, find: &str, replace: &str) -> Result<u32> {
    let mut count = 0;
    // Re-list runs after each edit: op indices stay stable because we patch
    // strings in place, but be defensive anyway.
    loop {
        let runs = text_runs(doc, page_no)?;
        let Some(hit) = runs.iter().find(|r| r.editable && r.text.contains(find)) else {
            break;
        };
        let new_text = hit.text.replace(find, replace);
        edit_run(doc, page_no, hit, &new_text)?;
        count += 1;
        if count > 10_000 {
            return Err(anyhow!("replacement loop runaway — aborting"));
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::load;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
    }

    #[test]
    fn lists_runs_with_positions() {
        let doc = load(fixture("sample-a.pdf")).unwrap();
        let runs = text_runs(&doc, 1).unwrap();
        assert!(!runs.is_empty());
        let secret = runs.iter().find(|r| r.text.contains("SECRET")).expect("secret run present");
        assert!(secret.editable, "standard-font run should be editable");
        assert!(secret.font_size > 0.0);
        assert!(secret.y > 0.0);
    }

    #[test]
    fn edits_a_run_in_place() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let runs = text_runs(&doc, 1).unwrap();
        let secret = runs.iter().find(|r| r.text.contains("SECRET")).unwrap().clone();
        edit_run(&mut doc, 1, &secret, "AXED-BY-RUST-99").unwrap();
        let after = crate::ops::extract_text(&doc, 1).unwrap();
        assert!(after.contains("AXED-BY-RUST-99"), "got: {after}");
        assert!(!after.contains("SECRET-CODE-42"));
    }

    #[test]
    fn find_replace_counts() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let n = find_replace(&mut doc, 1, "fox", "wolf").unwrap();
        assert_eq!(n, 1);
        let after = crate::ops::extract_text(&doc, 1).unwrap();
        assert!(after.contains("wolf"));
    }

    #[test]
    fn rejects_unencodable_text() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let runs = text_runs(&doc, 1).unwrap();
        let secret = runs.iter().find(|r| r.text.contains("SECRET")).unwrap().clone();
        assert!(edit_run(&mut doc, 1, &secret, "日本語テキスト").is_err());
    }

    /// Minimal one-page document with the given font as /F1.
    fn one_page_doc(font: Dictionary, content: &[u8]) -> Document {
        use lopdf::{dictionary, Stream};
        let mut doc = Document::with_version("1.5");
        let font_id = doc.add_object(font);
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => Object::Reference(font_id) },
            },
            "Contents" => Object::Reference(content_id),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc
    }

    #[test]
    fn differences_encoding_decodes_and_edits() {
        use lopdf::dictionary;
        // Code 65 ('A' in ASCII) remapped to /eacute.
        let font = dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "Encoding" => dictionary! {
                "Type" => "Encoding",
                "Differences" => vec![Object::Integer(65), Object::Name(b"eacute".to_vec())],
            },
        };
        let mut doc = one_page_doc(font, b"BT /F1 12 Tf 50 700 Td (A) Tj ET");
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "\u{e9}", "code 65 must decode via Differences");
        assert!(runs[0].editable);

        edit_run(&mut doc, 1, &runs[0], "éé").unwrap();
        let after = text_runs(&doc, 1).unwrap();
        assert_eq!(after[0].text, "éé");
        // 'A' no longer exists in this font's encoding — must refuse.
        assert!(edit_run(&mut doc, 1, &after[0], "A").is_err());
    }

    #[test]
    fn macroman_encoding_decodes() {
        use lopdf::dictionary;
        let font = dictionary! {
            "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "Helvetica",
            "Encoding" => "MacRomanEncoding",
        };
        let doc = one_page_doc(font, b"BT /F1 12 Tf 50 700 Td (caf\x8E) Tj ET");
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs[0].text, "café");
        assert!(runs[0].editable);
    }

    #[test]
    fn standard_encoding_maps_quotes() {
        use lopdf::dictionary;
        let font = dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Times-Roman",
            "Encoding" => "StandardEncoding",
        };
        let doc = one_page_doc(font, b"BT /F1 12 Tf 50 700 Td (it's) Tj ET");
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs[0].text, "it\u{2019}s", "0x27 is quoteright in StandardEncoding");
    }

    #[test]
    fn symbolic_font_is_read_only() {
        use lopdf::dictionary;
        let font = dictionary! {
            "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "Wingdings",
            "FontDescriptor" => dictionary! { "Flags" => 4 },
        };
        let doc = one_page_doc(font, b"BT /F1 12 Tf 50 700 Td (Hi) Tj ET");
        let runs = text_runs(&doc, 1).unwrap();
        assert!(!runs[0].editable, "symbolic fonts must refuse edits");
    }

    fn helv() -> Dictionary {
        use lopdf::dictionary;
        dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        }
    }

    /// Width of `text` in Helvetica at `size`, via the same metrics tables.
    fn helv_w(text: &str, size: f32) -> f32 {
        let gw = crate::metrics::std14_widths(b"Helvetica").unwrap();
        let st = crate::metrics::AdvanceState { font_size: size, ..Default::default() };
        crate::metrics::advance(&gw, text.as_bytes(), &st).unwrap()
    }

    #[test]
    fn reflow_shifts_rest_of_line_and_spares_next_line() {
        let content: &[u8] = b"BT /F1 12 Tf 50 700 Td (AB) Tj 60 0 Td (CD) Tj 0 -20 Td (EF) Tj ET";
        let mut doc = one_page_doc(helv(), content);
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs.len(), 3);
        assert!((runs[1].x - 110.0).abs() < 0.01);

        edit_run(&mut doc, 1, &runs[0], "ABAB").unwrap();
        let delta = helv_w("ABAB", 12.0) - helv_w("AB", 12.0);
        assert!(delta > 1.0);

        let after = text_runs(&doc, 1).unwrap();
        let cd = after.iter().find(|r| r.text == "CD").unwrap();
        let ef = after.iter().find(|r| r.text == "EF").unwrap();
        assert!((cd.x - (110.0 + delta)).abs() < 0.01, "CD must shift by {delta}, got x={}", cd.x);
        assert!((cd.y - 700.0).abs() < 0.01);
        assert!((ef.x - 110.0).abs() < 0.01, "next line must not shift, got x={}", ef.x);
        assert!((ef.y - 680.0).abs() < 0.01);
    }

    #[test]
    fn reflow_shrink_pulls_line_left() {
        let content: &[u8] = b"BT /F1 12 Tf 50 700 Td (AB) Tj 60 0 Td (CD) Tj 30 0 Td (EF) Tj ET";
        let mut doc = one_page_doc(helv(), content);
        let runs = text_runs(&doc, 1).unwrap();
        edit_run(&mut doc, 1, &runs[0], "A").unwrap();
        let delta = helv_w("A", 12.0) - helv_w("AB", 12.0);
        assert!(delta < -1.0);

        let after = text_runs(&doc, 1).unwrap();
        assert!((after[1].x - (110.0 + delta)).abs() < 0.01, "got x={}", after[1].x);
        // EF flows with the same line's shift (relative Td chain).
        assert!((after[2].x - (140.0 + delta)).abs() < 0.01, "got x={}", after[2].x);
    }

    #[test]
    fn reflow_tj_edit_shifts_following_td() {
        let content: &[u8] = b"BT /F1 12 Tf 50 700 Td [(AB) -100 (CD)] TJ 200 0 Td (EF) Tj ET";
        let mut doc = one_page_doc(helv(), content);
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs[1].text, "CD");

        edit_run(&mut doc, 1, &runs[1], "CDCD").unwrap();
        let delta = helv_w("CDCD", 12.0) - helv_w("CD", 12.0);

        let after = text_runs(&doc, 1).unwrap();
        let ef = after.iter().find(|r| r.text == "EF").unwrap();
        assert!((ef.x - (250.0 + delta)).abs() < 0.01, "got x={}", ef.x);
    }

    #[test]
    fn reflow_tm_same_line_shifts_only_that_tm() {
        let content: &[u8] = b"BT /F1 12 Tf 1 0 0 1 50 700 Tm (AB) Tj \
                               1 0 0 1 120 700 Tm (CD) Tj 1 0 0 1 50 680 Tm (EF) Tj ET";
        let mut doc = one_page_doc(helv(), content);
        let runs = text_runs(&doc, 1).unwrap();
        edit_run(&mut doc, 1, &runs[0], "ABAB").unwrap();
        let delta = helv_w("ABAB", 12.0) - helv_w("AB", 12.0);

        let after = text_runs(&doc, 1).unwrap();
        let cd = after.iter().find(|r| r.text == "CD").unwrap();
        let ef = after.iter().find(|r| r.text == "EF").unwrap();
        assert!((cd.x - (120.0 + delta)).abs() < 0.01, "got x={}", cd.x);
        assert!((ef.x - 50.0).abs() < 0.01 && (ef.y - 680.0).abs() < 0.01, "absolute Tm lines must stay put");
    }

    #[test]
    fn reflow_skips_unmeasurable_fonts() {
        use lopdf::dictionary;
        let font = dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "MysteryFont",
            "Encoding" => "WinAnsiEncoding",
        };
        let content: &[u8] = b"BT /F1 12 Tf 50 700 Td (AB) Tj 60 0 Td (CD) Tj ET";
        let mut doc = one_page_doc(font, content);
        let runs = text_runs(&doc, 1).unwrap();
        edit_run(&mut doc, 1, &runs[0], "ABAB").unwrap();

        let after = text_runs(&doc, 1).unwrap();
        assert_eq!(after[0].text, "ABAB", "edit itself must still land");
        assert!((after[1].x - 110.0).abs() < 0.01, "no metrics -> no shift, got x={}", after[1].x);
    }
}
