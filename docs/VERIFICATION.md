# Verification log

Every operation ships only after it is test-covered in the engine, exercised
end-to-end through the CLI, and — where it draws pixels — render-verified
through hayro with outputs cross-checked by an independent parser
(pdf-lib/Node). This is the running log.

## July 12, 2026

- 18/18 core tests: merge page counts, extract subset (content lands on right pages),
  reorder+rotate via assemble, in-place rotate, info, range parsing, text runs with
  positions, in-place edit, find/replace, unencodable-text rejection, /Differences
  decode+edit, MacRoman/Standard encodings, symbolic-font read-only, watermark
  page-targeting, page-number templates, stamped-doc save/reload
- Watermark + page numbers render-verified: diagonal translucent CONFIDENTIAL through
  page center (hayro honors the ExtGState alpha), "1 / 5" bottom center, original
  content intact
- CLI E2E on fixtures; outputs cross-verified with an independent parser (pdf-lib/Node)
- Pure-Rust render of a fixture page: fonts, weights, colors, layout all faithful
- Text edit E2E: `edit-text` replaced SECRET-CODE-42 across 5 pages; hayro render of the
  output shows the new text at the exact position/font/color of the old
- GUI launched + screenshot-verified: thumbnail page list (lazy, 2/frame budget), zoom
  controls (fit-width default, steps, Ctrl+scroll), edit-mode panel listing runs

## July 13, 2026

- Same-line reflow: an edit shifts the rest of its line by the real width
  delta (glyph metrics from /Widths or built-in standard-14 AFM tables) and un-shifts
  at the next line break; falls back to in-place when widths are unknown or the text
  matrix is rotated. 27 core tests; render-verified via CLI edit-text + hayro.
- Watermark + page-numbers GUI dialogs: screenshot-verified in both themes
  via `--watermark` / `--page-numbers` deep-link flags; the engine calls beneath the
  Apply buttons are the same test-covered watermark()/page_numbers() paths.
- Continuous multi-page scroll: all pages in one scroll viewport, layout
  pre-sized from MediaBoxes (`page_sizes()`), 1 rasterization/frame budget, textures
  evicted outside a viewport-height band, page list follows scroll and vice versa;
  screenshot-verified via `--page 4` deep-link (viewport and thumbnail both land on
  page 4).
- Paragraph re-wrap: edits redistribute words across the paragraph's
  existing lines (positions untouched — only strings change), overflow cascades
  down and shrink pulls words up; recognition requires uniform font/size/margin/
  leading and near-full lines, so ragged line-lists (addresses) refuse and fall
  back to same-line reflow. Render-verified: a 40-char-longer replacement wraps
  onto the next line instead of overflowing the margin.
- Redaction by raster burn: the page is re-rendered with the boxes
  burned black and its content replaced by that bitmap — extract_text on the
  redacted page returns nothing, other pages untouched, annotations dropped;
  CLI + render verified (black box exactly over SECRET-CODE-42, ~56 KB page at
  200 dpi).
- Images↔PDF: `images` builds one page per image (JPEG embedded
  as-is via DCTDecode — lossless; PNG re-encoded Flate RGB, alpha over white),
  `render --pages "1-3"` exports numbered PNGs; round-trip verified (PDF →
  PNGs → PDF → faithful render).
- GUI redact mode + image drop zone + keyboard nav: drag boxes on
  any page (live preview, per-box remove, burn button drives the test-covered
  redact_page), dropping PNG/JPEG files builds and opens a fresh PDF,
  PgUp/PgDn/Home/End navigate pages. Mode UI screenshot-verified via
  `--redact`; the drag gesture and drop gesture still need a human hand.
- In-document search: Ctrl+F box in the top bar, case-insensitive
  across all pages' runs, ◀ ▶ match stepping, each jump scrolls to the page
  and highlights the run. Screenshot-verified via `--find SECRET` (counter
  1/5, highlight box on the match).

## November 8, 2026 — audit response

Three independent code audits landed this week; two of them proved the same
Critical bug. Everything below was fixed and is now test-covered (61 core
tests, up from 40).

- **Redaction leak (Critical).** `redact_page` replaced the page's /Contents
  but never removed the old stream, and the CLI save path never pruned, so a
  "redacted" file still carried the original page as an unreferenced object.
  Reproduced before the fix with a raw-object scan of the CLI output (5
  streams held the marker; the orphan was byte-identical to the original
  page). Fix: the page dictionary is rebuilt from scratch (dropping /Annots,
  /Thumb, /StructParents, /PieceInfo and the rest), form fields whose widgets
  lived on the page are detached from the AcroForm, the tag tree is dropped,
  and `prune_objects()` runs inside `redact_page` itself. Regression test
  decompresses every object of a plain `save_to` output and asserts the
  marker survives on exactly the four untouched pages. Re-verified through
  the CLI binary after the fix: 4 streams, zero orphans.
- **Redaction scrub options.** `redact_pages` + `RedactOpts`: Info/XMP/
  PieceInfo stripped and bookmarks to burned pages removed by default; CLI
  `--keep-metadata` / `--keep-bookmarks`, GUI checkboxes. New `scrub` CLI
  command and GUI "Strip metadata" button. Tests cover both the default and
  the opt-out.
- **GUI save destroyed forms/bookmarks/metadata (High).** `assemble` wrote a
  bare catalog. It now carries the AcroForm (fields filtered to kept pages,
  /Fields and /CO merged across sources, DR merged), outlines (concatenated
  at the top level, named destinations rewritten to explicit ones, entries to
  dropped pages made inert, /Count recomputed), attachments (flat
  /EmbeddedFiles tree), Info, XMP, viewer prefs, page layout/mode, language,
  OCProperties, OpenAction, and PageLabels for identity saves — and nulls
  every reference to a dropped page before pruning, so a bookmark can no
  longer keep a removed page's content alive. Tests: identity save keeps all
  three fixture fields + Info + XMP; subset extract filters a page-2 field
  and leaks none of page 2; bookmarks sanitize; two-document merge
  concatenates outlines and attachments. CLI `extract`/`merge` re-verified
  against `sample-form.pdf`: AcroForm present in the output.
- **GUI save lifecycle (High).** Save now assembles from the live `Document`
  (not a possibly stale byte snapshot), re-opens the saved file so the title,
  path and dirty flag describe what is on disk, and is atomic (temp file +
  rename). Added Save / Ctrl+S (overwrites the opened file; falls back to
  Save as… for unsaved image builds), an unsaved-changes prompt on window
  close (Yes / No / Cancel via `close_requested` + `CancelClose`), and
  `(edited)` resets after a save. Re-serialization failures are surfaced in
  the status bar instead of swallowed.
- **Display geometry (Medium).** New `PageView` (CropBox ∩ MediaBox +
  /Rotate) maps user space ↔ display space. Used by the GUI (click-to-select,
  highlight boxes, redaction boxes), by `redact_page` (burn target and
  rebuilt page size follow the renderer), and by stamps (page numbers land
  at the displayed bottom centre of rotated/cropped pages, upright). Tests:
  round-trip mapping for all four rotations; renderer dimensions agree with
  `page_sizes` on a rotated, cropped, offset-origin page; a redaction on a
  rotated+cropped page burns exactly where the viewer shows the text (render
  verified).
- **Text engine (Medium).** ToUnicode parser rewritten as a tokenizer:
  array-form `bfrange`, `codespacerange`, `<< >>` dictionaries and
  `/glyphname` destinations no longer corrupt mappings. `text_runs` tracks
  `q`/`Q`/`cm` and the full text matrix (positions and widths are now true
  user-space values; `display_size` reports the rendered size), recurses into
  form XObjects (`Do`) with their /Matrix and own resources — those runs are
  listed and searchable, read-only, and `edit_run` refuses them with a clear
  message. Type3 fonts measure through /FontMatrix and decode only through
  /Differences. `find_replace` counts occurrences, not runs, and rejects an
  empty needle.
- **Smaller fixes.** Search results re-run after every edit (stale run
  indices); search jumps show the match preview; mixed drag-drop opens the
  PDF and says what was ignored; `--find`/`--page` without a document warn
  instead of silently doing nothing; redact drag commits on release outside
  the page; burn DPI selectable (150/200/300); dead code removed (the
  "preview" field is used, the deep orange is the Burn button); Courier
  matched by family not prefix; JPEG zero-size guard; font walk depth 6;
  `render --pages` reads the file once; site demo commands are now valid
  invocations (`-o`); site download links pinned to the v0.1.0 release URL;
  `cargo audit` added to CI with documented exceptions.
- **Verification performed:** `cargo test --workspace` 61/61; `cargo build
  --workspace` zero warnings; CLI redact/extract/merge/scrub/edit-text E2E
  on fixtures with an independent raw-object scanner; GUI smoke launch with
  `--redact --page 3 --find SECRET` deep links.

## Still awaiting human-in-the-loop verification

- Save / Ctrl+S / Save as… round trip and the close-prompt dialog
- Redact drag gesture (incl. release outside the window) and the Burn button
  with the metadata/bookmark options
- Click-text-on-page selection + Apply edit on rotated and cropped pages
- Dialog Apply buttons end-to-end
