//! Document-level structures that have to ride along when a document is
//! rebuilt page by page: the AcroForm field tree, outlines (bookmarks), file
//! attachments, info/metadata, optional-content configuration, the open
//! action, and the link/bookmark destinations that tie them to pages.
//!
//! Everything here works on a plain object map so [`crate::ops::assemble`]
//! can use it on the document it is building, and [`crate::scrub`] can use
//! the same primitives on a finished document.

use std::collections::{BTreeMap, HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

pub(crate) type Objects = BTreeMap<ObjectId, Object>;

/// What `assemble` needs to remember about one source document after its
/// objects were renumbered and copied.
pub(crate) struct SourceMeta {
    /// The source catalog (not copied into the output — only mined).
    pub catalog: Dictionary,
    /// Trailer /Info, usually a reference to a copied object.
    pub info: Option<Object>,
    /// Every page of the source, in order (indices double as page numbers
    /// for integer destinations).
    pub all_pages: Vec<ObjectId>,
    /// Pages selected from this source.
    pub kept: Vec<ObjectId>,
    /// Named destinations (/Dests dictionary and /Names → /Dests tree).
    pub named_dests: HashMap<Vec<u8>, Object>,
}

pub(crate) fn source_meta(doc: &Document, all_pages: Vec<ObjectId>, kept: Vec<ObjectId>) -> SourceMeta {
    let catalog = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|r| r.as_reference().ok())
        .and_then(|id| doc.get_dictionary(id).ok())
        .cloned()
        .unwrap_or_default();
    let info = doc.trailer.get(b"Info").ok().cloned().filter(|o| match o {
        Object::Reference(id) => doc.objects.get(id).is_some_and(|x| x.as_dict().is_ok()),
        Object::Dictionary(_) => true,
        _ => false,
    });
    let named_dests = named_dests_of(&doc.objects, &catalog);
    SourceMeta { catalog, info, all_pages, kept, named_dests }
}

/// Hands out fresh object ids above everything a build has used so far.
pub(crate) struct IdAlloc {
    next: u32,
}

impl IdAlloc {
    pub(crate) fn new(start: u32) -> Self {
        Self { next: start }
    }
    pub(crate) fn alloc(&mut self) -> ObjectId {
        let id = (self.next, 0);
        self.next += 1;
        id
    }
    pub(crate) fn next_id(&self) -> u32 {
        self.next
    }
}

// ----- low-level helpers -------------------------------------------------

pub(crate) fn resolve<'a>(objects: &'a Objects, obj: &'a Object) -> &'a Object {
    let mut cur = obj;
    for _ in 0..32 {
        match cur {
            Object::Reference(id) => match objects.get(id) {
                Some(o) => cur = o,
                None => return cur,
            },
            _ => return cur,
        }
    }
    cur
}

pub(crate) fn dict_of(objects: &Objects, obj: &Object) -> Option<Dictionary> {
    match resolve(objects, obj) {
        Object::Dictionary(d) => Some(d.clone()),
        Object::Stream(s) => Some(s.dict.clone()),
        _ => None,
    }
}

pub(crate) fn array_of(objects: &Objects, obj: &Object) -> Option<Vec<Object>> {
    match resolve(objects, obj) {
        Object::Array(a) => Some(a.clone()),
        _ => None,
    }
}

pub(crate) fn dict_mut(objects: &mut Objects, id: ObjectId) -> Option<&mut Dictionary> {
    match objects.get_mut(&id) {
        Some(Object::Dictionary(d)) => Some(d),
        Some(Object::Stream(s)) => Some(&mut s.dict),
        _ => None,
    }
}

fn reference(o: &Object) -> Option<ObjectId> {
    o.as_reference().ok()
}

/// Flatten a name tree (PDF 7.9.6) into (key, value) leaves.
pub(crate) fn name_tree_leaves(objects: &Objects, node: &Object, out: &mut Vec<(Vec<u8>, Object)>, depth: u8) {
    if depth > 32 {
        return;
    }
    let Some(d) = dict_of(objects, node) else { return };
    if let Some(names) = d.get(b"Names").ok().and_then(|n| array_of(objects, n)) {
        for pair in names.chunks(2) {
            if let [k, v] = pair {
                if let Ok(key) = resolve(objects, k).as_str() {
                    out.push((key.to_vec(), v.clone()));
                }
            }
        }
    }
    if let Some(kids) = d.get(b"Kids").ok().and_then(|k| array_of(objects, k)) {
        for kid in kids {
            name_tree_leaves(objects, &kid, out, depth + 1);
        }
    }
}

/// Every named destination a catalog knows: the legacy /Dests dictionary and
/// the /Names → /Dests name tree.
pub(crate) fn named_dests_of(objects: &Objects, catalog: &Dictionary) -> HashMap<Vec<u8>, Object> {
    let mut out = HashMap::new();
    if let Some(dests) = catalog.get(b"Dests").ok().and_then(|d| dict_of(objects, d)) {
        for (k, v) in dests.iter() {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(names) = catalog.get(b"Names").ok().and_then(|n| dict_of(objects, n)) {
        if let Ok(tree) = names.get(b"Dests") {
            let mut leaves = Vec::new();
            name_tree_leaves(objects, tree, &mut leaves, 0);
            for (k, v) in leaves {
                out.entry(k).or_insert(v);
            }
        }
    }
    out
}

/// Resolve a destination — explicit array, name, string, or a dictionary
/// with /D — to an explicit array whose first element is a page reference.
pub(crate) fn explicit_dest(
    objects: &Objects,
    dest: &Object,
    named: &HashMap<Vec<u8>, Object>,
    pages: &[ObjectId],
    depth: u8,
) -> Option<Vec<Object>> {
    if depth > 8 {
        return None;
    }
    match dest {
        Object::Array(a) => {
            let mut a = a.clone();
            match a.first()? {
                Object::Reference(_) => {}
                Object::Integer(n) => {
                    let idx = usize::try_from(*n).ok()?;
                    a[0] = Object::Reference(*pages.get(idx)?);
                }
                _ => return None,
            }
            Some(a)
        }
        Object::Name(n) => explicit_dest(objects, named.get(n.as_slice())?, &HashMap::new(), pages, depth + 1),
        Object::String(s, _) => explicit_dest(objects, named.get(s.as_slice())?, &HashMap::new(), pages, depth + 1),
        Object::Dictionary(d) => explicit_dest(objects, d.get(b"D").ok()?, named, pages, depth + 1),
        Object::Stream(s) => explicit_dest(objects, s.dict.get(b"D").ok()?, named, pages, depth + 1),
        Object::Reference(id) => explicit_dest(objects, objects.get(id)?, named, pages, depth + 1),
        _ => None,
    }
}

pub(crate) fn dest_page(arr: &[Object]) -> Option<ObjectId> {
    arr.first().and_then(reference)
}

/// The page an outline item / link / open action points at, if any.
pub(crate) fn target_page(
    objects: &Objects,
    holder: &Dictionary,
    named: &HashMap<Vec<u8>, Object>,
    pages: &[ObjectId],
) -> Option<ObjectId> {
    if let Ok(d) = holder.get(b"Dest") {
        return explicit_dest(objects, d, named, pages, 0).and_then(|a| dest_page(&a));
    }
    let action = holder.get(b"A").ok().and_then(|a| dict_of(objects, a))?;
    let s = action.get(b"S").ok().and_then(|s| s.as_name().ok()).unwrap_or_default();
    if s != b"GoTo" {
        return None;
    }
    explicit_dest(objects, action.get(b"D").ok()?, named, pages, 0).and_then(|a| dest_page(&a))
}

/// Rewrite the /Dest or GoTo /A of a holder (outline item, link annotation,
/// open action) to an explicit destination when its page is kept, and drop
/// it when the page is not. Indirect action dictionaries that change are
/// returned in `writebacks` for the caller to store.
pub(crate) fn sanitize_dest_holder(
    objects: &Objects,
    holder: &mut Dictionary,
    named: &HashMap<Vec<u8>, Object>,
    pages: &[ObjectId],
    kept: &HashSet<ObjectId>,
    writebacks: &mut Vec<(ObjectId, Object)>,
) -> bool {
    let mut changed = false;
    if let Ok(d) = holder.get(b"Dest").cloned() {
        match explicit_dest(objects, &d, named, pages, 0) {
            Some(arr) if dest_page(&arr).is_some_and(|p| kept.contains(&p)) => {
                if Object::Array(arr.clone()) != d {
                    holder.set("Dest", Object::Array(arr));
                    changed = true;
                }
            }
            _ => {
                holder.remove(b"Dest");
                changed = true;
            }
        }
    }
    if let Ok(a) = holder.get(b"A").cloned() {
        let (action_id, action) = match &a {
            Object::Reference(id) => (Some(*id), dict_of(objects, &a)),
            Object::Dictionary(d) => (None, Some(d.clone())),
            _ => (None, None),
        };
        if let Some(mut act) = action {
            let s = act.get(b"S").ok().and_then(|s| s.as_name().ok()).map(|n| n.to_vec()).unwrap_or_default();
            if s == b"GoTo" {
                let keep = act
                    .get(b"D")
                    .ok()
                    .cloned()
                    .and_then(|d| explicit_dest(objects, &d, named, pages, 0))
                    .filter(|arr| dest_page(arr).is_some_and(|p| kept.contains(&p)));
                match keep {
                    Some(arr) => {
                        act.set("D", Object::Array(arr));
                        match action_id {
                            Some(id) => writebacks.push((id, Object::Dictionary(act))),
                            None => holder.set("A", Object::Dictionary(act)),
                        }
                        changed = true;
                    }
                    None => {
                        holder.remove(b"A");
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

// ----- outlines ----------------------------------------------------------

/// Follow a First → Next chain.
fn chain(objects: &Objects, first: Option<ObjectId>) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = first;
    while let Some(id) = cur {
        if !seen.insert(id) || out.len() > 200_000 {
            break;
        }
        out.push(id);
        cur = objects
            .get(&id)
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| d.get(b"Next").ok())
            .and_then(reference);
    }
    out
}

/// Direct children of an outline node (root or item).
pub(crate) fn outline_children(objects: &Objects, node: ObjectId) -> Vec<ObjectId> {
    let first = objects
        .get(&node)
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"First").ok())
        .and_then(reference);
    chain(objects, first)
}

/// Every item under a node, depth-first.
pub(crate) fn outline_items(objects: &Objects, node: ObjectId) -> Vec<ObjectId> {
    fn walk(objects: &Objects, node: ObjectId, out: &mut Vec<ObjectId>, seen: &mut HashSet<ObjectId>, depth: u16) {
        if depth > 256 {
            return;
        }
        for child in outline_children(objects, node) {
            if !seen.insert(child) {
                continue;
            }
            out.push(child);
            walk(objects, child, out, seen, depth + 1);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(node);
    walk(objects, node, &mut out, &mut seen, 0);
    out
}

fn item_count(objects: &Objects, id: ObjectId) -> i64 {
    objects
        .get(&id)
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"Count").ok())
        .and_then(|c| c.as_i64().ok())
        .unwrap_or(0)
}

/// Number of items a viewer shows under `node` with the current open/closed
/// states — the PDF /Count rule.
fn visible_count(objects: &Objects, node: ObjectId, depth: u16) -> i64 {
    if depth > 256 {
        return 0;
    }
    outline_children(objects, node)
        .into_iter()
        .map(|c| {
            let open = item_count(objects, c) > 0;
            1 + if open { visible_count(objects, c, depth + 1) } else { 0 }
        })
        .sum()
}

/// Recompute every /Count under `root` after items were added or removed,
/// preserving each item's open/closed state.
pub(crate) fn recount_outline(objects: &mut Objects, root: ObjectId) {
    let mut items = outline_items(objects, root);
    items.reverse(); // children before parents
    for id in items {
        let kids = outline_children(objects, id);
        let was_closed = item_count(objects, id) < 0;
        let vis = if kids.is_empty() { 0 } else { visible_count(objects, id, 0) };
        if let Some(d) = dict_mut(objects, id) {
            if vis == 0 {
                d.remove(b"Count");
            } else {
                d.set("Count", Object::Integer(if was_closed { -vis } else { vis }));
            }
        }
    }
    let total = visible_count(objects, root, 0);
    if let Some(d) = dict_mut(objects, root) {
        d.set("Count", Object::Integer(total));
    }
}

/// Carry outlines from every source, concatenated at the top level under a
/// fresh root. Entries whose page was dropped lose their destination and
/// stay as inert headings; named destinations become explicit.
pub(crate) fn carry_outlines(
    objects: &mut Objects,
    catalog: &mut Dictionary,
    sources: &[SourceMeta],
    kept_pages: &HashSet<ObjectId>,
    alloc: &mut IdAlloc,
) {
    let mut tops: Vec<ObjectId> = Vec::new();
    for src in sources {
        let Some(root) = src.catalog.get(b"Outlines").ok().and_then(reference) else { continue };
        if !objects.contains_key(&root) {
            continue;
        }
        for item in outline_items(objects, root) {
            let Some(mut d) = objects.get(&item).and_then(|o| o.as_dict().ok()).cloned() else { continue };
            let mut wb = Vec::new();
            if sanitize_dest_holder(objects, &mut d, &src.named_dests, &src.all_pages, kept_pages, &mut wb) {
                objects.insert(item, Object::Dictionary(d));
            }
            for (id, o) in wb {
                objects.insert(id, o);
            }
        }
        tops.extend(outline_children(objects, root));
        // The old root is unreachable from the new catalog; drop it now so
        // nothing dangles.
        objects.remove(&root);
    }
    if tops.is_empty() {
        return;
    }
    let root_id = alloc.alloc();
    let n = tops.len();
    for (i, &t) in tops.iter().enumerate() {
        let Some(d) = dict_mut(objects, t) else { continue };
        d.set("Parent", Object::Reference(root_id));
        if i == 0 {
            d.remove(b"Prev");
        } else {
            d.set("Prev", Object::Reference(tops[i - 1]));
        }
        if i + 1 == n {
            d.remove(b"Next");
        } else {
            d.set("Next", Object::Reference(tops[i + 1]));
        }
    }
    let mut root = Dictionary::new();
    root.set("Type", Object::Name(b"Outlines".to_vec()));
    root.set("First", Object::Reference(tops[0]));
    root.set("Last", Object::Reference(tops[n - 1]));
    objects.insert(root_id, Object::Dictionary(root));
    recount_outline(objects, root_id);
    catalog.set("Outlines", Object::Reference(root_id));
}

// ----- forms -------------------------------------------------------------

/// Ids of every indirect annotation on the given page dictionaries.
pub(crate) fn annotation_ids<'a>(objects: &Objects, pages: impl Iterator<Item = &'a Dictionary>) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    for page in pages {
        if let Some(annots) = page.get(b"Annots").ok().and_then(|a| array_of(objects, a)) {
            for a in annots {
                if let Some(id) = reference(&a) {
                    out.insert(id);
                }
            }
        }
    }
    out
}

/// Decide whether a field (sub)tree survives: widgets must sit on a kept
/// page; non-terminal fields keep the kids that survive and go when none do.
/// Ids of surviving nodes are collected into `kept_ids`.
pub(crate) fn keep_field(
    objects: &mut Objects,
    id: ObjectId,
    kept_annots: &HashSet<ObjectId>,
    kept_ids: &mut HashSet<ObjectId>,
    depth: u8,
) -> bool {
    if depth > 32 {
        return false;
    }
    let Some(d) = objects.get(&id).and_then(|o| o.as_dict().ok()).cloned() else { return false };
    if let Some(kids) = d.get(b"Kids").ok().and_then(|k| array_of(objects, k)) {
        let kept: Vec<Object> = kids
            .into_iter()
            .filter(|k| match k {
                Object::Reference(kid) => keep_field(objects, *kid, kept_annots, kept_ids, depth + 1),
                _ => false,
            })
            .collect();
        if kept.is_empty() {
            return false;
        }
        if let Some(dm) = dict_mut(objects, id) {
            dm.set("Kids", Object::Array(kept));
        }
        kept_ids.insert(id);
        return true;
    }
    if kept_annots.contains(&id) {
        kept_ids.insert(id);
        true
    } else {
        false
    }
}

fn merge_resources(objects: &Objects, base: &mut Dictionary, other: &Dictionary) {
    for (cat, v) in other.iter() {
        let Some(theirs) = dict_of(objects, v) else { continue };
        let mut ours = base.get(cat).ok().and_then(|o| dict_of(objects, o)).unwrap_or_default();
        for (k, val) in theirs.iter() {
            if ours.get(k).is_err() {
                ours.set(k.clone(), val.clone());
            }
        }
        base.set(cat.clone(), Object::Dictionary(ours));
    }
}

/// Carry the AcroForm: fields filtered to kept pages, /Fields and /CO
/// concatenated across sources, default resources merged.
pub(crate) fn carry_acroform(
    objects: &mut Objects,
    catalog: &mut Dictionary,
    sources: &[SourceMeta],
    kept_annots: &HashSet<ObjectId>,
    single_identity: bool,
    alloc: &mut IdAlloc,
) {
    let mut base: Option<Dictionary> = None;
    let mut fields: Vec<Object> = Vec::new();
    let mut kept_ids: HashSet<ObjectId> = HashSet::new();
    let mut need_app = false;
    let mut co: Vec<Object> = Vec::new();
    for src in sources {
        let Some(acro_obj) = src.catalog.get(b"AcroForm").ok().cloned() else { continue };
        let Some(acro) = dict_of(objects, &acro_obj) else { continue };
        match &mut base {
            None => base = Some(acro.clone()),
            Some(b) => {
                if let (Some(mut ours), Some(theirs)) = (
                    b.get(b"DR").ok().and_then(|o| dict_of(objects, o)),
                    acro.get(b"DR").ok().and_then(|o| dict_of(objects, o)),
                ) {
                    merge_resources(objects, &mut ours, &theirs);
                    b.set("DR", Object::Dictionary(ours));
                } else if let Some(theirs) = acro.get(b"DR").ok().cloned() {
                    if b.get(b"DR").is_err() {
                        b.set("DR", theirs);
                    }
                }
            }
        }
        need_app |= acro.get(b"NeedAppearances").ok().and_then(|o| o.as_bool().ok()).unwrap_or(false);
        if let Some(fs) = acro.get(b"Fields").ok().and_then(|f| array_of(objects, f)) {
            for f in fs {
                if let Object::Reference(id) = f {
                    if keep_field(objects, id, kept_annots, &mut kept_ids, 0) {
                        fields.push(f);
                    }
                }
            }
        }
        if let Some(c) = acro.get(b"CO").ok().and_then(|f| array_of(objects, f)) {
            co.extend(c);
        }
    }
    let Some(mut acro) = base else { return };
    if fields.is_empty() {
        return;
    }
    acro.set("Fields", Object::Array(fields));
    if need_app {
        acro.set("NeedAppearances", Object::Boolean(true));
    }
    let co: Vec<Object> = co
        .into_iter()
        .filter(|o| matches!(o, Object::Reference(id) if kept_ids.contains(id)))
        .collect();
    if co.is_empty() {
        acro.remove(b"CO");
    } else {
        acro.set("CO", Object::Array(co));
    }
    if !single_identity {
        // An XFA template describes the whole original form; it can't be
        // trusted after pages were dropped or merged.
        acro.remove(b"XFA");
    }
    let id = alloc.alloc();
    objects.insert(id, Object::Dictionary(acro));
    catalog.set("AcroForm", Object::Reference(id));
}

// ----- the rest of the catalog -------------------------------------------

/// Metadata, viewer preferences, language, page layout/mode and the
/// optional-content configuration (OCG lists merged across sources).
pub(crate) fn carry_catalog_extras(objects: &mut Objects, catalog: &mut Dictionary, sources: &[SourceMeta]) {
    for key in [&b"Metadata"[..], b"ViewerPreferences", b"PageLayout", b"PageMode", b"Lang"] {
        if let Some(v) = sources.iter().find_map(|s| s.catalog.get(key).ok().cloned()) {
            if key == b"Metadata" {
                // Only carry a real XMP stream.
                if !matches!(resolve(objects, &v), Object::Stream(_)) {
                    continue;
                }
            }
            catalog.set(key.to_vec(), v);
        }
    }
    let mut ocp: Option<Dictionary> = None;
    let mut ocgs: Vec<Object> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    for src in sources {
        let Some(d) = src.catalog.get(b"OCProperties").ok().and_then(|o| dict_of(objects, o)) else { continue };
        if ocp.is_none() {
            ocp = Some(d.clone());
        }
        if let Some(list) = d.get(b"OCGs").ok().and_then(|o| array_of(objects, o)) {
            for g in list {
                match reference(&g) {
                    Some(id) if seen.insert(id) => ocgs.push(g),
                    Some(_) => {}
                    None => ocgs.push(g),
                }
            }
        }
    }
    if let Some(mut d) = ocp {
        d.set("OCGs", Object::Array(ocgs));
        catalog.set("OCProperties", Object::Dictionary(d));
    }
}

/// File attachments from every source, as one flat /EmbeddedFiles name tree.
pub(crate) fn carry_embedded_files(
    objects: &mut Objects,
    catalog: &mut Dictionary,
    sources: &[SourceMeta],
    alloc: &mut IdAlloc,
) {
    let mut leaves: Vec<(Vec<u8>, Object)> = Vec::new();
    for src in sources {
        let Some(names) = src.catalog.get(b"Names").ok().and_then(|n| dict_of(objects, n)) else { continue };
        if let Ok(tree) = names.get(b"EmbeddedFiles") {
            name_tree_leaves(objects, tree, &mut leaves, 0);
        }
    }
    if leaves.is_empty() {
        return;
    }
    // Keys must be unique and sorted.
    let mut used: HashSet<Vec<u8>> = HashSet::new();
    for (k, _) in leaves.iter_mut() {
        let mut cand = k.clone();
        let mut n = 2;
        while !used.insert(cand.clone()) {
            cand = k.clone();
            cand.extend_from_slice(format!(" ({n})").as_bytes());
            n += 1;
        }
        *k = cand;
    }
    leaves.sort_by(|a, b| a.0.cmp(&b.0));
    let mut arr = Vec::with_capacity(leaves.len() * 2);
    for (k, v) in leaves {
        arr.push(Object::String(k, lopdf::StringFormat::Literal));
        arr.push(v);
    }
    let mut tree = Dictionary::new();
    tree.set("Names", Object::Array(arr));
    let tree_id = alloc.alloc();
    objects.insert(tree_id, Object::Dictionary(tree));
    let mut names = Dictionary::new();
    names.set("EmbeddedFiles", Object::Reference(tree_id));
    catalog.set("Names", Object::Dictionary(names));
}

/// The first source's /OpenAction, if it still points at a kept page.
pub(crate) fn carry_open_action(
    objects: &mut Objects,
    catalog: &mut Dictionary,
    sources: &[SourceMeta],
    kept_pages: &HashSet<ObjectId>,
) {
    let Some(src) = sources.first() else { return };
    let Some(oa) = src.catalog.get(b"OpenAction").ok().cloned() else { return };
    let mut holder = Dictionary::new();
    match explicit_dest(objects, &oa, &src.named_dests, &src.all_pages, 0) {
        Some(arr) => holder.set("Dest", Object::Array(arr)),
        None => holder.set("A", oa),
    }
    let mut wb = Vec::new();
    sanitize_dest_holder(objects, &mut holder, &src.named_dests, &src.all_pages, kept_pages, &mut wb);
    for (id, o) in wb {
        objects.insert(id, o);
    }
    if let Ok(d) = holder.get(b"Dest") {
        catalog.set("OpenAction", d.clone());
    } else if let Some(act) = holder.get(b"A").ok().and_then(|a| dict_of(objects, a)) {
        // Only navigation actions are carried; anything else (JavaScript,
        // launches) doesn't belong in a rebuilt document.
        if act.get(b"S").ok().and_then(|s| s.as_name().ok()) == Some(b"GoTo") {
            catalog.set("OpenAction", holder.get(b"A").unwrap().clone());
        }
    }
}

/// Link annotations on kept pages: named destinations become explicit,
/// links to dropped pages lose their target.
pub(crate) fn sanitize_page_links(objects: &mut Objects, sources: &[SourceMeta], kept_pages: &HashSet<ObjectId>) {
    for src in sources {
        for &page_id in &src.kept {
            let Some(page) = objects.get(&page_id).and_then(|o| o.as_dict().ok()).cloned() else { continue };
            let Some(annots_obj) = page.get(b"Annots").ok().cloned() else { continue };
            let Some(annots) = array_of(objects, &annots_obj) else { continue };
            let mut rewritten = annots.clone();
            let mut array_changed = false;
            for (i, a) in annots.iter().enumerate() {
                let (annot_id, dict) = match a {
                    Object::Reference(id) => (Some(*id), objects.get(id).and_then(|o| o.as_dict().ok()).cloned()),
                    Object::Dictionary(d) => (None, Some(d.clone())),
                    _ => (None, None),
                };
                let Some(mut d) = dict else { continue };
                if d.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) != Some(b"Link") {
                    continue;
                }
                let mut wb = Vec::new();
                if sanitize_dest_holder(objects, &mut d, &src.named_dests, &src.all_pages, kept_pages, &mut wb) {
                    match annot_id {
                        Some(id) => {
                            objects.insert(id, Object::Dictionary(d));
                        }
                        None => {
                            rewritten[i] = Object::Dictionary(d);
                            array_changed = true;
                        }
                    }
                }
                for (id, o) in wb {
                    objects.insert(id, o);
                }
            }
            if array_changed {
                match &annots_obj {
                    Object::Reference(id) => {
                        objects.insert(*id, Object::Array(rewritten));
                    }
                    _ => {
                        if let Some(p) = dict_mut(objects, page_id) {
                            p.set("Annots", Object::Array(rewritten));
                        }
                    }
                }
            }
        }
    }
}

/// Replace every reference to one of `removed` with null, everywhere.
pub(crate) fn null_out_references(objects: &mut Objects, trailer: &mut Dictionary, removed: &HashSet<ObjectId>) {
    if removed.is_empty() {
        return;
    }
    fn walk(o: &mut Object, removed: &HashSet<ObjectId>) {
        match o {
            Object::Reference(id) if removed.contains(id) => *o = Object::Null,
            Object::Array(a) => a.iter_mut().for_each(|x| walk(x, removed)),
            Object::Dictionary(d) => d.iter_mut().for_each(|(_, v)| walk(v, removed)),
            Object::Stream(s) => s.dict.iter_mut().for_each(|(_, v)| walk(v, removed)),
            _ => {}
        }
    }
    for (_, o) in objects.iter_mut() {
        walk(o, removed);
    }
    for (_, v) in trailer.iter_mut() {
        walk(v, removed);
    }
}
