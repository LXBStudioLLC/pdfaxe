//! Text editing, step 1 of the wedge: extract positioned text runs from page
//! content streams, decode them through the font's encoding, re-encode edited
//! strings, and patch the content stream in place.
//!
//! Scope (stated honestly, enforced in code):
//! - Simple single-byte fonts (Type1/TrueType with WinAnsi/Standard encodings
//!   or a ToUnicode CMap) are editable. Composite (Type0/CID) fonts are
//!   displayed but marked non-editable unless their ToUnicode map inverts.
//! - An edit keeps the run's start position. When the font's glyph widths are
//!   known (/Widths, /W, or a standard-14 face), the rest of the line shifts
//!   to fit the new width and the shift is undone at the next line break —
//!   same-line reflow; recognizable wrapped prose re-wraps whole paragraphs.
//! - Text the run's font can't encode no longer refuses: a matching system
//!   font is subset-embedded and the run switches to it (see [`crate::fonts`]).
//! - Positions include within-line advances whenever glyph widths are known;
//!   otherwise they fall back to the show op's start position.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, StringFormat};

use crate::fonts::{embed_replacement, FontHints};
use crate::metrics::{
    advance, page_font_widths, tj_number_advance, widths_from_resources, AdvanceState, GlyphWidths,
};
use crate::ops::resolve_inherited;

/// One show-text operation (or one string element inside a TJ array).
#[derive(Debug, Clone)]
pub struct TextRun {
    /// Index of the operation in the decoded content stream (the page's, or
    /// the form XObject's when `in_form`).
    pub op_index: usize,
    /// For TJ: index of the string element inside the operand array.
    pub tj_item: Option<usize>,
    /// Font resource name (e.g. "F0").
    pub font_res: String,
    /// Nominal size from `Tf` — what a replacement `Tf` must repeat.
    pub font_size: f32,
    /// Size as rendered: `font_size` × text-matrix scale × CTM scale.
    pub display_size: f32,
    /// Baseline origin at the start of the run in PDF user space (text
    /// matrix and CTM applied) — directly comparable with page geometry.
    pub x: f32,
    pub y: f32,
    /// Advance width in user-space points, when the font is measurable.
    pub width: Option<f32>,
    pub text: String,
    pub editable: bool,
    /// Drawn through a form XObject (`Do`). Listed for search and display,
    /// but not editable in place.
    pub in_form: bool,
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

/// Lexical pieces of a CMap that matter for ToUnicode parsing.
enum CTok<'a> {
    Hex(Vec<u8>),
    Open,
    Close,
    Kw(&'a [u8]),
    Other,
}

fn is_delim_or_ws(c: u8) -> bool {
    matches!(
        c,
        b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0' | b'<' | b'>' | b'[' | b']' | b'(' | b')' | b'/' | b'%' | b'{' | b'}'
    )
}

/// `<hex digits>` → bytes; an odd trailing digit is padded with 0 as the
/// PDF spec prescribes.
fn hex_bytes(hex: &[u8]) -> Option<Vec<u8>> {
    if hex.is_empty() {
        return None;
    }
    let mut h = hex.to_vec();
    if h.len() % 2 == 1 {
        h.push(b'0');
    }
    let mut bytes = Vec::with_capacity(h.len() / 2);
    for pair in h.chunks(2) {
        let s = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(bytes)
}

/// Tokenize a CMap stream: hex strings, array brackets, bare keywords.
/// Dictionaries, literal strings, names and numbers are skipped as a unit so
/// a `<< … >>` in `/CIDSystemInfo` never reads as a hex string.
fn cmap_tokens(data: &[u8]) -> Vec<CTok<'_>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        match data[i] {
            b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0' => i += 1,
            b'%' => {
                while i < data.len() && data[i] != b'\n' && data[i] != b'\r' {
                    i += 1;
                }
            }
            b'<' => {
                if data.get(i + 1) == Some(&b'<') {
                    let mut depth = 0i32;
                    while i < data.len() {
                        if data[i..].starts_with(b"<<") {
                            depth += 1;
                            i += 2;
                        } else if data[i..].starts_with(b">>") {
                            depth -= 1;
                            i += 2;
                            if depth <= 0 {
                                break;
                            }
                        } else {
                            i += 1;
                        }
                    }
                    out.push(CTok::Other);
                } else {
                    i += 1;
                    let start = i;
                    while i < data.len() && data[i] != b'>' {
                        i += 1;
                    }
                    let hex: Vec<u8> = data[start..i].iter().copied().filter(|c| c.is_ascii_hexdigit()).collect();
                    i += 1; // '>'
                    match hex_bytes(&hex) {
                        Some(b) => out.push(CTok::Hex(b)),
                        None => out.push(CTok::Other),
                    }
                }
            }
            b'[' => {
                out.push(CTok::Open);
                i += 1;
            }
            b']' => {
                out.push(CTok::Close);
                i += 1;
            }
            b'(' => {
                let mut depth = 0i32;
                while i < data.len() {
                    match data[i] {
                        b'\\' => i += 2,
                        b'(' => {
                            depth += 1;
                            i += 1;
                        }
                        b')' => {
                            depth -= 1;
                            i += 1;
                            if depth <= 0 {
                                break;
                            }
                        }
                        _ => i += 1,
                    }
                }
                out.push(CTok::Other);
            }
            b'/' => {
                i += 1;
                while i < data.len() && !is_delim_or_ws(data[i]) {
                    i += 1;
                }
                out.push(CTok::Other);
            }
            b'{' | b'}' | b'>' | b')' => i += 1,
            _ => {
                let start = i;
                while i < data.len() && !is_delim_or_ws(data[i]) {
                    i += 1;
                }
                let tok = &data[start..i];
                if tok.first().is_some_and(|c| c.is_ascii_alphabetic()) {
                    out.push(CTok::Kw(tok));
                } else {
                    out.push(CTok::Other);
                }
            }
        }
    }
    out
}

fn utf16be_to_string(bytes: &[u8]) -> Option<String> {
    if bytes.len() % 2 != 0 {
        // single byte destination: treat as Latin-1
        return Some(bytes.iter().map(|&b| b as char).collect());
    }
    let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16(&units).ok()
}

/// The destination of a bfrange with a single `<dst>`: the last scalar is
/// incremented by the code's offset from the range start.
fn bump_last(dst: &[u8], offset: usize) -> Option<String> {
    let mut s = utf16be_to_string(dst)?;
    let last = s.pop()?;
    let bumped = char::from_u32(last as u32 + offset as u32)?;
    s.push(bumped);
    Some(s)
}

/// Parse a ToUnicode CMap's codespacerange/bfchar/bfrange sections into a
/// codec. Both bfrange forms are handled: `<lo> <hi> <dst>` and
/// `<lo> <hi> [<d1> <d2> …]`.
fn tounicode_codec(cmap: &[u8]) -> Option<FontCodec> {
    let toks = cmap_tokens(cmap);
    let mut to_uni: HashMap<Vec<u8>, String> = HashMap::new();
    let mut code_len = 0usize;

    let mut i = 0usize;
    while i < toks.len() {
        match &toks[i] {
            CTok::Kw(k) if *k == b"begincodespacerange" => {
                i += 1;
                while let (Some(CTok::Hex(lo)), Some(CTok::Hex(_))) = (toks.get(i), toks.get(i + 1)) {
                    code_len = code_len.max(lo.len());
                    i += 2;
                }
            }
            CTok::Kw(k) if *k == b"beginbfchar" => {
                i += 1;
                loop {
                    match (toks.get(i), toks.get(i + 1)) {
                        (Some(CTok::Hex(src)), Some(CTok::Hex(dst))) => {
                            if let Some(s) = utf16be_to_string(dst) {
                                to_uni.insert(src.clone(), s);
                            }
                            i += 2;
                        }
                        // `<src> /glyphname` form: not a Unicode mapping, skip.
                        (Some(CTok::Hex(_)), Some(CTok::Other)) => i += 2,
                        _ => break,
                    }
                }
            }
            CTok::Kw(k) if *k == b"beginbfrange" => {
                i += 1;
                loop {
                    let (Some(CTok::Hex(lo)), Some(CTok::Hex(hi))) = (toks.get(i), toks.get(i + 1)) else { break };
                    let (lo_n, hi_n) = (be_num(lo), be_num(hi));
                    let width = lo.len();
                    if hi_n < lo_n || hi_n - lo_n > 65_535 {
                        break;
                    }
                    match toks.get(i + 2) {
                        Some(CTok::Hex(dst)) => {
                            for (offset, code) in (lo_n..=hi_n).enumerate() {
                                if let Some(s) = bump_last(dst, offset) {
                                    to_uni.insert(num_be(code, width), s);
                                }
                            }
                            i += 3;
                        }
                        Some(CTok::Open) => {
                            let mut j = i + 3;
                            let mut code = lo_n;
                            while let Some(CTok::Hex(dst)) = toks.get(j) {
                                if code <= hi_n {
                                    if let Some(s) = utf16be_to_string(dst) {
                                        to_uni.insert(num_be(code, width), s);
                                    }
                                }
                                code += 1;
                                j += 1;
                            }
                            if matches!(toks.get(j), Some(CTok::Close)) {
                                j += 1;
                            }
                            i = j;
                        }
                        _ => break,
                    }
                }
            }
            _ => i += 1,
        }
    }

    if to_uni.is_empty() {
        return None;
    }
    if code_len == 0 {
        code_len = to_uni.keys().map(|k| k.len()).max().unwrap_or(1);
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
    let Some(res_obj) = resolve_inherited(doc, page_id, b"Resources") else { return HashMap::new() };
    let res_resolved = resolve(doc, &res_obj).clone();
    let Ok(res_dict) = res_resolved.as_dict() else { return HashMap::new() };
    codecs_from_resources(doc, res_dict)
}

/// Build codecs for every font in a resources dictionary (a page's, or a
/// form XObject's own).
fn codecs_from_resources(doc: &Document, res_dict: &Dictionary) -> HashMap<String, FontCodec> {
    let mut out = HashMap::new();
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
    // A Type3 font's codes mean nothing outside its own /Differences: there
    // is no base encoding to fall back on.
    let type3 = subtype == b"Type3";

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
                // encoding: unknowable for symbolic fonts, nonexistent for
                // Type3, ~WinAnsi otherwise.
                if symbolic || type3 { [None; 256] } else { winansi_table() }
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
        _ => table_codec(&winansi_table(), !symbolic && !type3),
    }
}

fn num(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r as f32),
        _ => None,
    }
}

/// 2-D affine matrix in PDF's row-vector convention `[a b c d e f]`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mat {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Mat {
    const IDENTITY: Mat = Mat { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    fn translate(tx: f32, ty: f32) -> Mat {
        Mat { e: tx, f: ty, ..Mat::IDENTITY }
    }

    fn from_operands(ops: &[Object]) -> Option<Mat> {
        if ops.len() < 6 {
            return None;
        }
        Some(Mat {
            a: num(&ops[0])?,
            b: num(&ops[1])?,
            c: num(&ops[2])?,
            d: num(&ops[3])?,
            e: num(&ops[4])?,
            f: num(&ops[5])?,
        })
    }

    /// `self` applied first, then `next` — PDF's `self × next`.
    fn then(&self, next: &Mat) -> Mat {
        Mat {
            a: self.a * next.a + self.b * next.c,
            b: self.a * next.b + self.b * next.d,
            c: self.c * next.a + self.d * next.c,
            d: self.c * next.b + self.d * next.d,
            e: self.e * next.a + self.f * next.c + next.e,
            f: self.e * next.b + self.f * next.d + next.f,
        }
    }

    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f)
    }

    /// Uniform scale factor (geometric mean of the axis scales).
    fn scale(&self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }
}

/// The graphics-state slice `q`/`Q` save and restore: CTM plus text state.
#[derive(Clone)]
struct GState {
    ctm: Mat,
    font_res: String,
    font_size: f32,
    char_sp: f32,
    word_sp: f32,
    h_scale: f32,
    leading: f32,
}

impl Default for GState {
    fn default() -> Self {
        Self {
            ctm: Mat::IDENTITY,
            font_res: String::new(),
            font_size: 0.0,
            char_sp: 0.0,
            word_sp: 0.0,
            h_scale: 100.0,
            leading: 0.0,
        }
    }
}

/// Fonts visible from a content stream: its own resources first, then the
/// enclosing ones (a form XObject without /Resources inherits the page's).
struct FontScope<'a> {
    codecs: Vec<&'a HashMap<String, FontCodec>>,
    widths: Vec<&'a HashMap<String, GlyphWidths>>,
}

impl FontScope<'_> {
    fn codec(&self, name: &str) -> Option<&FontCodec> {
        self.codecs.iter().find_map(|m| m.get(name))
    }
    fn widths(&self, name: &str) -> Option<&GlyphWidths> {
        self.widths.iter().find_map(|m| m.get(name))
    }
}

/// Extract text runs from one page (1-based), including text drawn through
/// form XObjects (listed read-only).
pub fn text_runs(doc: &Document, page_no: u32) -> Result<Vec<TextRun>> {
    let pages = doc.get_pages();
    let &page_id = pages.get(&page_no).ok_or_else(|| anyhow!("page {page_no} out of range"))?;
    let content_bytes = doc.get_page_content(page_id);
    let content = Content::decode(&content_bytes).map_err(|e| anyhow!("couldn't parse page content: {e}"))?;
    let res = resolve_inherited(doc, page_id, b"Resources")
        .and_then(|o| resolve(doc, &o).as_dict().ok().cloned())
        .unwrap_or_default();
    let codecs = codecs_from_resources(doc, &res);
    let widths = widths_from_resources(doc, &res);
    let scope = FontScope { codecs: vec![&codecs], widths: vec![&widths] };

    let mut runs = Vec::new();
    let mut active: Vec<ObjectId> = Vec::new();
    walk_content(doc, &content, &res, &scope, GState::default(), false, 0, &mut active, &mut runs);
    Ok(runs)
}

#[allow(clippy::too_many_arguments)]
fn make_run(
    op_index: usize,
    tj_item: Option<usize>,
    gs: &GState,
    tm: &Mat,
    adv: Option<f32>,
    text: String,
    editable: bool,
    in_form: bool,
) -> TextRun {
    let (x, y) = gs.ctm.apply(tm.e, tm.f);
    let k = tm.scale() * gs.ctm.scale();
    TextRun {
        op_index,
        tj_item,
        font_res: gs.font_res.clone(),
        font_size: gs.font_size,
        display_size: gs.font_size * k,
        x,
        y,
        width: adv.map(|w| w * k),
        text,
        editable: editable && !in_form,
        in_form,
    }
}

/// Walk one content stream, tracking `q`/`Q`/`cm`, the text and line
/// matrices, and recursing into form XObjects on `Do`.
#[allow(clippy::too_many_arguments)]
fn walk_content(
    doc: &Document,
    content: &Content,
    res: &Dictionary,
    scope: &FontScope,
    init: GState,
    in_form: bool,
    depth: u8,
    active: &mut Vec<ObjectId>,
    runs: &mut Vec<TextRun>,
) {
    let mut gs = init;
    let mut stack: Vec<GState> = Vec::new();
    let mut tm = Mat::IDENTITY;
    let mut tlm = Mat::IDENTITY;

    let decode = |font: &str, bytes: &[u8]| -> (String, bool) {
        match scope.codec(font) {
            Some(c) => (c.decode(bytes), c.invertible),
            None => (String::from_utf8_lossy(bytes).into_owned(), false),
        }
    };

    for (op_index, op) in content.operations.iter().enumerate() {
        match op.operator.as_str() {
            "q" => stack.push(gs.clone()),
            "Q" => {
                if let Some(g) = stack.pop() {
                    gs = g;
                }
            }
            "cm" => {
                if let Some(m) = Mat::from_operands(&op.operands) {
                    gs.ctm = m.then(&gs.ctm);
                }
            }
            "BT" => {
                tm = Mat::IDENTITY;
                tlm = Mat::IDENTITY;
            }
            "Tf" => {
                if let Some(Object::Name(n)) = op.operands.first() {
                    gs.font_res = String::from_utf8_lossy(n).into_owned();
                }
                if let Some(sz) = op.operands.get(1).and_then(num) {
                    gs.font_size = sz;
                }
            }
            "Tm" => {
                if let Some(m) = Mat::from_operands(&op.operands) {
                    tm = m;
                    tlm = m;
                }
            }
            "Td" | "TD" => {
                let tx = op.operands.first().and_then(num).unwrap_or(0.0);
                let ty = op.operands.get(1).and_then(num).unwrap_or(0.0);
                if op.operator == "TD" {
                    gs.leading = -ty;
                }
                tlm = Mat::translate(tx, ty).then(&tlm);
                tm = tlm;
            }
            "TL" => gs.leading = op.operands.first().and_then(num).unwrap_or(0.0),
            "Tc" => gs.char_sp = op.operands.first().and_then(num).unwrap_or(0.0),
            "Tw" => gs.word_sp = op.operands.first().and_then(num).unwrap_or(0.0),
            "Tz" => gs.h_scale = op.operands.first().and_then(num).unwrap_or(100.0),
            "T*" => {
                tlm = Mat::translate(0.0, -gs.leading).then(&tlm);
                tm = tlm;
            }
            "Tj" | "'" | "\"" => {
                if op.operator == "\"" {
                    gs.word_sp = op.operands.first().and_then(num).unwrap_or(gs.word_sp);
                    gs.char_sp = op.operands.get(1).and_then(num).unwrap_or(gs.char_sp);
                }
                if op.operator != "Tj" {
                    tlm = Mat::translate(0.0, -gs.leading).then(&tlm);
                    tm = tlm;
                }
                let string_operand = op.operands.iter().rev().find_map(|o| match o {
                    Object::String(bytes, _) => Some(bytes.clone()),
                    _ => None,
                });
                if let Some(bytes) = string_operand {
                    let st = AdvanceState {
                        font_size: gs.font_size,
                        char_spacing: gs.char_sp,
                        word_spacing: gs.word_sp,
                        h_scale: gs.h_scale,
                    };
                    let adv = scope.widths(&gs.font_res).and_then(|gw| advance(gw, &bytes, &st));
                    let (text, editable) = decode(&gs.font_res, &bytes);
                    runs.push(make_run(op_index, None, &gs, &tm, adv, text, editable, in_form));
                    if let Some(w) = adv {
                        tm = Mat::translate(w, 0.0).then(&tm);
                    }
                }
            }
            "TJ" => {
                if let Some(Object::Array(items)) = op.operands.first() {
                    let st = AdvanceState {
                        font_size: gs.font_size,
                        char_spacing: gs.char_sp,
                        word_spacing: gs.word_sp,
                        h_scale: gs.h_scale,
                    };
                    // Track the advance across items so each string gets its
                    // real start x; falls back to the TJ's start when a width
                    // is unknowable.
                    let mut acc: Option<f32> = Some(0.0);
                    for (tj_item, item) in items.iter().enumerate() {
                        match item {
                            Object::String(bytes, _) => {
                                let adv = scope.widths(&gs.font_res).and_then(|gw| advance(gw, bytes, &st));
                                let (text, editable) = decode(&gs.font_res, bytes);
                                let here = Mat::translate(acc.unwrap_or(0.0), 0.0).then(&tm);
                                runs.push(make_run(op_index, Some(tj_item), &gs, &here, adv, text, editable, in_form));
                                acc = match (acc, adv) {
                                    (Some(a), Some(w)) => Some(a + w),
                                    _ => None,
                                };
                            }
                            other => {
                                if let Some(n) = num(other) {
                                    acc = acc.map(|a| a + tj_number_advance(n, &st));
                                }
                            }
                        }
                    }
                    if let Some(a) = acc {
                        tm = Mat::translate(a, 0.0).then(&tm);
                    }
                }
            }
            "Do" => {
                if depth >= 8 {
                    continue;
                }
                let Some(Object::Name(name)) = op.operands.first() else { continue };
                let Some(xobjs) = res.get(b"XObject").ok().map(|o| resolve(doc, o)).and_then(|o| o.as_dict().ok()) else {
                    continue;
                };
                let Ok(entry) = xobjs.get(name) else { continue };
                let id = entry.as_reference().ok();
                if id.is_some_and(|id| active.contains(&id)) {
                    continue; // a form that draws itself — never recurse
                }
                let Object::Stream(form) = resolve(doc, entry) else { continue };
                if form.dict.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) != Some(b"Form") {
                    continue;
                }
                let Ok(data) = form.decompressed_content() else { continue };
                let Ok(inner) = Content::decode(&data) else { continue };
                let matrix = form
                    .dict
                    .get(b"Matrix")
                    .ok()
                    .map(|o| resolve(doc, o))
                    .and_then(|o| o.as_array().ok())
                    .and_then(|a| Mat::from_operands(a))
                    .unwrap_or(Mat::IDENTITY);
                let form_res = form
                    .dict
                    .get(b"Resources")
                    .ok()
                    .map(|o| resolve(doc, o))
                    .and_then(|o| o.as_dict().ok())
                    .cloned();
                // `Do` runs the form inside an implicit q…Q with its /Matrix
                // prepended to the CTM; the text state is inherited.
                let mut inner_gs = gs.clone();
                inner_gs.ctm = matrix.then(&gs.ctm);
                if let Some(id) = id {
                    active.push(id);
                }
                match form_res {
                    Some(fr) => {
                        let codecs = codecs_from_resources(doc, &fr);
                        let widths = widths_from_resources(doc, &fr);
                        let mut c = vec![&codecs];
                        c.extend(scope.codecs.iter().copied());
                        let mut w = vec![&widths];
                        w.extend(scope.widths.iter().copied());
                        let inner_scope = FontScope { codecs: c, widths: w };
                        walk_content(doc, &inner, &fr, &inner_scope, inner_gs, true, depth + 1, active, runs);
                    }
                    None => walk_content(doc, &inner, res, scope, inner_gs, true, depth + 1, active, runs),
                }
                if id.is_some() {
                    active.pop();
                }
            }
            _ => {}
        }
    }
}

/// What an edit did beyond changing the string.
#[derive(Debug, Default, Clone)]
pub struct EditOutcome {
    /// Set when the text needed glyphs the run's own font couldn't encode, so
    /// a system font was subset-embedded and the run switched to it. Carries
    /// the human-facing name of the stand-in ("Arial Bold").
    pub refont: Option<String>,
}

/// Replace the text of one run (same font) and patch the page's content
/// stream. Layout preservation, best first:
/// 1. Paragraph re-wrap — when the run is a line of recognizable wrapped
///    prose, words redistribute across the paragraph's existing lines.
/// 2. Same-line reflow — otherwise, when glyph widths are known, the rest of
///    the run's line shifts by the width delta and un-shifts at the next
///    line break.
/// 3. In-place patch — when widths are unknown or the text matrix is rotated.
/// When the font's encoding can't express the new text at all, a matching
/// system font is subset-embedded and just this run switches to it (see
/// `fonts`); the returned [`EditOutcome`] says so.
/// Content-stream op indices may change — re-list runs after every edit.
pub fn edit_run(doc: &mut Document, page_no: u32, run: &TextRun, new_text: &str) -> Result<EditOutcome> {
    if run.in_form {
        return Err(anyhow!(
            "this text is drawn through a form XObject — editing it in place isn't supported yet"
        ));
    }
    let pages = doc.get_pages();
    let &page_id = pages.get(&page_no).ok_or_else(|| anyhow!("page {page_no} out of range"))?;
    let codecs = page_font_codecs(doc, page_id);
    let codec = codecs
        .get(&run.font_res)
        .ok_or_else(|| anyhow!("font {} not found in page resources", run.font_res))?;
    let Some(new_bytes) = codec.encode(new_text) else {
        return refont_edit(doc, page_id, run, new_text);
    };
    let widths = page_font_widths(doc, page_id);

    let content_bytes = doc.get_page_content(page_id);
    let mut content = Content::decode(&content_bytes).map_err(|e| anyhow!("couldn't parse page content: {e}"))?;

    if try_rewrap(&mut content.operations, run.op_index, run.tj_item, new_text, codec, &widths).is_some() {
        let encoded = content.encode().map_err(|e| anyhow!("couldn't re-encode page content: {e}"))?;
        doc.change_page_content(page_id, encoded)
            .map_err(|e| anyhow!("couldn't write page content: {e}"))?;
        return Ok(EditOutcome::default());
    }

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

    reflow_line(&mut content.operations, run.op_index, run.tj_item, &old_bytes, &new_bytes, &widths, None);

    let encoded = content.encode().map_err(|e| anyhow!("couldn't re-encode page content: {e}"))?;
    doc.change_page_content(page_id, encoded)
        .map_err(|e| anyhow!("couldn't write page content: {e}"))?;
    Ok(EditOutcome::default())
}

/// Fallback for text the run's own font can't encode: subset-embed a matching
/// system font and switch just this run to it. The show op is re-encoded as
/// 2-byte CIDs wrapped in `Tf` switches; when the run is one string of a TJ
/// array, the array is split around it so every kern number keeps its place.
fn refont_edit(doc: &mut Document, page_id: ObjectId, run: &TextRun, new_text: &str) -> Result<EditOutcome> {
    let (base, hints) = font_identity(doc, page_id, &run.font_res);
    let chars: BTreeSet<char> = new_text.chars().collect();
    let embedded = embed_replacement(doc, &base, hints, &chars).map_err(|e| {
        anyhow!("this font's encoding can't represent that text, and no installed font could stand in ({e})")
    })?;
    let res_name = add_page_font(doc, page_id, embedded.font_id)?;

    let mut new_bytes = Vec::with_capacity(new_text.len() * 2);
    for ch in new_text.chars() {
        // embed_replacement guarantees coverage of every char in `chars`.
        new_bytes.extend_from_slice(&embedded.encode[&ch].to_be_bytes());
    }

    let content_bytes = doc.get_page_content(page_id);
    let mut content = Content::decode(&content_bytes).map_err(|e| anyhow!("couldn't parse page content: {e}"))?;
    let ops = &mut content.operations;

    let stale = || anyhow!("content changed since runs were listed — reload the page");
    let tf = |name: &str, size: f32| {
        Operation::new("Tf", vec![Object::Name(name.as_bytes().to_vec()), size.into()])
    };

    let (edit_idx, edit_tj, old_bytes);
    match run.tj_item {
        None => {
            let op = ops.get_mut(run.op_index).ok_or_else(stale)?;
            let target = op.operands.iter_mut().rev().find(|o| matches!(o, Object::String(..)));
            let Some(Object::String(bytes, format)) = target else { return Err(stale()) };
            old_bytes = std::mem::replace(bytes, new_bytes.clone());
            *format = StringFormat::Hexadecimal;
            ops.insert(run.op_index, tf(&res_name, run.font_size));
            ops.insert(run.op_index + 2, tf(&run.font_res, run.font_size));
            edit_idx = run.op_index + 1;
            edit_tj = None;
        }
        Some(k) => {
            let op = ops.get(run.op_index).ok_or_else(stale)?;
            if op.operator != "TJ" {
                return Err(stale());
            }
            let Some(Object::Array(items)) = op.operands.first() else { return Err(stale()) };
            let Some(Object::String(old, _)) = items.get(k) else { return Err(stale()) };
            old_bytes = old.clone();
            let prefix = items[..k].to_vec();
            let suffix = items[k + 1..].to_vec();
            let mut seq = Vec::with_capacity(5);
            if !prefix.is_empty() {
                seq.push(Operation::new("TJ", vec![Object::Array(prefix)]));
            }
            seq.push(tf(&res_name, run.font_size));
            let rel = seq.len();
            seq.push(Operation::new(
                "TJ",
                vec![Object::Array(vec![Object::String(new_bytes.clone(), StringFormat::Hexadecimal)])],
            ));
            seq.push(tf(&run.font_res, run.font_size));
            if !suffix.is_empty() {
                seq.push(Operation::new("TJ", vec![Object::Array(suffix)]));
            }
            ops.splice(run.op_index..=run.op_index, seq);
            edit_idx = run.op_index + rel;
            edit_tj = Some(0);
        }
    }

    // Both fonts are measurable now (the embedded one through its /W array),
    // so the usual same-line reflow applies across the font switch.
    let widths = page_font_widths(doc, page_id);
    reflow_line(ops, edit_idx, edit_tj, &old_bytes, &new_bytes, &widths, Some(&run.font_res));

    let encoded = content.encode().map_err(|e| anyhow!("couldn't re-encode page content: {e}"))?;
    doc.change_page_content(page_id, encoded)
        .map_err(|e| anyhow!("couldn't write page content: {e}"))?;
    Ok(EditOutcome { refont: Some(embedded.label) })
}

/// BaseFont name + style hints for one font resource on a page, feeding the
/// system-font matcher.
fn font_identity(doc: &Document, page_id: ObjectId, font_res: &str) -> (String, FontHints) {
    let mut base = String::new();
    let mut hints = FontHints::default();
    let font = (|| -> Option<Dictionary> {
        let res = resolve_inherited(doc, page_id, b"Resources")?;
        let res = resolve(doc, &res).clone();
        let fonts = res.as_dict().ok()?.get(b"Font").ok().cloned()?;
        let fonts = resolve(doc, &fonts).clone();
        let font = fonts.as_dict().ok()?.get(font_res.as_bytes()).ok().cloned()?;
        resolve(doc, &font).as_dict().ok().cloned()
    })();
    let Some(font) = font else { return (base, hints) };
    if let Ok(b) = font.get(b"BaseFont") {
        if let Ok(name) = resolve(doc, b).as_name() {
            base = String::from_utf8_lossy(name).into_owned();
        }
    }
    if let Some(desc) = font
        .get(b"FontDescriptor")
        .ok()
        .map(|o| resolve(doc, o))
        .and_then(|o| o.as_dict().ok())
    {
        if let Some(flags) = desc.get(b"Flags").ok().and_then(|f| f.as_i64().ok()) {
            hints.fixed = flags & 1 != 0;
            hints.serif = flags & 2 != 0;
            hints.italic = flags & 64 != 0;
        }
        if let Some(angle) = desc.get(b"ItalicAngle").ok().and_then(num) {
            hints.italic |= angle.abs() > 0.1;
        }
        if let Some(stem) = desc.get(b"StemV").ok().and_then(num) {
            hints.bold |= stem >= 120.0;
        }
        if let Some(weight) = desc.get(b"FontWeight").ok().and_then(num) {
            hints.bold |= weight >= 600.0;
        }
    }
    (base, hints)
}

/// Register `font_id` under a fresh name in the page's /Resources /Font dict.
/// The resolved resources are cloned onto the page itself, so inherited or
/// shared dictionaries on other pages stay untouched.
fn add_page_font(doc: &mut Document, page_id: ObjectId, font_id: ObjectId) -> Result<String> {
    let res = resolve_inherited(doc, page_id, b"Resources");
    let mut res_dict: Dictionary = match res.as_ref().map(|o| resolve(doc, o)) {
        Some(Object::Dictionary(d)) => d.clone(),
        _ => Dictionary::new(),
    };
    let mut font_dict: Dictionary = match res_dict.get(b"Font").ok().map(|o| resolve(doc, o).clone()) {
        Some(Object::Dictionary(d)) => d,
        _ => Dictionary::new(),
    };
    let mut n = 0u32;
    let name = loop {
        let cand = format!("PXF{n}");
        if font_dict.get(cand.as_bytes()).is_err() {
            break cand;
        }
        n += 1;
    };
    font_dict.set(name.clone(), Object::Reference(font_id));
    res_dict.set("Font", Object::Dictionary(font_dict));
    let page = doc
        .get_object_mut(page_id)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("page dictionary unavailable: {e}"))?;
    page.set("Resources", Object::Dictionary(res_dict));
    Ok(name)
}

/// One whole-line show op, as collected for paragraph re-wrap.
struct BlockLine {
    op_index: usize,
    /// `Some(0)` when the line is a single-string TJ.
    tj_item: Option<usize>,
    x: f32,
    y: f32,
    font_res: String,
    st: AdvanceState,
    bytes: Vec<u8>,
    /// One string, translation-only matrices — safe to redistribute into.
    simple: bool,
}

/// Word-processor-style paragraph re-wrap: replace the edited line's text and
/// redistribute words across the *existing* lines of its paragraph so nothing
/// overflows the paragraph's width. Only strings change — every line keeps
/// its positioning ops — so the failure mode is refusing, never mangling.
///
/// A paragraph is only recognized when the evidence is strong: consecutive
/// one-string lines sharing font, size and left margin, at uniform leading,
/// with every line but the last near-full (ragged line-lists like addresses
/// fail that test and fall back to same-line reflow). Overflow that no longer
/// fits by the paragraph's last line stays on that line.
///
/// Returns `None` (without touching anything) when the block doesn't qualify.
fn try_rewrap(
    ops: &mut [Operation],
    edit_op: usize,
    edit_tj: Option<usize>,
    new_text: &str,
    codec: &FontCodec,
    widths: &HashMap<String, GlyphWidths>,
) -> Option<()> {
    const X_EPS: f32 = 0.5;
    const STEP_EPS: f32 = 0.5;
    /// Lines this fraction of the block width count as "full" (wrapped).
    const FULLNESS: f32 = 0.7;
    let opnum = |op: &Operation, i: usize| op.operands.get(i).and_then(num).unwrap_or(0.0);

    // Collect every whole-line show op in stream order.
    let mut lines: Vec<BlockLine> = Vec::new();
    let mut font_res = String::new();
    let mut st = AdvanceState::default();
    let mut tlm = (0.0f32, 0.0f32);
    let mut leading = 0.0f32;
    let mut pure = true;

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
                    if (a - 1.0).abs() > 0.05 || b.abs() > 0.05 || c.abs() > 0.05 || (d - 1.0).abs() > 0.05 {
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
            "T*" => tlm.1 -= leading,
            "Tj" | "'" | "\"" => {
                if op.operator == "\"" {
                    st.word_spacing = opnum(op, 0);
                    st.char_spacing = opnum(op, 1);
                }
                if op.operator != "Tj" {
                    tlm.1 -= leading;
                }
                if let Some(Object::String(bytes, _)) =
                    op.operands.iter().rev().find(|o| matches!(o, Object::String(..)))
                {
                    lines.push(BlockLine {
                        op_index: i,
                        tj_item: None,
                        x: tlm.0,
                        y: tlm.1,
                        font_res: font_res.clone(),
                        st,
                        bytes: bytes.clone(),
                        simple: pure,
                    });
                }
            }
            "TJ" => {
                if let Some(Object::Array(items)) = op.operands.first() {
                    // Only a lone string is a redistributable line; kerned
                    // arrays would lose their adjustments.
                    let lone = match items.as_slice() {
                        [Object::String(bytes, _)] => Some(bytes.clone()),
                        _ => None,
                    };
                    let simple = pure && lone.is_some();
                    lines.push(BlockLine {
                        op_index: i,
                        tj_item: Some(0),
                        x: tlm.0,
                        y: tlm.1,
                        font_res: font_res.clone(),
                        st,
                        bytes: lone.unwrap_or_default(),
                        simple,
                    });
                }
            }
            _ => {}
        }
    }

    // The edited line, by op identity. (A multi-item TJ never matches — its
    // recorded tj_item is 0 with simple=false, and simple gates below.)
    let e = lines
        .iter()
        .position(|l| l.op_index == edit_op && l.tj_item.is_some() == edit_tj.is_some())?;
    if !lines[e].simple || edit_tj.is_some_and(|k| k != 0) {
        return None;
    }
    if !codec.invertible {
        return None;
    }
    let gw = widths.get(&lines[e].font_res)?;
    let st_e = lines[e].st;

    let same_family = |l: &BlockLine| {
        l.simple
            && l.font_res == lines[e].font_res
            && (l.st.font_size - st_e.font_size).abs() < 0.01
            && (l.x - lines[e].x).abs() < X_EPS
    };

    // Uniform leading, inferred from the nearest neighbor.
    let step_ok = |above: &BlockLine, below: &BlockLine, lead: f32| {
        let step = above.y - below.y;
        step > 0.1 && (step - lead).abs() < STEP_EPS
    };
    let lead = if e + 1 < lines.len() && same_family(&lines[e + 1]) && lines[e].y - lines[e + 1].y > 0.1 {
        lines[e].y - lines[e + 1].y
    } else if e > 0 && same_family(&lines[e - 1]) && lines[e - 1].y - lines[e].y > 0.1 {
        lines[e - 1].y - lines[e].y
    } else {
        return None; // no paragraph around this line
    };
    if lead > st_e.font_size * 3.0 {
        return None;
    }

    let mut start = e;
    while start > 0 && same_family(&lines[start - 1]) && step_ok(&lines[start - 1], &lines[start], lead) {
        start -= 1;
    }
    let mut end = e; // inclusive
    while end + 1 < lines.len() && same_family(&lines[end + 1]) && step_ok(&lines[end], &lines[end + 1], lead) {
        end += 1;
    }
    if end == start {
        return None;
    }

    // Paragraph width = the widest line; every line but the last must be
    // near-full or this is a line list, not wrapped prose.
    let mut line_w = Vec::with_capacity(end - start + 1);
    for l in &lines[start..=end] {
        line_w.push(advance(gw, &l.bytes, &st_e)?);
    }
    let w_max = line_w.iter().copied().fold(0.0f32, f32::max);
    if w_max <= 1.0 {
        return None;
    }
    if line_w[..line_w.len() - 1].iter().any(|&w| w < w_max * FULLNESS) {
        return None;
    }

    // Gather words from the edited line onward and refill the slots greedily.
    let mut words: Vec<String> = Vec::new();
    for (k, l) in lines[e..=end].iter().enumerate() {
        let text = if k == 0 { new_text.to_string() } else { codec.decode(&l.bytes) };
        words.extend(text.split_whitespace().map(str::to_owned));
    }

    let slots = end - e + 1;
    let mut new_lines: Vec<String> = Vec::with_capacity(slots);
    let mut wi = 0usize;
    for slot in 0..slots {
        if slot + 1 == slots {
            new_lines.push(words[wi..].join(" "));
            break;
        }
        let mut cur = String::new();
        while wi < words.len() {
            let cand = if cur.is_empty() { words[wi].clone() } else { format!("{cur} {}", words[wi]) };
            let cand_bytes = codec.encode(&cand)?;
            if !cur.is_empty() && advance(gw, &cand_bytes, &st_e)? > w_max + 0.25 {
                break;
            }
            cur = cand;
            wi += 1;
        }
        new_lines.push(cur);
    }

    // Encode everything up front — an unencodable word must refuse the whole
    // re-wrap, not leave the paragraph half-patched.
    let mut encoded = Vec::with_capacity(new_lines.len());
    for t in &new_lines {
        encoded.push(codec.encode(t)?);
    }

    for (l, bytes) in lines[e..=end].iter().zip(encoded) {
        let op = &mut ops[l.op_index];
        let target = match l.tj_item {
            None => op.operands.iter_mut().rev().find(|o| matches!(o, Object::String(..))),
            Some(k) => match op.operands.first_mut() {
                Some(Object::Array(items)) => items.get_mut(k),
                _ => None,
            },
        };
        match target {
            Some(Object::String(b, _)) => *b = bytes,
            _ => return None, // structurally impossible after collection
        }
    }
    Some(())
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
///
/// `old_font` overrides which font's widths measure `old_bytes` — used after a
/// refont, where the edit op now runs in the embedded font but the replaced
/// bytes belonged to the original one.
fn reflow_line(
    ops: &mut Vec<Operation>,
    edit_op: usize,
    edit_tj: Option<usize>,
    old_bytes: &[u8],
    new_bytes: &[u8],
    widths: &HashMap<String, GlyphWidths>,
    old_font: Option<&str>,
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
    let old_gw = match old_font {
        Some(name) => match widths.get(name) {
            Some(g) => g,
            None => return,
        },
        None => gw,
    };
    let (Some(old_adv), Some(new_adv)) = (advance(old_gw, old_bytes, &st), advance(gw, new_bytes, &st))
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

/// Find & replace across a page's editable runs. Returns the number of
/// occurrences replaced (a run holding `find` twice counts twice).
pub fn find_replace(doc: &mut Document, page_no: u32, find: &str, replace: &str) -> Result<u32> {
    if find.is_empty() {
        return Err(anyhow!("nothing to find — the search text is empty"));
    }
    let mut count = 0u32;
    let mut edits = 0u32;
    // Re-list runs after each edit: op indices stay stable because we patch
    // strings in place, but be defensive anyway.
    loop {
        let runs = text_runs(doc, page_no)?;
        let Some(hit) = runs.iter().find(|r| r.editable && r.text.contains(find)) else {
            break;
        };
        let occurrences = hit.text.matches(find).count() as u32;
        let new_text = hit.text.replace(find, replace);
        edit_run(doc, page_no, hit, &new_text)?;
        count += occurrences.max(1);
        edits += 1;
        if edits > 10_000 {
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
    fn rejects_text_no_font_can_carry() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let runs = text_runs(&doc, 1).unwrap();
        let secret = runs.iter().find(|r| r.text.contains("SECRET")).unwrap().clone();
        // U+FFFE is a noncharacter: no font maps it, so even the system-font
        // fallback must refuse rather than emit .notdef boxes.
        assert!(edit_run(&mut doc, 1, &secret, "bad \u{FFFE} char").is_err());
    }

    /// True when the machine has a font covering `text` — refont tests skip
    /// themselves on bare machines instead of failing.
    fn refont_available(text: &str) -> bool {
        let chars: BTreeSet<char> = text.chars().collect();
        let mut probe = Document::with_version("1.7");
        if embed_replacement(&mut probe, "Helvetica", FontHints::default(), &chars).is_err() {
            eprintln!("skipping: no system font covers {text:?}");
            return false;
        }
        true
    }

    #[test]
    fn refont_embeds_replacement_for_foreign_glyphs() {
        let new_text = "Ω→SECRET-Ω";
        if !refont_available(new_text) {
            return;
        }
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let runs = text_runs(&doc, 1).unwrap();
        let secret = runs.iter().find(|r| r.text.contains("SECRET")).unwrap().clone();

        let outcome = edit_run(&mut doc, 1, &secret, new_text).unwrap();
        assert!(outcome.refont.is_some(), "edit must report the embedded stand-in");

        let after = text_runs(&doc, 1).unwrap();
        let run = after
            .iter()
            .find(|r| r.text == new_text)
            .expect("re-listed run must decode via the generated ToUnicode")
            .clone();
        assert!(run.editable, "ToUnicode must keep the run editable");
        assert!(run.width.is_some(), "the /W array must make the run measurable");

        // Survives save/reload, and the refonted run takes further edits —
        // including chars that force a second embed ('A' wasn't in the subset).
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let mut re = Document::load_mem(&buf).unwrap();
        let runs2 = text_runs(&re, 1).unwrap();
        let run2 = runs2.iter().find(|r| r.text == new_text).unwrap().clone();
        edit_run(&mut re, 1, &run2, "Ω→AXE").unwrap();
        assert!(text_runs(&re, 1).unwrap().iter().any(|r| r.text == "Ω→AXE"));

        // And the renderer actually rasterizes the embedded subset: inked
        // (non-white) pixels must appear inside the run's box. The fixture's
        // run is red, so test distance-from-white, not darkness.
        let img = crate::render::render_page_rgba(&buf, 1, 800).unwrap();
        let sx = img.width as f32 / img.point_width;
        let sy = img.height as f32 / img.point_height;
        let (x0, x1) = ((run.x * sx) as usize, ((run.x + run.width.unwrap()) * sx) as usize);
        let y0 = ((img.point_height - run.y - run.font_size) * sy).max(0.0) as usize;
        let y1 = ((img.point_height - run.y) * sy) as usize;
        let mut inked = 0u32;
        for py in y0..y1.min(img.height as usize) {
            for px in x0..x1.min(img.width as usize) {
                let o = (py * img.width as usize + px) * 4;
                let (r, g, b, a) =
                    (img.pixels[o], img.pixels[o + 1], img.pixels[o + 2], img.pixels[o + 3]);
                if a > 128 && (r as u32 + g as u32 + b as u32) < 600 {
                    inked += 1;
                }
            }
        }
        assert!(inked > 20, "embedded font must rasterize, got {inked} inked pixels in the run box");
    }

    #[test]
    fn refont_splits_tj_and_keeps_kerns() {
        if !refont_available("Ω") {
            return;
        }
        let content: &[u8] = b"BT /F1 12 Tf 50 700 Td [(AB) -100 (CD) -50 (EF)] TJ ET";
        let mut doc = one_page_doc(helv(), content);
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs[1].text, "CD");

        let outcome = edit_run(&mut doc, 1, &runs[1], "Ω").unwrap();
        assert!(outcome.refont.is_some());

        let after = text_runs(&doc, 1).unwrap();
        let texts: Vec<&str> = after.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["AB", "Ω", "EF"], "TJ must split around the refonted item");

        // Kern numbers keep their place: -100 before Ω, -50 before EF.
        let (ab, om, ef) = (&after[0], &after[1], &after[2]);
        let kern = |n: f32| -n / 1000.0 * 12.0;
        assert!(
            (om.x - (ab.x + ab.width.unwrap() + kern(-100.0))).abs() < 0.05,
            "kern before the edit lost: om.x={}, expected {}",
            om.x,
            ab.x + ab.width.unwrap() + kern(-100.0)
        );
        assert!(
            (ef.x - (om.x + om.width.unwrap() + kern(-50.0))).abs() < 0.05,
            "kern after the edit lost: ef.x={}",
            ef.x
        );
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
        // 'A' no longer exists in this font's encoding — the system-font
        // fallback must carry it (or refuse cleanly on machines without one).
        match edit_run(&mut doc, 1, &after[0], "A") {
            Ok(outcome) => {
                assert!(outcome.refont.is_some(), "an out-of-encoding edit must report its embed");
                assert!(text_runs(&doc, 1).unwrap().iter().any(|r| r.text == "A"));
            }
            Err(_) => eprintln!("no system font available — refusal accepted"),
        }
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

    /// Three near-full lines of Helvetica 12 prose — a recognizable paragraph.
    const PARA: &[u8] = b"BT /F1 12 Tf 50 700 Td (The quick brown fox jumps over) Tj \
                          0 -14 Td (the lazy dog while the cat naps) Tj \
                          0 -14 Td (quietly indoors.) Tj ET";

    #[test]
    fn rewrap_cascades_overflow_through_paragraph() {
        let mut doc = one_page_doc(helv(), PARA);
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs.len(), 3);
        let w_max = runs.iter().map(|r| r.width.unwrap()).fold(0.0f32, f32::max);

        edit_run(&mut doc, 1, &runs[0], "The extraordinarily quick brown fox jumps over").unwrap();

        let after = text_runs(&doc, 1).unwrap();
        assert_eq!(after.len(), 3, "line count must not change");
        for (a, b) in runs.iter().zip(after.iter()) {
            assert!((a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01, "positions must not move");
        }
        for r in &after[..2] {
            assert!(
                r.width.unwrap() <= w_max + 0.3,
                "wrapped line overflows: {:?} ({} > {w_max})",
                r.text,
                r.width.unwrap()
            );
        }
        let joined = after.iter().map(|r| r.text.clone()).collect::<Vec<_>>().join(" ");
        assert_eq!(
            joined,
            "The extraordinarily quick brown fox jumps over the lazy dog while the cat naps quietly indoors."
        );
        assert_ne!(after[0].text, "The extraordinarily quick brown fox jumps over", "overflow must cascade");
    }

    #[test]
    fn rewrap_pulls_words_up_on_shrink() {
        let mut doc = one_page_doc(helv(), PARA);
        let runs = text_runs(&doc, 1).unwrap();
        edit_run(&mut doc, 1, &runs[0], "Tip.").unwrap();

        let after = text_runs(&doc, 1).unwrap();
        assert!(after[0].text.split_whitespace().count() > 1, "shrunk line must pull words up: {:?}", after[0].text);
        let joined = after
            .iter()
            .map(|r| r.text.clone())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, "Tip. the lazy dog while the cat naps quietly indoors.");
    }

    #[test]
    fn rewrap_refuses_ragged_line_lists() {
        // Address-like block: same font/leading/margin but ragged widths —
        // redistributing words here would corrupt meaning.
        let content: &[u8] = b"BT /F1 12 Tf 50 700 Td (John Smith) Tj \
                               0 -14 Td (12 Foo Street Apt 4B Springfield) Tj \
                               0 -14 Td (USA) Tj ET";
        let mut doc = one_page_doc(helv(), content);
        let runs = text_runs(&doc, 1).unwrap();
        edit_run(&mut doc, 1, &runs[0], "Johnathan Smithers").unwrap();

        let after = text_runs(&doc, 1).unwrap();
        assert_eq!(after[0].text, "Johnathan Smithers");
        assert_eq!(after[1].text, "12 Foo Street Apt 4B Springfield", "line lists must stay line lists");
        assert_eq!(after[2].text, "USA");
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

    #[test]
    fn tounicode_bfrange_array_form_parses() {
        let cmap: &[u8] = b"/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
            /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
            /CMapName /Adobe-Identity-UCS def\n\
            1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
            1 beginbfrange\n<41> <43> [<0058> <0059> <005A>]\n<61> <62> <0061>\nendbfrange\n\
            1 beginbfchar\n<44> <0044>\n<45> /Euro\nendbfchar\n\
            endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend";
        let codec = tounicode_codec(cmap).expect("cmap parses");
        assert_eq!(codec.code_len, 1);
        assert_eq!(codec.decode(b"ABCDab"), "XYZDab", "array-form bfrange must map each code to its own entry");
        assert!(codec.invertible);
        assert_eq!(codec.encode("XYZ"), Some(b"ABC".to_vec()));

        // Through a font on a page, with the CMap as the font's ToUnicode.
        use lopdf::{dictionary, Stream};
        let font = dictionary! {
            "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "Custom",
            "ToUnicode" => Object::Stream(Stream::new(dictionary! {}, cmap.to_vec())),
        };
        let doc = one_page_doc(font, b"BT /F1 12 Tf 50 700 Td (ABC) Tj ET");
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs[0].text, "XYZ");
    }

    #[test]
    fn two_byte_bfrange_array_form_keeps_code_width() {
        let cmap: &[u8] = b"1 begincodespacerange <0000> <FFFF> endcodespacerange\n\
            1 beginbfrange <0010> <0011> [<0041> <0042>] endbfrange";
        let codec = tounicode_codec(cmap).unwrap();
        assert_eq!(codec.code_len, 2);
        assert_eq!(codec.decode(&[0x00, 0x10, 0x00, 0x11]), "AB");
    }

    #[test]
    fn runs_honor_cm_and_scaled_tm() {
        let content: &[u8] = b"q 1 0 0 1 100 50 cm BT /F1 1 Tf 12 0 0 12 20 30 Tm (AB) Tj ET Q \
                               BT /F1 12 Tf 50 700 Td (CD) Tj ET";
        let doc = one_page_doc(helv(), content);
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs.len(), 2);
        // Tm scale 12 × Tf 1 at (20,30), shifted by the cm (100,50).
        assert!((runs[0].x - 120.0).abs() < 0.01 && (runs[0].y - 80.0).abs() < 0.01, "got ({},{})", runs[0].x, runs[0].y);
        assert!((runs[0].display_size - 12.0).abs() < 0.01, "got {}", runs[0].display_size);
        assert!((runs[0].font_size - 1.0).abs() < 0.01, "nominal Tf size must stay");
        assert!((runs[0].width.unwrap() - helv_w("AB", 12.0)).abs() < 0.01, "user-space width includes the Tm scale");
        // After Q the cm is gone.
        assert!((runs[1].x - 50.0).abs() < 0.01 && (runs[1].y - 700.0).abs() < 0.01);
        assert!((runs[1].display_size - 12.0).abs() < 0.01);
    }

    #[test]
    fn text_inside_form_xobjects_is_listed_read_only() {
        use lopdf::{dictionary, Stream};
        let mut doc = one_page_doc(helv(), b"BT /F1 12 Tf 50 700 Td (Page) Tj ET q 1 0 0 1 200 100 cm /Fx Do Q");
        let page_id = doc.get_pages()[&1];
        let font_ref = {
            let page = doc.get_dictionary(page_id).unwrap();
            let res = page.get(b"Resources").unwrap().as_dict().unwrap();
            res.get(b"Font").unwrap().as_dict().unwrap().get(b"F1").unwrap().clone()
        };
        let form = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
                "Matrix" => vec![1.into(), 0.into(), 0.into(), 1.into(), 10.into(), 20.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_ref } },
            },
            b"BT /F1 10 Tf 5 5 Td (Inside) Tj ET".to_vec(),
        ));
        {
            let page = doc.get_dictionary_mut(page_id).unwrap();
            let mut res = page.get(b"Resources").unwrap().as_dict().unwrap().clone();
            res.set("XObject", dictionary! { "Fx" => Object::Reference(form) });
            page.set("Resources", Object::Dictionary(res));
        }
        let runs = text_runs(&doc, 1).unwrap();
        let texts: Vec<&str> = runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["Page", "Inside"]);
        let inside = runs[1].clone();
        assert!(inside.in_form && !inside.editable);
        assert!(!runs[0].in_form && runs[0].editable);
        // (5,5) in form space → +Matrix (10,20) → +cm (200,100).
        assert!((inside.x - 215.0).abs() < 0.01 && (inside.y - 125.0).abs() < 0.01, "got ({},{})", inside.x, inside.y);
        assert!(edit_run(&mut doc, 1, &inside, "Nope").is_err(), "form text must refuse in-place edits");
        // find_replace skips it without error and still edits page-level text.
        assert_eq!(find_replace(&mut doc, 1, "Inside", "Out").unwrap(), 0);
        assert_eq!(find_replace(&mut doc, 1, "Page", "Sheet").unwrap(), 1);
        assert!(text_runs(&doc, 1).unwrap().iter().any(|r| r.text == "Sheet"));
    }

    #[test]
    fn find_replace_counts_every_occurrence() {
        let mut doc = one_page_doc(helv(), b"BT /F1 12 Tf 50 700 Td (ab ab ab) Tj ET");
        assert_eq!(find_replace(&mut doc, 1, "ab", "x").unwrap(), 3);
        assert_eq!(text_runs(&doc, 1).unwrap()[0].text, "x x x");
        assert!(find_replace(&mut doc, 1, "", "x").is_err());
    }

    #[test]
    fn type3_fonts_measure_through_their_font_matrix() {
        use lopdf::dictionary;
        // Glyph space is 1/100 text space; 'a' is 50 glyph units wide → 0.5 em.
        let font = dictionary! {
            "Type" => "Font", "Subtype" => "Type3",
            "FontMatrix" => vec![Object::Real(0.01), 0.into(), 0.into(), Object::Real(0.01), 0.into(), 0.into()],
            "FontBBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "CharProcs" => dictionary! {},
            "Encoding" => dictionary! { "Type" => "Encoding", "Differences" => vec![97.into(), Object::Name(b"a".to_vec())] },
            "FirstChar" => 97, "LastChar" => 97, "Widths" => vec![50.into()],
        };
        let doc = one_page_doc(font, b"BT /F1 20 Tf 50 700 Td (aa) Tj ET");
        let runs = text_runs(&doc, 1).unwrap();
        assert_eq!(runs[0].text, "aa");
        assert!((runs[0].width.unwrap() - 20.0).abs() < 0.01, "2 × 0.5em × 20pt = 20, got {:?}", runs[0].width);
        // Codes outside /Differences decode as unknown rather than as WinAnsi.
        let doc2 = one_page_doc(
            dictionary! {
                "Type" => "Font", "Subtype" => "Type3",
                "FontMatrix" => vec![Object::Real(0.01), 0.into(), 0.into(), Object::Real(0.01), 0.into(), 0.into()],
                "Encoding" => dictionary! { "Differences" => vec![97.into(), Object::Name(b"a".to_vec())] },
            },
            b"BT /F1 20 Tf 50 700 Td (ab) Tj ET",
        );
        assert_eq!(text_runs(&doc2, 1).unwrap()[0].text, "a\u{FFFD}");
    }
}
