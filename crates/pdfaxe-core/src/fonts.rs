//! System font discovery, matching and re-embedding — the fallback that makes
//! text edits work when the document's own font can't encode the new text.
//!
//! Discovery reads only each font file's table directory and `name` table (a
//! few KB per file), so indexing the Windows Fonts folder stays cheap; the
//! full file is read once, at embed time, for the single face that matched.
//! Embedding subsets the face down to the used glyphs and writes a
//! Type0/Identity-H font: the subset glyph ids double as CIDs, widths come
//! from the real font's metrics, and a generated ToUnicode CMap keeps text
//! extraction — and future edits of the run — working.
//!
//! v1 scope: TrueType (glyf) faces only, no shaping — one glyph per char via
//! the font's cmap. CFF-flavored faces are indexed but never chosen.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read as _, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use skrifa::instance::{LocationRef, Size};
use skrifa::{FontRef, GlyphId, MetadataProvider};
use subsetter::GlyphRemapper;

/// One physical face inside a font file, as discovered by the scanner.
#[derive(Debug, Clone)]
pub(crate) struct FaceInfo {
    pub path: PathBuf,
    /// Face index inside a .ttc collection (0 for plain files).
    pub index: u32,
    /// Typographic family (name id 16, falling back to 1) — "Arial".
    pub family: String,
    /// Typographic subfamily (17 falling back to 2) — "Bold Italic".
    pub subfamily: String,
    /// PostScript name (6) — "Arial-BoldMT". May be empty.
    pub ps_name: String,
    /// TrueType outlines present — what the embedder can handle.
    pub has_glyf: bool,
}

impl FaceInfo {
    /// Human-facing label: "Arial Bold", or just "Arial" for regular faces.
    pub fn label(&self) -> String {
        let sub = self.subfamily.trim();
        if sub.is_empty() || sub.eq_ignore_ascii_case("regular") {
            self.family.clone()
        } else {
            format!("{} {sub}", self.family)
        }
    }
}

// ---------------------------------------------------------------------------
// sfnt name-table scanning (header + one table per file, seek-based)
// ---------------------------------------------------------------------------

fn u16be(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]))
}

fn u32be(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?]))
}

fn read_at(f: &mut File, off: u64, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    f.seek(SeekFrom::Start(off)).ok()?;
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn utf16be(bytes: &[u8]) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16(&units).ok()
}

fn scan_file(path: &Path) -> Vec<FaceInfo> {
    let Ok(mut f) = File::open(path) else { return Vec::new() };
    let Some(head) = read_at(&mut f, 0, 12) else { return Vec::new() };
    let mut faces = Vec::new();
    if &head[..4] == b"ttcf" {
        let n = u32be(&head, 8).unwrap_or(0).min(64);
        if let Some(offs) = read_at(&mut f, 12, n as usize * 4) {
            for i in 0..n {
                let off = u32be(&offs, i as usize * 4).unwrap_or(0);
                faces.extend(scan_face(&mut f, path, i, off as u64));
            }
        }
    } else if head[..4] == [0, 1, 0, 0] || &head[..4] == b"true" || &head[..4] == b"OTTO" {
        faces.extend(scan_face(&mut f, path, 0, 0));
    }
    faces
}

fn scan_face(f: &mut File, path: &Path, index: u32, off: u64) -> Option<FaceInfo> {
    let head = read_at(f, off, 12)?;
    let num_tables = u16be(&head, 4)? as usize;
    if num_tables == 0 || num_tables > 512 {
        return None;
    }
    let dir = read_at(f, off + 12, num_tables * 16)?;
    let (mut name_off, mut name_len, mut has_glyf) = (None, 0usize, false);
    for i in 0..num_tables {
        let e = &dir[i * 16..i * 16 + 16];
        match &e[..4] {
            b"name" => {
                name_off = Some(u32be(e, 8)? as u64);
                name_len = u32be(e, 12)? as usize;
            }
            b"glyf" => has_glyf = true,
            _ => {}
        }
    }
    // Table offsets are absolute in both plain files and collections.
    let name = read_at(f, name_off?, name_len.min(131_072))?;
    let count = u16be(&name, 2)? as usize;
    let str_off = u16be(&name, 4)? as usize;

    // name id -> (rank, value); Windows en-US beats other Unicode beats Mac.
    let mut best: HashMap<u16, (u8, String)> = HashMap::new();
    for i in 0..count {
        let r = 6 + i * 12;
        if r + 12 > name.len() {
            break;
        }
        let (plat, enc, lang) = (u16be(&name, r)?, u16be(&name, r + 2)?, u16be(&name, r + 4)?);
        let id = u16be(&name, r + 6)?;
        if !matches!(id, 1 | 2 | 6 | 16 | 17) {
            continue;
        }
        let len = u16be(&name, r + 8)? as usize;
        let so = str_off + u16be(&name, r + 10)? as usize;
        let Some(raw) = name.get(so..so + len) else { continue };
        let (rank, value) = match (plat, enc) {
            (3, 1) | (3, 10) | (0, _) => {
                let Some(s) = utf16be(raw) else { continue };
                (if plat == 3 && lang == 0x409 { 3u8 } else { 2 }, s)
            }
            (1, 0) => (1, raw.iter().map(|&b| b as char).collect()),
            _ => continue,
        };
        match best.get(&id) {
            Some((r0, _)) if *r0 >= rank => {}
            _ => {
                best.insert(id, (rank, value));
            }
        }
    }

    let get = |id: u16| best.get(&id).map(|(_, s)| s.trim().to_string()).unwrap_or_default();
    let family = { let f16 = get(16); if f16.is_empty() { get(1) } else { f16 } };
    if family.is_empty() {
        return None;
    }
    let subfamily = { let s17 = get(17); if s17.is_empty() { get(2) } else { s17 } };
    Some(FaceInfo { path: path.to_path_buf(), index, family, subfamily, ps_name: get(6), has_glyf })
}

// ---------------------------------------------------------------------------
// System index + matching
// ---------------------------------------------------------------------------

/// Style/shape hints scraped from the PDF font dictionary, guiding the match.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FontHints {
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub fixed: bool,
}

/// Comparison key: lowercase alphanumerics only ("Times New Roman" ==
/// "TimesNewRoman").
fn key(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// (bold-ish, italic-ish) judged from a style string.
fn style_of(s: &str) -> (bool, bool) {
    let k = s.to_ascii_lowercase();
    (
        k.contains("bold") || k.contains("black") || k.contains("heavy"),
        k.contains("italic") || k.contains("oblique"),
    )
}

/// Family key + style parsed from a PDF BaseFont name like
/// "ABCDEF+TimesNewRomanPS-BoldItalicMT".
fn parse_base(base: &str) -> (String, bool, bool) {
    let base = match base.split_once('+') {
        Some((p, rest)) if p.len() == 6 => rest,
        _ => base,
    };
    let (fam, style) = match base.split_once(['-', ',']) {
        Some((f, s)) => (f, s),
        None => (base, ""),
    };
    let (bold, italic) = style_of(style);
    let mut k = key(fam);
    for suffix in ["psmt", "ps", "mt"] {
        if let Some(stripped) = k.strip_suffix(suffix) {
            if !stripped.is_empty() {
                k = stripped.to_string();
            }
        }
    }
    (k, bold, italic)
}

/// Standard-14 (and friends) to fonts that actually exist on disk.
fn alias(k: &str) -> &str {
    match k {
        "helvetica" | "helveticaneue" | "arial" => "arial",
        "times" | "timesroman" | "timesnewroman" => "timesnewroman",
        "courier" | "couriernew" => "couriernew",
        other => other,
    }
}

pub(crate) struct SystemFonts {
    faces: Vec<FaceInfo>,
}

fn walk(dir: &Path, depth: u8, out: &mut Vec<FaceInfo>) {
    if depth > 3 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, depth + 1, out);
            continue;
        }
        match p.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("ttf" | "otf" | "ttc" | "otc") => out.extend(scan_file(&p)),
            _ => {}
        }
    }
}

fn default_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        dirs.push(root.join("Fonts"));
        if let Some(lad) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(lad).join(r"Microsoft\Windows\Fonts"));
        }
    } else {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(&home).join(".local/share/fonts"));
            dirs.push(PathBuf::from(&home).join(".fonts"));
        }
    }
    dirs
}

impl SystemFonts {
    pub(crate) fn scan(dirs: &[PathBuf]) -> Self {
        let mut faces = Vec::new();
        for d in dirs {
            walk(d, 0, &mut faces);
        }
        SystemFonts { faces }
    }

    /// The machine's font index, scanned once per process.
    pub(crate) fn system() -> &'static SystemFonts {
        static SYS: OnceLock<SystemFonts> = OnceLock::new();
        SYS.get_or_init(|| SystemFonts::scan(&default_dirs()))
    }

    fn find_ps(&self, ps_key: &str) -> Option<&FaceInfo> {
        self.faces
            .iter()
            .find(|f| f.has_glyf && !f.ps_name.is_empty() && key(&f.ps_name) == ps_key)
    }

    fn find_family(&self, fam_key: &str, bold: bool, italic: bool) -> Option<&FaceInfo> {
        let mut best: Option<(&FaceInfo, u8)> = None;
        for face in &self.faces {
            if !face.has_glyf || key(&face.family) != fam_key {
                continue;
            }
            let (fb, fi) = style_of(&face.subfamily);
            let score = if (fb, fi) == (bold, italic) {
                3
            } else if !fb && !fi {
                2 // regular stands in for a missing style
            } else {
                1
            };
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((face, score));
            }
        }
        best.map(|(f, _)| f)
    }

    /// Replacement candidates for a PDF base-font name, best first: exact
    /// PostScript name, family+style (through the standard-14 aliases), a
    /// shape-matched generic, then broad-coverage fallbacks so symbols and
    /// CJK still find a glyph somewhere on the machine.
    pub(crate) fn candidates(&self, base_font: &str, hints: FontHints) -> Vec<&FaceInfo> {
        let (fam, name_bold, name_italic) = parse_base(base_font);
        let bold = hints.bold || name_bold;
        let italic = hints.italic || name_italic;

        let mut out: Vec<&FaceInfo> = Vec::new();
        let base_key = {
            let stripped = match base_font.split_once('+') {
                Some((p, rest)) if p.len() == 6 => rest,
                _ => base_font,
            };
            key(stripped)
        };
        out.extend(self.find_ps(&base_key));
        out.extend(self.find_family(alias(&fam), bold, italic));

        let generics: &[&str] = if hints.fixed {
            &["couriernew", "consolas"]
        } else if hints.serif {
            &["timesnewroman", "cambria", "georgia"]
        } else {
            &["arial", "segoeui", "calibri"]
        };
        for g in generics {
            out.extend(self.find_family(g, bold, italic));
        }
        // Coverage extenders, regular style: Latin/Greek/Cyrillic, symbols,
        // then the big CJK families.
        for g in ["segoeui", "arial", "timesnewroman", "segoeuisymbol", "microsoftyahei",
                  "yugothicui", "msgothic", "malgungothic"]
        {
            out.extend(self.find_family(g, false, false));
        }

        let mut seen: BTreeSet<(PathBuf, u32)> = BTreeSet::new();
        out.retain(|f| seen.insert((f.path.clone(), f.index)));
        out
    }
}

// ---------------------------------------------------------------------------
// Embedding
// ---------------------------------------------------------------------------

/// A font subset embedded into the document, plus how to write text with it.
pub(crate) struct Embedded {
    /// Object id of the Type0 font dictionary.
    pub font_id: ObjectId,
    /// char -> 2-byte code (the subset glyph id, used as CID).
    pub encode: HashMap<char, u16>,
    /// Human-facing name of what got embedded ("Arial Bold").
    pub label: String,
}

/// Find a system font standing in for `base_font` that covers `chars`, and
/// embed a subset of it. Candidates that lack a glyph are skipped; the
/// document is only touched on success.
pub(crate) fn embed_replacement(
    doc: &mut Document,
    base_font: &str,
    hints: FontHints,
    chars: &BTreeSet<char>,
) -> Result<Embedded> {
    let sys = SystemFonts::system();
    let cands = sys.candidates(base_font, hints);
    if cands.is_empty() {
        return Err(anyhow!("no usable system fonts found (looked for a stand-in for {base_font})"));
    }
    let mut last_err = None;
    for face in cands {
        match embed_subset(doc, face, hints, chars) {
            Ok(e) => return Ok(e),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no candidate font covered the text")))
}

/// Subset `face` down to `chars` and embed it as a Type0/Identity-H font.
/// All fallible work happens before any object is added, so a failed
/// candidate leaves the document untouched.
pub(crate) fn embed_subset(
    doc: &mut Document,
    face: &FaceInfo,
    hints: FontHints,
    chars: &BTreeSet<char>,
) -> Result<Embedded> {
    let data = std::fs::read(&face.path)
        .map_err(|e| anyhow!("couldn't read {}: {e}", face.path.display()))?;
    let font = FontRef::from_index(&data, face.index)
        .map_err(|e| anyhow!("couldn't parse {}: {e}", face.path.display()))?;

    // Map every char; a missing glyph disqualifies this candidate.
    let charmap = font.charmap();
    let mut remapper = GlyphRemapper::new();
    let mut encode: HashMap<char, u16> = HashMap::new();
    let mut old_gid: HashMap<char, u16> = HashMap::new();
    for &ch in chars {
        let gid = charmap.map(ch).map(|g| g.to_u32()).unwrap_or(0);
        let gid = u16::try_from(gid).map_err(|_| anyhow!("glyph id out of range"))?;
        if gid == 0 {
            return Err(anyhow!("{} has no glyph for {ch:?}", face.label()));
        }
        encode.insert(ch, remapper.remap(gid));
        old_gid.insert(ch, gid);
    }

    let metrics = font.metrics(Size::unscaled(), LocationRef::default());
    let upem = metrics.units_per_em as f32;
    if upem <= 0.0 {
        return Err(anyhow!("{} has no units-per-em", face.label()));
    }
    let k = 1000.0 / upem;
    let gm = font.glyph_metrics(Size::unscaled(), LocationRef::default());
    // new gid -> width in 1/1000 text units
    let mut widths: HashMap<u16, f32> = HashMap::new();
    for (ch, &old) in &old_gid {
        let w = gm.advance_width(GlyphId::new(old as u32)).unwrap_or(upem / 2.0) * k;
        widths.insert(encode[ch], w);
    }

    let subset_bytes = subsetter::subset(&data, face.index, &remapper)
        .map_err(|e| anyhow!("couldn't subset {}: {e}", face.label()))?;

    // ---- From here on, nothing can fail: write the PDF objects. ----

    // Subset tag (deterministic from the glyph set) + PostScript-ish name.
    let mut h: u32 = 2_166_136_261;
    let mut sorted: Vec<u16> = old_gid.values().copied().collect();
    sorted.sort_unstable();
    for g in &sorted {
        for byte in g.to_be_bytes() {
            h = (h ^ byte as u32).wrapping_mul(16_777_619);
        }
    }
    let tag: String = (0..6).map(|i| char::from(b'A' + ((h >> (i * 4)) % 26) as u8)).collect();
    let fam_compact: String = face.family.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let fam_compact = if fam_compact.is_empty() { "Embedded".to_string() } else { fam_compact };
    let style_compact: String = face.subfamily.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let ps_like = if style_compact.is_empty() || style_compact.eq_ignore_ascii_case("regular") {
        fam_compact
    } else {
        format!("{fam_compact}-{style_compact}")
    };
    let base_name = format!("{tag}+{ps_like}");

    // /W array, consecutive codes grouped: c [w1 w2 ...].
    let mut codes: Vec<u16> = widths.keys().copied().collect();
    codes.sort_unstable();
    let mut w_arr: Vec<Object> = Vec::new();
    let mut i = 0usize;
    while i < codes.len() {
        let mut ws: Vec<Object> = vec![widths[&codes[i]].into()];
        let mut j = i + 1;
        while j < codes.len() && codes[j] == codes[j - 1] + 1 {
            ws.push(widths[&codes[j]].into());
            j += 1;
        }
        w_arr.push((codes[i] as i64).into());
        w_arr.push(Object::Array(ws));
        i = j;
    }

    // ToUnicode CMap so extraction and later edits keep working.
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let mut pairs: Vec<(u16, char)> = encode.iter().map(|(&c, &g)| (g, c)).collect();
    pairs.sort_unstable();
    for chunk in pairs.chunks(100) {
        cmap.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (g, c) in chunk {
            let mut buf = [0u16; 2];
            let dst: String = c.encode_utf16(&mut buf).iter().map(|u| format!("{u:04X}")).collect();
            cmap.push_str(&format!("<{g:04X}> <{dst}>\n"));
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");

    let raw_len = subset_bytes.len() as i64;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&subset_bytes).map_err(|e| anyhow!("couldn't compress font file: {e}"))?;
    let compressed = enc.finish().map_err(|e| anyhow!("couldn't compress font file: {e}"))?;
    let file_id = doc.add_object(Stream::new(
        dictionary! { "Length1" => raw_len, "Filter" => "FlateDecode" },
        compressed,
    ));

    let (face_bold, face_italic) = style_of(&face.subfamily);
    let flags: i64 = 32
        | if hints.fixed { 1 } else { 0 }
        | if hints.serif { 2 } else { 0 }
        | if face_italic { 64 } else { 0 };
    let bbox = metrics
        .bounds
        .map(|b| [b.x_min * k, b.y_min * k, b.x_max * k, b.y_max * k])
        .unwrap_or([-1000.0, -300.0, 2000.0, 1000.0]);
    let ascent = metrics.ascent.abs() * k;
    let descent = -(metrics.descent * k).abs();
    let cap_height = metrics.cap_height.map(|c| c * k).unwrap_or(ascent * 0.9);

    let desc_id = doc.add_object(dictionary! {
        "Type" => "FontDescriptor",
        "FontName" => Object::Name(base_name.clone().into_bytes()),
        "Flags" => flags,
        "FontBBox" => vec![bbox[0].into(), bbox[1].into(), bbox[2].into(), bbox[3].into()],
        "ItalicAngle" => if face_italic { -12 } else { 0 },
        "Ascent" => ascent,
        "Descent" => descent,
        "CapHeight" => cap_height,
        "StemV" => if face_bold { 120 } else { 80 },
        "FontFile2" => Object::Reference(file_id),
    });
    let cid_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "CIDFontType2",
        "BaseFont" => Object::Name(base_name.clone().into_bytes()),
        "CIDSystemInfo" => dictionary! {
            "Registry" => Object::string_literal("Adobe"),
            "Ordering" => Object::string_literal("Identity"),
            "Supplement" => 0,
        },
        "FontDescriptor" => Object::Reference(desc_id),
        "DW" => 500,
        "W" => Object::Array(w_arr),
        "CIDToGIDMap" => "Identity",
    });
    let tu_id = doc.add_object(Stream::new(dictionary! {}, cmap.into_bytes()));
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type0",
        "BaseFont" => Object::Name(base_name.into_bytes()),
        "Encoding" => "Identity-H",
        "DescendantFonts" => vec![Object::Reference(cid_id)],
        "ToUnicode" => Object::Reference(tu_id),
    });

    Ok(Embedded { font_id, encode, label: face.label() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_names_parse() {
        assert_eq!(parse_base("ABCDEF+TimesNewRomanPS-BoldItalicMT"),
                   ("timesnewroman".to_string(), true, true));
        assert_eq!(parse_base("Helvetica-Oblique"), ("helvetica".to_string(), false, true));
        assert_eq!(parse_base("ArialMT"), ("arial".to_string(), false, false));
        assert_eq!(parse_base("Courier"), ("courier".to_string(), false, false));
        assert_eq!(alias("helvetica"), "arial");
        assert_eq!(alias("timesroman"), "timesnewroman");
    }

    /// Skip helper: system-font tests only run where fonts actually exist.
    fn sys() -> Option<&'static SystemFonts> {
        let s = SystemFonts::system();
        if s.faces.is_empty() {
            eprintln!("skipping: no system fonts found");
            return None;
        }
        Some(s)
    }

    #[test]
    fn scans_and_matches_core_faces() {
        let Some(s) = sys() else { return };
        let c = s.candidates("Helvetica-Bold", FontHints::default());
        assert!(!c.is_empty(), "expected at least a generic candidate");
        if cfg!(windows) {
            let first = c[0];
            assert_eq!(key(&first.family), "arial", "Helvetica must map to Arial, got {first:?}");
            let (b, _) = style_of(&first.subfamily);
            assert!(b, "bold style must be honored, got {first:?}");
        }
    }

    #[test]
    fn embeds_a_subset_with_tounicode() {
        let Some(s) = sys() else { return };
        let chars: BTreeSet<char> = "Hello Ω".chars().collect();
        let cands = s.candidates("Helvetica", FontHints::default());
        let mut doc = Document::with_version("1.7");
        let mut embedded = None;
        for f in cands {
            if let Ok(e) = embed_subset(&mut doc, f, FontHints::default(), &chars) {
                embedded = Some(e);
                break;
            }
        }
        let Some(e) = embedded else {
            eprintln!("skipping: no candidate covered the test chars");
            return;
        };
        assert_eq!(e.encode.len(), chars.len());
        assert!(e.encode.values().all(|&g| g != 0), "no char may map to .notdef");
        // The Type0 dict must resolve and carry a ToUnicode stream.
        let font = doc.get_object(e.font_id).unwrap().as_dict().unwrap();
        assert_eq!(font.get(b"Subtype").unwrap().as_name().unwrap(), b"Type0");
        let tu = font.get(b"ToUnicode").unwrap().as_reference().unwrap();
        let Object::Stream(stream) = doc.get_object(tu).unwrap() else { panic!("ToUnicode missing") };
        let text = String::from_utf8_lossy(&stream.content).into_owned();
        assert!(text.contains("beginbfchar"));
    }
}
