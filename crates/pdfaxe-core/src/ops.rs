use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use lopdf::{Dictionary, Document, Object, ObjectId};

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

pub fn save(doc: &mut Document, path: impl AsRef<Path>) -> Result<()> {
    doc.save(path.as_ref())
        .with_context(|| format!("couldn't write {}", path.as_ref().display()))?;
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
/// Known v0 limitations (documented, not hidden): document-level extras —
/// bookmarks/outlines, AcroForm field trees, attachments — are not carried
/// over. Page content, resources and annotations are.
pub fn assemble(parts: Vec<(Document, Vec<PageSel>)>) -> Result<Document> {
    let mut out = Document::with_version("1.7");
    let mut max_id: u32 = 1;
    let mut ordered_pages: Vec<(ObjectId, Dictionary)> = Vec::new();

    // Attributes a page may inherit from its Pages ancestors; they must be
    // copied onto the page itself because we re-parent everything.
    const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

    for (mut doc, sels) in parts {
        doc.renumber_objects_with(max_id);
        max_id = doc.max_id + 1;
        let page_map = doc.get_pages();

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
        }

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
    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_id));
    out.objects.insert(catalog_id, Object::Dictionary(catalog));
    out.trailer.set("Root", Object::Reference(catalog_id));
    out.max_id = catalog_id.0;

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
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
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
}
