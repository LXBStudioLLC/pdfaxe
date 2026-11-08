use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::carry::{self, SourceMeta};

/// One selected source page inside an [`assemble`] job.
#[derive(Debug, Clone, Copy)]
pub struct PageSel {
    /// 1-based page number in the source document.
    pub page: u32,
    /// Extra clockwise rotation to apply, in degrees (multiples of 90).
    pub extra_rotation: i64,
}

impl PageSel {
    pub fn plain(page: u32) -> Self {
        Self { page, extra_rotation: 0 }
    }
}

pub fn load(path: impl AsRef<Path>) -> Result<Document> {
    let path = path.as_ref();
    Document::load(path).with_context(|| format!("couldn't read {} — is it a valid PDF?", path.display()))
}

/// Serialize `doc` and write it to `path` atomically: the bytes land in a
/// sibling temp file first and are renamed over the destination only once
/// they are complete, so a failed save can never leave a half-written PDF
/// where the user's file used to be.
pub fn save(doc: &mut Document, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let mut buf = Vec::new();
    doc.save_to(&mut buf)
        .with_context(|| format!("couldn't serialize the document for {}", path.display()))?;
    write_atomic(path, &buf)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };
    let stem = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "out.pdf".into());
    let tmp = dir.join(format!(".{stem}.pdfaxe-{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("couldn't write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!(e)).with_context(|| format!("couldn't write {}", path.display()));
    }
    Ok(())
}

#[derive(Debug)]
pub struct DocInfo {
    pub pages: u32,
    pub version: String,
    pub encrypted: bool,
}

pub fn info(doc: &Document) -> DocInfo {
    DocInfo {
        pages: doc.get_pages().len() as u32,
        version: doc.version.clone(),
        encrypted: doc.trailer.get(b"Encrypt").is_ok(),
    }
}

pub fn extract_text(doc: &Document, page: u32) -> Result<String> {
    doc.extract_text(&[page]).map_err(|e| anyhow!("text extraction failed: {e}"))
}

fn num(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r as f32),
        _ => None,
    }
}

/// A (possibly inherited) rectangle attribute of a page, normalized so that
/// x0 <= x1 and y0 <= y1.
fn page_box(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<(f32, f32, f32, f32)> {
    let nums: Vec<f32> = resolve_inherited(doc, page_id, key)
        .and_then(|o| match o {
            Object::Array(a) => Some(a),
            Object::Reference(id) => match doc.get_object(id) {
                Ok(Object::Array(a)) => Some(a.clone()),
                _ => None,
            },
            _ => None,
        })
        .map(|a| a.iter().filter_map(|o| num(match o {
            Object::Reference(id) => doc.get_object(*id).unwrap_or(o),
            other => other,
        })).collect())
        .unwrap_or_default();
    if nums.len() != 4 {
        return None;
    }
    let (x0, x1) = (nums[0].min(nums[2]), nums[0].max(nums[2]));
    let (y0, y1) = (nums[1].min(nums[3]), nums[1].max(nums[3]));
    Some((x0, y0, x1, y1))
}

/// MediaBox of a page as (x0, y0, x1, y1); the US-Letter default when absent.
pub fn page_media_box(doc: &Document, page_id: ObjectId) -> (f32, f32, f32, f32) {
    page_box(doc, page_id, b"MediaBox").unwrap_or((0.0, 0.0, 612.0, 792.0))
}

/// The box viewers actually show: CropBox ∩ MediaBox, falling back to the
/// MediaBox when the CropBox is absent or the intersection is degenerate.
/// This is what the renderer rasterizes and what stamps/redactions must use
/// as "the page".
pub fn page_visible_box(doc: &Document, page_id: ObjectId) -> (f32, f32, f32, f32) {
    let media = page_media_box(doc, page_id);
    let Some(crop) = page_box(doc, page_id, b"CropBox") else { return media };
    let x0 = media.0.max(crop.0);
    let y0 = media.1.max(crop.1);
    let x1 = media.2.min(crop.2);
    let y1 = media.3.min(crop.3);
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        media
    } else {
        (x0, y0, x1, y1)
    }
}

/// The page's /Rotate, normalized to 0 / 90 / 180 / 270.
pub fn page_rotation(doc: &Document, page_id: ObjectId) -> i64 {
    let raw = resolve_inherited(doc, page_id, b"Rotate")
        .and_then(|o| match o {
            Object::Integer(i) => Some(i),
            Object::Real(r) => Some(r.round() as i64),
            Object::Reference(r) => doc.get_object(r).ok().and_then(|x| x.as_i64().ok()),
            _ => None,
        })
        .unwrap_or(0);
    // Round to the nearest quarter turn: non-multiples of 90 are invalid and
    // viewers treat them as 0.
    let q = ((raw as f64 / 90.0).round() as i64).rem_euclid(4);
    q * 90
}

/// How a page appears on screen: its visible box in PDF user space and its
/// rotation. Converts between *user space* (what content streams and text
/// runs use) and *display space* — points measured from the bottom-left of
/// the page as the viewer shows it, rotation applied — which is what the
/// renderer's pixels, the GUI's mouse and redaction boxes all use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageView {
    /// Visible box (CropBox ∩ MediaBox) in user space.
    pub x0: f32,
    pub y0: f32,
    pub w: f32,
    pub h: f32,
    /// Clockwise display rotation: 0, 90, 180 or 270.
    pub rotate: i64,
}

impl PageView {
    /// Width and height of the page as displayed, in points.
    pub fn display_size(&self) -> (f32, f32) {
        if self.rotate == 90 || self.rotate == 270 {
            (self.h, self.w)
        } else {
            (self.w, self.h)
        }
    }

    /// User-space point → display-space point.
    pub fn to_display(&self, ux: f32, uy: f32) -> (f32, f32) {
        let (lx, ly) = (ux - self.x0, uy - self.y0);
        match self.rotate {
            90 => (ly, self.w - lx),
            180 => (self.w - lx, self.h - ly),
            270 => (self.h - ly, lx),
            _ => (lx, ly),
        }
    }

    /// Display-space point → user-space point.
    pub fn from_display(&self, dx: f32, dy: f32) -> (f32, f32) {
        let (lx, ly) = match self.rotate {
            90 => (self.w - dy, dx),
            180 => (self.w - dx, self.h - dy),
            270 => (dy, self.h - dx),
            _ => (dx, dy),
        };
        (lx + self.x0, ly + self.y0)
    }

    /// Axis-aligned display box (x, y, w, h) of a user-space rectangle.
    pub fn rect_to_display(&self, x: f32, y: f32, w: f32, h: f32) -> (f32, f32, f32, f32) {
        let pts = [
            self.to_display(x, y),
            self.to_display(x + w, y),
            self.to_display(x, y + h),
            self.to_display(x + w, y + h),
        ];
        let min_x = pts.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
        let max_x = pts.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max);
        let min_y = pts.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
        let max_y = pts.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max);
        (min_x, min_y, max_x - min_x, max_y - min_y)
    }
}

/// The display geometry of a page given its object id.
pub fn page_view_for(doc: &Document, page_id: ObjectId) -> PageView {
    let (x0, y0, x1, y1) = page_visible_box(doc, page_id);
    PageView {
        x0,
        y0,
        w: (x1 - x0).max(1.0),
        h: (y1 - y0).max(1.0),
        rotate: page_rotation(doc, page_id),
    }
}

/// The display geometry of one page (1-based).
pub fn page_view(doc: &Document, page_no: u32) -> Result<PageView> {
    let pages = doc.get_pages();
    let &id = pages.get(&page_no).ok_or_else(|| anyhow!("page {page_no} out of range"))?;
    Ok(page_view_for(doc, id))
}

/// Display geometry of every page, in order.
pub fn page_views(doc: &Document) -> Vec<PageView> {
    doc.get_pages().values().map(|&id| page_view_for(doc, id)).collect()
}

/// Display size (width, height) in points of every page in order, honoring
/// the CropBox and the page's /Rotate — cheap layout data, no rendering
/// involved, and the same dimensions the renderer will produce.
pub fn page_sizes(doc: &Document) -> Vec<(f32, f32)> {
    page_views(doc).into_iter().map(|v| v.display_size()).collect()
}

/// Look up a possibly-inherited page attribute by walking the Parent chain.
fn inherited(objects: &BTreeMap<ObjectId, Object>, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    let mut current = Some(page_id);
    let mut hops = 0;
    while let Some(id) = current {
        if hops > 64 {
            return None; // cycle guard
        }
        hops += 1;
        let dict = objects.get(&id)?.as_dict().ok()?;
        if let Ok(v) = dict.get(key) {
            return Some(v.clone());
        }
        current = dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok());
    }
    None
}

/// Public variant of the inherited-attribute walk, for other modules.
pub fn resolve_inherited(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    inherited(&doc.objects, page_id, key)
}

fn dict_type(obj: &Object) -> Option<Vec<u8>> {
    obj.as_dict()
        .ok()
        .and_then(|d| d.get(b"Type").ok())
        .and_then(|t| t.as_name().ok())
        .map(|n| n.to_vec())
}

/// Rebuild a document from an ordered list of source pages — the single
/// primitive behind merge (many docs, all pages), extract (one doc, subset)
/// and organize (one doc, reordered/rotated pages).
///
/// Page content, resources and annotations are carried. So are the
/// document-level structures that users notice when they vanish: the
/// AcroForm field tree (filtered to fields whose widgets are on kept pages),
/// bookmarks/outlines (concatenated across sources; entries pointing at
/// dropped pages become inert headings), file attachments, document info
/// and XMP metadata, viewer preferences, optional-content configuration,
/// and — for a single source with every page kept in order — page labels.
///
/// Pages that are *not* selected never reach the output, even when a
/// bookmark, link or form field pointed at them: such references are nulled
/// before unreachable objects are pruned.
///
/// Still not carried (stated, not hidden): the tagged-PDF structure tree,
/// document-level JavaScript, and named destinations as a table (links and
/// bookmarks that used them are rewritten to explicit destinations).
pub fn assemble(parts: Vec<(Document, Vec<PageSel>)>) -> Result<Document> {
    let mut out = Document::with_version("1.7");
    let mut max_id: u32 = 1;
    let mut ordered_pages: Vec<(ObjectId, Dictionary)> = Vec::new();
    let mut sources: Vec<SourceMeta> = Vec::new();
    let single_identity = parts.len() == 1 && {
        let (doc, sels) = &parts[0];
        let n = doc.get_pages().len() as u32;
        sels.len() as u32 == n && sels.iter().enumerate().all(|(i, s)| s.page == i as u32 + 1)
    };

    // Attributes a page may inherit from its Pages ancestors; they must be
    // copied onto the page itself because we re-parent everything.
    const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

    for (mut doc, sels) in parts {
        doc.renumber_objects_with(max_id);
        max_id = doc.max_id + 1;
        let page_map = doc.get_pages();
        let mut kept_here: Vec<ObjectId> = Vec::with_capacity(sels.len());

        for sel in &sels {
            let &page_id = page_map
                .get(&sel.page)
                .ok_or_else(|| anyhow!("page {} out of range (document has {} pages)", sel.page, page_map.len()))?;
            let mut dict = doc
                .get_object(page_id)
                .map_err(|e| anyhow!("bad page object: {e}"))?
                .as_dict()
                .map_err(|e| anyhow!("page {} is not a dictionary: {e}", sel.page))?
                .clone();

            for key in INHERITABLE {
                if dict.get(key).is_err() {
                    if let Some(v) = inherited(&doc.objects, page_id, key) {
                        dict.set(key.to_vec(), v);
                    }
                }
            }
            if sel.extra_rotation != 0 {
                let current = dict.get(b"Rotate").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0);
                dict.set("Rotate", Object::Integer((current + sel.extra_rotation).rem_euclid(360)));
            }
            dict.remove(b"Parent");
            ordered_pages.push((page_id, dict));
            kept_here.push(page_id);
        }

        sources.push(carry::source_meta(&doc, page_map.values().copied().collect(), kept_here));

        // Copy every object except old catalogs and pages-tree nodes.
        for (id, obj) in doc.objects.iter() {
            match dict_type(obj).as_deref() {
                Some(b"Catalog") | Some(b"Pages") => continue,
                _ => {
                    out.objects.insert(*id, obj.clone());
                }
            }
        }
    }

    if ordered_pages.is_empty() {
        return Err(anyhow!("no pages selected — nothing to assemble"));
    }

    // Fresh pages tree + catalog.
    let pages_id: ObjectId = (max_id, 0);
    max_id += 1;
    let kids: Vec<Object> = ordered_pages.iter().map(|(id, _)| Object::Reference(*id)).collect();
    let count = ordered_pages.len() as i64;

    let kept_pages: HashSet<ObjectId> = ordered_pages.iter().map(|(id, _)| *id).collect();
    let removed_pages: HashSet<ObjectId> = sources
        .iter()
        .flat_map(|s| s.all_pages.iter().copied())
        .filter(|id| !kept_pages.contains(id))
        .collect();
    let kept_annots = carry::annotation_ids(&out.objects, ordered_pages.iter().map(|(_, d)| d));

    for (id, mut dict) in ordered_pages {
        dict.set("Parent", Object::Reference(pages_id));
        out.objects.insert(id, Object::Dictionary(dict));
    }

    let mut pages_dict = Dictionary::new();
    pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
    pages_dict.set("Kids", Object::Array(kids));
    pages_dict.set("Count", Object::Integer(count));
    out.objects.insert(pages_id, Object::Dictionary(pages_dict));

    let catalog_id: ObjectId = (max_id, 0);
    max_id += 1;
    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_id));

    // Document-level carry-over. Each helper only touches objects that were
    // copied above and allocates new ones from `max_id` onward.
    let mut alloc = carry::IdAlloc::new(max_id);
    carry::carry_catalog_extras(&mut out.objects, &mut catalog, &sources);
    carry::carry_acroform(&mut out.objects, &mut catalog, &sources, &kept_annots, single_identity, &mut alloc);
    carry::carry_outlines(&mut out.objects, &mut catalog, &sources, &kept_pages, &mut alloc);
    carry::carry_embedded_files(&mut out.objects, &mut catalog, &sources, &mut alloc);
    carry::carry_open_action(&mut out.objects, &mut catalog, &sources, &kept_pages);
    if single_identity {
        if let Some(labels) = sources[0].catalog.get(b"PageLabels").ok().cloned() {
            catalog.set("PageLabels", labels);
        }
    }
    carry::sanitize_page_links(&mut out.objects, &sources, &kept_pages);
    if let Some(info) = sources.iter().find_map(|s| s.info.clone()) {
        out.trailer.set("Info", info);
    }

    out.objects.insert(catalog_id, Object::Dictionary(catalog));
    out.trailer.set("Root", Object::Reference(catalog_id));
    out.max_id = alloc.next_id();

    // Nothing may keep a dropped page alive: null every reference to one,
    // then prune whatever became unreachable (the dropped pages included).
    carry::null_out_references(&mut out.objects, &mut out.trailer, &removed_pages);
    out.prune_objects();
    out.renumber_objects();
    out.compress();
    Ok(out)
}

/// All pages of a document as plain selections (helper for merge).
pub fn all_pages(doc: &Document) -> Vec<PageSel> {
    (1..=doc.get_pages().len() as u32).map(PageSel::plain).collect()
}

/// Rotate the given 1-based pages in place by `delta` degrees clockwise.
pub fn rotate_pages(doc: &mut Document, pages: &[u32], delta: i64) -> Result<()> {
    let page_map = doc.get_pages();
    for p in pages {
        let &id = page_map.get(p).ok_or_else(|| anyhow!("page {p} out of range"))?;
        let current = inherited(&doc.objects, id, b"Rotate")
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(0);
        let dict = doc
            .get_object_mut(id)
            .map_err(|e| anyhow!("bad page object: {e}"))?
            .as_dict_mut()
            .map_err(|e| anyhow!("page {p} is not a dictionary: {e}"))?;
        dict.set("Rotate", Object::Integer((current + delta).rem_euclid(360)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn roundtrip(doc: &mut Document) -> Document {
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        Document::load_mem(&buf).unwrap()
    }

    fn catalog(doc: &Document) -> Dictionary {
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary(root).unwrap().clone()
    }

    /// Terminal field names reachable from /AcroForm /Fields.
    fn field_names(doc: &Document) -> Vec<String> {
        fn walk(doc: &Document, obj: &Object, prefix: &str, out: &mut Vec<String>) {
            let Ok(id) = obj.as_reference() else { return };
            let Ok(d) = doc.get_dictionary(id) else { return };
            let name = d
                .get(b"T")
                .ok()
                .and_then(|t| t.as_str().ok())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .unwrap_or_default();
            let full = if prefix.is_empty() { name } else if name.is_empty() { prefix.to_owned() } else { format!("{prefix}.{name}") };
            match d.get(b"Kids") {
                Ok(Object::Array(kids)) => {
                    for k in kids {
                        walk(doc, k, &full, out);
                    }
                }
                _ => out.push(full),
            }
        }
        let mut out = Vec::new();
        let cat = catalog(doc);
        let Ok(acro) = cat.get(b"AcroForm") else { return out };
        let acro = match acro {
            Object::Reference(id) => doc.get_dictionary(*id).unwrap().clone(),
            Object::Dictionary(d) => d.clone(),
            _ => return out,
        };
        if let Ok(Object::Array(fields)) = acro.get(b"Fields") {
            for f in fields {
                walk(doc, f, "", &mut out);
            }
        }
        out.sort();
        out
    }

    #[test]
    fn merge_counts_pages() {
        let a = load(fixture("sample-a.pdf")).unwrap();
        let b = load(fixture("sample-b.pdf")).unwrap();
        let sel_a = all_pages(&a);
        let sel_b = all_pages(&b);
        let mut merged = assemble(vec![(a, sel_a), (b, sel_b)]).unwrap();
        let out = tmp("pdfaxe-test-merged.pdf");
        save(&mut merged, &out).unwrap();
        let reloaded = load(&out).unwrap();
        assert_eq!(reloaded.get_pages().len(), 8);
    }

    #[test]
    fn extract_subset() {
        let a = load(fixture("sample-a.pdf")).unwrap();
        let sels = vec![PageSel::plain(1), PageSel::plain(3), PageSel::plain(5)];
        let mut doc = assemble(vec![(a, sels)]).unwrap();
        let out = tmp("pdfaxe-test-extract.pdf");
        save(&mut doc, &out).unwrap();
        let reloaded = load(&out).unwrap();
        assert_eq!(reloaded.get_pages().len(), 3);
        // page 2 of the extract should be source page 3
        let text = extract_text(&reloaded, 2).unwrap();
        assert!(text.contains("page 3"), "expected source page 3 text, got: {text}");
    }

    #[test]
    fn reorder_and_rotate_via_assemble() {
        let a = load(fixture("sample-a.pdf")).unwrap();
        let sels = vec![
            PageSel { page: 5, extra_rotation: 90 },
            PageSel::plain(1),
        ];
        let mut doc = assemble(vec![(a, sels)]).unwrap();
        let out = tmp("pdfaxe-test-reorder.pdf");
        save(&mut doc, &out).unwrap();
        let reloaded = load(&out).unwrap();
        assert_eq!(reloaded.get_pages().len(), 2);
        let pages = reloaded.get_pages();
        let first_id = pages[&1];
        let dict = reloaded.get_object(first_id).unwrap().as_dict().unwrap();
        assert_eq!(dict.get(b"Rotate").unwrap().as_i64().unwrap(), 90);
        let text = extract_text(&reloaded, 1).unwrap();
        assert!(text.contains("page 5"), "expected source page 5 first, got: {text}");
    }

    #[test]
    fn rotate_in_place() {
        let mut a = load(fixture("sample-a.pdf")).unwrap();
        rotate_pages(&mut a, &[2], 180).unwrap();
        let out = tmp("pdfaxe-test-rotate.pdf");
        save(&mut a, &out).unwrap();
        let reloaded = load(&out).unwrap();
        let pages = reloaded.get_pages();
        let dict = reloaded.get_object(pages[&2]).unwrap().as_dict().unwrap();
        assert_eq!(dict.get(b"Rotate").unwrap().as_i64().unwrap(), 180);
    }

    #[test]
    fn info_reads_basics() {
        let a = load(fixture("sample-a.pdf")).unwrap();
        let i = info(&a);
        assert_eq!(i.pages, 5);
        assert!(!i.encrypted);
    }

    #[test]
    fn page_sizes_reads_mediaboxes() {
        let a = load(fixture("sample-a.pdf")).unwrap();
        let sizes = page_sizes(&a);
        assert_eq!(sizes.len(), 5);
        assert!(sizes.iter().all(|&(w, h)| w > 100.0 && h > w), "portrait pages expected: {sizes:?}");
    }

    #[test]
    fn save_is_atomic_and_overwrites() {
        let out = tmp("pdfaxe-test-atomic.pdf");
        std::fs::write(&out, b"stale bytes").unwrap();
        let mut a = load(fixture("sample-a.pdf")).unwrap();
        save(&mut a, &out).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"%PDF"), "destination must hold the new file");
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(out.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("pdfaxe-test-atomic.pdf.pdfaxe-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file must be renamed away: {leftovers:?}");
    }

    #[test]
    fn page_view_maps_rotations_both_ways() {
        let v = PageView { x0: 100.0, y0: 50.0, w: 400.0, h: 600.0, rotate: 0 };
        assert_eq!(v.to_display(100.0, 50.0), (0.0, 0.0));
        assert_eq!(v.to_display(500.0, 650.0), (400.0, 600.0));
        for rot in [0, 90, 180, 270] {
            let v = PageView { rotate: rot, ..v };
            let (dw, dh) = v.display_size();
            if rot % 180 == 0 { assert_eq!((dw, dh), (400.0, 600.0)) } else { assert_eq!((dw, dh), (600.0, 400.0)) }
            for (ux, uy) in [(100.0, 50.0), (500.0, 650.0), (250.0, 300.0), (123.0, 456.0)] {
                let (dx, dy) = v.to_display(ux, uy);
                assert!(dx >= -0.001 && dx <= dw + 0.001 && dy >= -0.001 && dy <= dh + 0.001, "rot {rot}: ({dx},{dy}) outside display");
                let (bx, by) = v.from_display(dx, dy);
                assert!((bx - ux).abs() < 1e-3 && (by - uy).abs() < 1e-3, "rot {rot}: round trip failed");
            }
        }
        // /Rotate 90 turns the page clockwise: the user-space bottom-left
        // corner becomes the displayed top-left.
        let v90 = PageView { rotate: 90, ..v };
        assert_eq!(v90.to_display(100.0, 50.0), (0.0, 400.0));
        assert_eq!(v90.to_display(500.0, 50.0), (0.0, 0.0));
    }

    #[test]
    fn visible_box_honors_cropbox_and_origin() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let pages = doc.get_pages();
        let p1 = pages[&1];
        let page = doc.get_object_mut(p1).unwrap().as_dict_mut().unwrap();
        page.set("MediaBox", vec![Object::Integer(100), Object::Integer(100), Object::Integer(700), Object::Integer(900)]);
        page.set("CropBox", vec![Object::Integer(150), Object::Integer(120), Object::Integer(650), Object::Integer(880)]);
        page.set("Rotate", Object::Integer(90));
        let v = page_view(&doc, 1).unwrap();
        assert_eq!((v.x0, v.y0, v.w, v.h, v.rotate), (150.0, 120.0, 500.0, 760.0, 90));
        assert_eq!(page_sizes(&doc)[0], (760.0, 500.0));
        // ...and the renderer agrees on the display size.
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let img = crate::render::render_page_rgba(&buf, 1, 400).unwrap();
        assert!((img.point_width - 760.0).abs() < 0.5 && (img.point_height - 500.0).abs() < 0.5, "renderer dims {}x{}", img.point_width, img.point_height);
    }

    #[test]
    fn assemble_keeps_form_fields_info_and_metadata_on_identity_save() {
        let mut src = load(fixture("sample-form.pdf")).unwrap();
        let before = field_names(&src);
        assert_eq!(before.len(), 3, "fixture should have three fields: {before:?}");
        // Give it Info + XMP so we can watch them survive.
        let info_id = src.add_object(dictionary! { "Title" => Object::string_literal("Quarterly Form"), "Author" => Object::string_literal("Someone") });
        src.trailer.set("Info", Object::Reference(info_id));
        let xmp = src.add_object(lopdf::Stream::new(dictionary! { "Type" => "Metadata", "Subtype" => "XML" }, b"<x:xmpmeta/>".to_vec()));
        let root = src.trailer.get(b"Root").unwrap().as_reference().unwrap();
        src.get_dictionary_mut(root).unwrap().set("Metadata", Object::Reference(xmp));

        let sels = all_pages(&src);
        let mut out = assemble(vec![(src, sels)]).unwrap();
        let re = roundtrip(&mut out);
        assert_eq!(field_names(&re), before, "every field must survive an identity save");
        let cat = catalog(&re);
        assert!(cat.get(b"Metadata").is_ok(), "XMP must be carried");
        let info = re.trailer.get(b"Info").unwrap().as_reference().unwrap();
        let title = re.get_dictionary(info).unwrap().get(b"Title").unwrap().as_str().unwrap();
        assert_eq!(title, b"Quarterly Form");
    }

    #[test]
    fn assemble_filters_fields_to_kept_pages_and_drops_removed_page_content() {
        // Build a two-page form: page 1 = sample-form's page, page 2 = sample-a page 1,
        // with one text field on each page.
        let form = load(fixture("sample-form.pdf")).unwrap();
        let a = load(fixture("sample-a.pdf")).unwrap();
        let merged = assemble(vec![(form, vec![PageSel::plain(1)]), (a, vec![PageSel::plain(1)])]).unwrap();
        let mut merged = roundtrip(&mut merged.clone());
        assert_eq!(merged.get_pages().len(), 2);
        assert_eq!(field_names(&merged).len(), 3);

        // Add a field whose widget sits on page 2.
        let p2 = merged.get_pages()[&2];
        let widget = merged.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "T" => Object::string_literal("page2.note"),
            "V" => Object::string_literal("ONLY-ON-PAGE-TWO"), "Rect" => vec![10.into(), 10.into(), 100.into(), 30.into()],
            "P" => Object::Reference(p2),
        });
        merged.get_dictionary_mut(p2).unwrap().set("Annots", vec![Object::Reference(widget)]);
        let root = merged.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let acro_ref = merged.get_dictionary(root).unwrap().get(b"AcroForm").unwrap().clone();
        match acro_ref {
            Object::Reference(id) => {
                let acro = merged.get_dictionary_mut(id).unwrap();
                let mut fields = acro.get(b"Fields").unwrap().as_array().unwrap().clone();
                fields.push(Object::Reference(widget));
                acro.set("Fields", fields);
            }
            _ => panic!("AcroForm should be indirect after assemble"),
        }
        let merged = roundtrip(&mut merged);
        assert_eq!(field_names(&merged).len(), 4);

        // Keep only page 1: the page-2 field (and its value) must be gone,
        // and so must every byte of page 2's content.
        let mut only_first = assemble(vec![(merged, vec![PageSel::plain(1)])]).unwrap();
        let re = roundtrip(&mut only_first);
        assert_eq!(re.get_pages().len(), 1);
        let names = field_names(&re);
        assert_eq!(names.len(), 3, "the page-2 field must be filtered out: {names:?}");
        for (_, obj) in &re.objects {
            if let Object::Stream(s) = obj {
                let data = s.decompressed_content().unwrap_or_else(|_| s.content.clone());
                assert!(!data.windows(14).any(|w| w == b"SECRET-CODE-42" || w == b"5345435245542D"), "dropped page content leaked");
            }
            if let Ok(d) = obj.as_dict() {
                if let Ok(v) = d.get(b"V") {
                    assert_ne!(v.as_str().ok(), Some(&b"ONLY-ON-PAGE-TWO"[..]), "dropped page's field value leaked");
                }
            }
        }
    }

    #[test]
    fn assemble_carries_and_sanitizes_bookmarks() {
        let mut src = load(fixture("sample-a.pdf")).unwrap();
        let pages = src.get_pages();
        let (p1, p3) = (pages[&1], pages[&3]);
        // Outline: "One" → page 1 (explicit), "Three" → page 3 (named dest).
        let item1 = src.new_object_id();
        let item3 = src.new_object_id();
        let outlines = src.new_object_id();
        src.objects.insert(item1, Object::Dictionary(dictionary! {
            "Title" => Object::string_literal("One"), "Parent" => Object::Reference(outlines),
            "Next" => Object::Reference(item3),
            "Dest" => vec![Object::Reference(p1), Object::Name(b"Fit".to_vec())],
        }));
        src.objects.insert(item3, Object::Dictionary(dictionary! {
            "Title" => Object::string_literal("Three"), "Parent" => Object::Reference(outlines),
            "Prev" => Object::Reference(item1),
            "Dest" => Object::Name(b"chapter3".to_vec()),
        }));
        src.objects.insert(outlines, Object::Dictionary(dictionary! {
            "Type" => "Outlines", "First" => Object::Reference(item1), "Last" => Object::Reference(item3), "Count" => 2,
        }));
        let root = src.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let cat = src.get_dictionary_mut(root).unwrap();
        cat.set("Outlines", Object::Reference(outlines));
        cat.set("Dests", dictionary! { "chapter3" => vec![Object::Reference(p3), Object::Name(b"Fit".to_vec())] });

        // Keep pages 1 and 2 only.
        let mut out = assemble(vec![(src, vec![PageSel::plain(1), PageSel::plain(2)])]).unwrap();
        let re = roundtrip(&mut out);
        let cat = catalog(&re);
        let ol = cat.get(b"Outlines").unwrap().as_reference().unwrap();
        let ol = re.get_dictionary(ol).unwrap();
        let first = ol.get(b"First").unwrap().as_reference().unwrap();
        let one = re.get_dictionary(first).unwrap();
        assert_eq!(one.get(b"Title").unwrap().as_str().unwrap(), b"One");
        let dest = one.get(b"Dest").unwrap().as_array().unwrap();
        assert_eq!(dest[0].as_reference().unwrap(), re.get_pages()[&1], "explicit dest must follow the page");
        let next = one.get(b"Next").unwrap().as_reference().unwrap();
        let three = re.get_dictionary(next).unwrap();
        assert_eq!(three.get(b"Title").unwrap().as_str().unwrap(), b"Three");
        assert!(three.get(b"Dest").is_err() && three.get(b"A").is_err(), "a bookmark to a dropped page must go inert");
        assert_eq!(ol.get(b"Count").unwrap().as_i64().unwrap(), 2);
        // ...and page 3's content didn't ride along via the bookmark.
        assert_eq!(re.get_pages().len(), 2);
        let dropped_page_dicts = re.objects.values().filter(|o| dict_type(o).as_deref() == Some(b"Page")).count();
        assert_eq!(dropped_page_dicts, 2);
    }

    #[test]
    fn merge_concatenates_outlines_and_attachments() {
        fn with_outline_and_file(mut doc: Document, title: &str, file: &str) -> Document {
            let p1 = doc.get_pages()[&1];
            let item = doc.new_object_id();
            let ol = doc.new_object_id();
            doc.objects.insert(item, Object::Dictionary(dictionary! {
                "Title" => Object::string_literal(title), "Parent" => Object::Reference(ol),
                "Dest" => vec![Object::Reference(p1), Object::Name(b"Fit".to_vec())],
            }));
            doc.objects.insert(ol, Object::Dictionary(dictionary! {
                "Type" => "Outlines", "First" => Object::Reference(item), "Last" => Object::Reference(item), "Count" => 1,
            }));
            let ef = doc.add_object(lopdf::Stream::new(dictionary! { "Type" => "EmbeddedFile" }, b"hello".to_vec()));
            let fs = doc.add_object(dictionary! {
                "Type" => "Filespec", "F" => Object::string_literal(file), "UF" => Object::string_literal(file),
                "EF" => dictionary! { "F" => Object::Reference(ef) },
            });
            let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
            let cat = doc.get_dictionary_mut(root).unwrap();
            cat.set("Outlines", Object::Reference(ol));
            cat.set("Names", dictionary! {
                "EmbeddedFiles" => dictionary! { "Names" => vec![Object::string_literal(file), Object::Reference(fs)] },
            });
            doc
        }
        let a = with_outline_and_file(load(fixture("sample-a.pdf")).unwrap(), "Doc A", "a.txt");
        let b = with_outline_and_file(load(fixture("sample-b.pdf")).unwrap(), "Doc B", "b.txt");
        let (sa, sb) = (all_pages(&a), all_pages(&b));
        let mut out = assemble(vec![(a, sa), (b, sb)]).unwrap();
        let re = roundtrip(&mut out);
        let cat = catalog(&re);
        let ol = re.get_dictionary(cat.get(b"Outlines").unwrap().as_reference().unwrap()).unwrap();
        assert_eq!(ol.get(b"Count").unwrap().as_i64().unwrap(), 2);
        let first = re.get_dictionary(ol.get(b"First").unwrap().as_reference().unwrap()).unwrap();
        let last = re.get_dictionary(ol.get(b"Last").unwrap().as_reference().unwrap()).unwrap();
        assert_eq!(first.get(b"Title").unwrap().as_str().unwrap(), b"Doc A");
        assert_eq!(last.get(b"Title").unwrap().as_str().unwrap(), b"Doc B");
        assert_eq!(first.get(b"Next").unwrap().as_reference().unwrap(), ol.get(b"Last").unwrap().as_reference().unwrap());
        // Doc B's bookmark points at the merged page 6 (B's first page).
        let dest = last.get(b"Dest").unwrap().as_array().unwrap();
        assert_eq!(dest[0].as_reference().unwrap(), re.get_pages()[&6]);
        // Both attachments present, sorted by name.
        let names = cat.get(b"Names").unwrap().as_dict().unwrap();
        let ef = re.get_dictionary(names.get(b"EmbeddedFiles").unwrap().as_reference().unwrap()).unwrap();
        let arr = ef.get(b"Names").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0].as_str().unwrap(), b"a.txt");
        assert_eq!(arr[2].as_str().unwrap(), b"b.txt");
    }
}
