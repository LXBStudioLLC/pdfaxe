//! Document-level scrubbing — the parts of a PDF that can carry information
//! about a page without being *on* the page: info dictionary and XMP
//! metadata, application private data, bookmarks that name what a page was
//! about, form fields that still hold a value after their widget is gone, and
//! the tagged-PDF structure tree with its text alternates.

use std::collections::HashSet;

use lopdf::{Document, Object, ObjectId};

use crate::carry::{self, Objects};

/// What [`strip_metadata`] removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StripReport {
    /// The trailer's /Info dictionary (Title, Author, Producer, dates …).
    pub info: bool,
    /// The catalog's XMP metadata stream.
    pub xmp: bool,
    /// /PieceInfo application-private dictionaries (document + pages).
    pub piece_info: usize,
    /// Page-level /Metadata streams.
    pub page_metadata: usize,
}

impl StripReport {
    pub fn is_empty(&self) -> bool {
        !self.info && !self.xmp && self.piece_info == 0 && self.page_metadata == 0
    }
}

fn catalog_id(doc: &Document) -> Option<ObjectId> {
    doc.trailer.get(b"Root").ok()?.as_reference().ok()
}

/// Remove document info, XMP metadata and application private data. The
/// objects themselves are pruned so nothing stays behind as an orphan.
pub fn strip_metadata(doc: &mut Document) -> StripReport {
    let mut rep = StripReport::default();
    if doc.trailer.remove(b"Info").is_some() {
        rep.info = true;
    }
    if let Some(cat) = catalog_id(doc) {
        if let Ok(c) = doc.get_dictionary_mut(cat) {
            if c.remove(b"Metadata").is_some() {
                rep.xmp = true;
            }
            if c.remove(b"PieceInfo").is_some() {
                rep.piece_info += 1;
            }
        }
    }
    let pages: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    for id in pages {
        if let Ok(p) = doc.get_dictionary_mut(id) {
            if p.remove(b"Metadata").is_some() {
                rep.page_metadata += 1;
            }
            if p.remove(b"PieceInfo").is_some() {
                rep.piece_info += 1;
            }
        }
    }
    doc.prune_objects();
    rep
}

fn has_doomed_ancestor(objects: &Objects, item: ObjectId, doomed: &HashSet<ObjectId>) -> bool {
    let mut cur = item;
    for _ in 0..256 {
        let parent = objects
            .get(&cur)
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| d.get(b"Parent").ok())
            .and_then(|p| p.as_reference().ok());
        match parent {
            Some(p) if doomed.contains(&p) => return true,
            Some(p) => cur = p,
            None => return false,
        }
    }
    false
}

/// Take an outline item out of its sibling chain and its parent's
/// First/Last pointers.
fn unlink_outline_item(objects: &mut Objects, item: ObjectId) {
    let Some(d) = objects.get(&item).and_then(|o| o.as_dict().ok()).cloned() else { return };
    let prev = d.get(b"Prev").ok().and_then(|o| o.as_reference().ok());
    let next = d.get(b"Next").ok().and_then(|o| o.as_reference().ok());
    let parent = d.get(b"Parent").ok().and_then(|o| o.as_reference().ok());
    if let Some(p) = prev {
        if let Some(pd) = carry::dict_mut(objects, p) {
            match next {
                Some(n) => pd.set("Next", Object::Reference(n)),
                None => {
                    pd.remove(b"Next");
                }
            }
        }
    }
    if let Some(n) = next {
        if let Some(nd) = carry::dict_mut(objects, n) {
            match prev {
                Some(p) => nd.set("Prev", Object::Reference(p)),
                None => {
                    nd.remove(b"Prev");
                }
            }
        }
    }
    if let Some(par) = parent {
        if let Some(pd) = carry::dict_mut(objects, par) {
            if pd.get(b"First").ok().and_then(|o| o.as_reference().ok()) == Some(item) {
                match next {
                    Some(n) => pd.set("First", Object::Reference(n)),
                    None => {
                        pd.remove(b"First");
                    }
                }
            }
            if pd.get(b"Last").ok().and_then(|o| o.as_reference().ok()) == Some(item) {
                match prev {
                    Some(p) => pd.set("Last", Object::Reference(p)),
                    None => {
                        pd.remove(b"Last");
                    }
                }
            }
        }
    }
}

/// Remove every bookmark (outline item) whose destination is one of the
/// given 1-based pages, together with its sub-items. Returns how many items
/// were removed.
pub fn drop_outline_items_for_pages(doc: &mut Document, pages: &[u32]) -> usize {
    let page_map = doc.get_pages();
    let targets: HashSet<ObjectId> = pages.iter().filter_map(|p| page_map.get(p).copied()).collect();
    if targets.is_empty() {
        return 0;
    }
    let all_pages: Vec<ObjectId> = page_map.values().copied().collect();
    let Some(cat_id) = catalog_id(doc) else { return 0 };
    let Some(catalog) = doc.get_dictionary(cat_id).ok().cloned() else { return 0 };
    let Some(root) = catalog.get(b"Outlines").ok().and_then(|o| o.as_reference().ok()) else { return 0 };
    let named = carry::named_dests_of(&doc.objects, &catalog);
    let objects = &mut doc.objects;

    let doomed: HashSet<ObjectId> = carry::outline_items(objects, root)
        .into_iter()
        .filter(|&item| {
            objects
                .get(&item)
                .and_then(|o| o.as_dict().ok())
                .and_then(|d| carry::target_page(objects, d, &named, &all_pages))
                .is_some_and(|p| targets.contains(&p))
        })
        .collect();
    if doomed.is_empty() {
        return 0;
    }
    let mut removed = 0;
    for &item in &doomed {
        if has_doomed_ancestor(objects, item, &doomed) {
            continue; // goes with its ancestor
        }
        let subtree = carry::outline_items(objects, item);
        unlink_outline_item(objects, item);
        objects.remove(&item);
        for s in &subtree {
            objects.remove(s);
        }
        removed += 1 + subtree.len();
    }
    carry::recount_outline(objects, root);
    removed
}

fn prune_field(objects: &mut Objects, id: ObjectId, gone: &HashSet<ObjectId>, removed: &mut usize, depth: u8) -> bool {
    if depth > 32 {
        return true;
    }
    if gone.contains(&id) {
        *removed += 1;
        objects.remove(&id);
        return false;
    }
    let kids = objects
        .get(&id)
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"Kids").ok().cloned())
        .and_then(|k| carry::array_of(objects, &k));
    let Some(kids) = kids else { return true };
    let kept: Vec<Object> = kids
        .into_iter()
        .filter(|k| match k {
            Object::Reference(kid) => prune_field(objects, *kid, gone, removed, depth + 1),
            _ => true,
        })
        .collect();
    if kept.is_empty() {
        *removed += 1;
        objects.remove(&id);
        return false;
    }
    if let Some(d) = carry::dict_mut(objects, id) {
        d.set("Kids", Object::Array(kept));
    }
    true
}

/// After annotations were removed from a page, take the form fields whose
/// widgets they were out of the AcroForm tree as well — otherwise the
/// field's /V value would survive in the file with nothing displaying it.
/// Parents left without kids go too. Returns the number of field nodes
/// removed.
pub(crate) fn detach_form_fields(doc: &mut Document, annots: &[ObjectId]) -> usize {
    if annots.is_empty() {
        return 0;
    }
    let gone: HashSet<ObjectId> = annots.iter().copied().collect();
    let Some(cat_id) = catalog_id(doc) else { return 0 };
    let Some(acro_obj) = doc.get_dictionary(cat_id).ok().and_then(|c| c.get(b"AcroForm").ok().cloned()) else { return 0 };
    let acro_id = acro_obj.as_reference().ok();
    let Some(mut acro) = carry::dict_of(&doc.objects, &acro_obj) else { return 0 };
    let Some(fields) = acro.get(b"Fields").ok().and_then(|f| carry::array_of(&doc.objects, f)) else { return 0 };

    let mut removed = 0;
    let kept: Vec<Object> = {
        let objects = &mut doc.objects;
        fields
            .into_iter()
            .filter(|f| match f {
                Object::Reference(id) => prune_field(objects, *id, &gone, &mut removed, 0),
                _ => true,
            })
            .collect()
    };
    if let Some(co) = acro.get(b"CO").ok().and_then(|c| carry::array_of(&doc.objects, c)) {
        let co: Vec<Object> = co
            .into_iter()
            .filter(|o| match o {
                Object::Reference(id) => doc.objects.contains_key(id),
                _ => true,
            })
            .collect();
        if co.is_empty() {
            acro.remove(b"CO");
        } else {
            acro.set("CO", Object::Array(co));
        }
    }
    if kept.is_empty() {
        if let Ok(c) = doc.get_dictionary_mut(cat_id) {
            c.remove(b"AcroForm");
        }
        if let Some(id) = acro_id {
            doc.objects.remove(&id);
        }
    } else {
        acro.set("Fields", Object::Array(kept));
        match acro_id {
            Some(id) => {
                doc.objects.insert(id, Object::Dictionary(acro));
            }
            None => {
                if let Ok(c) = doc.get_dictionary_mut(cat_id) {
                    c.set("AcroForm", Object::Dictionary(acro));
                }
            }
        }
    }
    removed
}

/// Drop the tagged-PDF structure tree. Structure elements can carry
/// /ActualText, /Alt and /E strings — copies of page text that outlive a
/// redaction — so a redacted document loses its tags. Returns whether there
/// was one.
pub(crate) fn drop_structure_tree(doc: &mut Document) -> bool {
    let Some(cat_id) = catalog_id(doc) else { return false };
    let had = match doc.get_dictionary_mut(cat_id) {
        Ok(c) => {
            let had = c.remove(b"StructTreeRoot").is_some();
            c.remove(b"MarkInfo");
            had
        }
        Err(_) => false,
    };
    if had {
        let pages: Vec<ObjectId> = doc.get_pages().values().copied().collect();
        for id in pages {
            if let Ok(p) = doc.get_dictionary_mut(id) {
                p.remove(b"StructParents");
            }
        }
    }
    had
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::load;
    use lopdf::dictionary;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
    }

    #[test]
    fn strip_metadata_removes_info_xmp_and_pieceinfo() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let info = doc.add_object(dictionary! { "Title" => Object::string_literal("Payroll Q3"), "Author" => Object::string_literal("A. Person") });
        doc.trailer.set("Info", Object::Reference(info));
        let xmp = doc.add_object(lopdf::Stream::new(dictionary! { "Type" => "Metadata", "Subtype" => "XML" }, b"<x:xmpmeta>A. Person</x:xmpmeta>".to_vec()));
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let cat = doc.get_dictionary_mut(root).unwrap();
        cat.set("Metadata", Object::Reference(xmp));
        cat.set("PieceInfo", dictionary! { "SomeApp" => dictionary! { "Private" => Object::string_literal("secret") } });

        let rep = strip_metadata(&mut doc);
        assert_eq!(rep, StripReport { info: true, xmp: true, piece_info: 1, page_metadata: 0 });
        assert!(doc.trailer.get(b"Info").is_err());
        assert!(!doc.objects.contains_key(&info), "Info object must be pruned, not orphaned");
        assert!(!doc.objects.contains_key(&xmp), "XMP stream must be pruned, not orphaned");
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        assert!(!buf.windows(9).any(|w| w == b"A. Person"), "author must not survive anywhere in the file");
        assert!(strip_metadata(&mut doc).is_empty());
    }

    #[test]
    fn dropping_bookmarks_for_a_page_removes_subtrees_and_recounts() {
        let mut doc = load(fixture("sample-a.pdf")).unwrap();
        let pages = doc.get_pages();
        let (p1, p2, p3) = (pages[&1], pages[&2], pages[&3]);
        let root = doc.new_object_id();
        let a = doc.new_object_id(); // → p1
        let b = doc.new_object_id(); // → p2, has child c
        let c = doc.new_object_id(); // → p3 (child of b)
        let d = doc.new_object_id(); // → p2
        let item = |title: &str, page: ObjectId, parent: ObjectId| {
            dictionary! {
                "Title" => Object::string_literal(title), "Parent" => Object::Reference(parent),
                "Dest" => vec![Object::Reference(page), Object::Name(b"Fit".to_vec())],
            }
        };
        let mut ia = item("A", p1, root);
        ia.set("Next", Object::Reference(b));
        let mut ib = item("B", p2, root);
        ib.set("Prev", Object::Reference(a));
        ib.set("Next", Object::Reference(d));
        ib.set("First", Object::Reference(c));
        ib.set("Last", Object::Reference(c));
        ib.set("Count", 1);
        let ic = item("C", p3, b);
        let mut id_ = item("D", p2, root);
        id_.set("Prev", Object::Reference(b));
        doc.objects.insert(a, Object::Dictionary(ia));
        doc.objects.insert(b, Object::Dictionary(ib));
        doc.objects.insert(c, Object::Dictionary(ic));
        doc.objects.insert(d, Object::Dictionary(id_));
        doc.objects.insert(root, Object::Dictionary(dictionary! {
            "Type" => "Outlines", "First" => Object::Reference(a), "Last" => Object::Reference(d), "Count" => 4,
        }));
        let cat_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(cat_id).unwrap().set("Outlines", Object::Reference(root));

        // Page 2 is redacted: B (with its child C) and D go; A stays.
        let removed = drop_outline_items_for_pages(&mut doc, &[2]);
        assert_eq!(removed, 3);
        let rd = doc.get_dictionary(root).unwrap();
        assert_eq!(rd.get(b"First").unwrap().as_reference().unwrap(), a);
        assert_eq!(rd.get(b"Last").unwrap().as_reference().unwrap(), a);
        assert_eq!(rd.get(b"Count").unwrap().as_i64().unwrap(), 1);
        let ad = doc.get_dictionary(a).unwrap();
        assert!(ad.get(b"Next").is_err());
        assert!(!doc.objects.contains_key(&b) && !doc.objects.contains_key(&c) && !doc.objects.contains_key(&d));
    }
}
