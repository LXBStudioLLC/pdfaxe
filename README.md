# PDFAxe — Windows app (100% Rust)

The product: a native, local-only PDF app. No uploads, no account, no telemetry —
architecturally incapable of seeing your files. The mission wedge (real in-place text
editing) gets built here.

## Stack — pure Rust, zero C/C++ FFI

| Layer | Crate | Why |
|---|---|---|
| Document surgery | `lopdf` (MIT) | parse/rebuild PDF object graphs |
| Rasterization | `hayro` (typst ecosystem) | pure-Rust PDF renderer, incl. its own JPEG2000/JBIG2 decoders |
| UI | `egui`/`eframe` 0.35 | immediate-mode native GUI |
| Dialogs | `rfd` | native Windows file dialogs |
| CLI | `clap` | scriptable surface + test harness |

Same core compiles to WASM later — the parked web app (`../app`) eventually becomes a
thin shell over this exact engine.

## Workspace

```
crates/pdfaxe-core   engine: assemble() (merge/extract/reorder/rotate in one primitive),
                     ranges, render (hayro behind a swappable module boundary),
                     text (positioned runs, in-place edit, find/replace),
                     stamp (watermark, page numbers — q…Q-sandwiched appends)
crates/pdfaxe-cli    pdfaxe info|merge|extract|rotate|text|render|runs|edit-text|
                     watermark|page-numbers
crates/pdfaxe-app    the Windows GUI (open/drop a PDF, thumbnail page list with
                     reorder/rotate/delete, zoomable rendered view, text-edit mode
                     with click-on-page selection, save-as; flags: --dark --edit)
fixtures/            test PDFs (regenerate via ../app/scripts/make-samples.mjs)
```

Build artifacts go to `$env:USERPROFILE\AppData\Local\pdfaxe-target` (see `.cargo/config.toml`)
to keep OneDrive from syncing gigabytes of `target/`.

## Commands

```bash
cargo test -p pdfaxe-core            # engine tests (11)
cargo run -p pdfaxe-cli -- info fixtures/sample-a.pdf
cargo run -p pdfaxe-cli -- merge -o out.pdf a.pdf b.pdf
cargo run -p pdfaxe-cli -- render fixtures/sample-a.pdf -o page1.png --page 1 --width 1200
cargo run -p pdfaxe-cli -- runs fixtures/sample-a.pdf          # positioned text runs
cargo run -p pdfaxe-cli -- edit-text in.pdf -o out.pdf --find "old" --replace "new"
cargo run -p pdfaxe-cli -- watermark in.pdf -o out.pdf --text "CONFIDENTIAL"
cargo run -p pdfaxe-cli -- page-numbers in.pdf -o out.pdf --format "{page} / {total}"
cargo run -p pdfaxe-app              # the app; also: pdfaxe-app.exe some.pdf ("Open with")
cargo build --release --workspace    # optimized binaries
```

## Verified (July 12, 2026)

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
- Same-line reflow (July 13): an edit shifts the rest of its line by the real width
  delta (glyph metrics from /Widths or built-in standard-14 AFM tables) and un-shifts
  at the next line break; falls back to in-place when widths are unknown or the text
  matrix is rotated. 27 core tests; render-verified via CLI edit-text + hayro.
- Watermark + page-numbers GUI dialogs (July 13): screenshot-verified in both themes
  via `--watermark` / `--page-numbers` deep-link flags; the engine calls beneath the
  Apply buttons are the same test-covered watermark()/page_numbers() paths.
- Not yet human-verified: click-text-on-page selection + Apply edit in the GUI (engine
  path underneath is test-covered), dialog Apply buttons end-to-end

## Engine notes

- `assemble()` copies inheritable page attributes (Resources/MediaBox/CropBox/Rotate)
  onto each page before re-parenting — the correctness detail naive merges miss.
- v0 limitation (documented in the fn docs): document-level extras (bookmarks/outlines,
  AcroForm trees, attachments) aren't carried through `assemble()` yet.
- hayro is young: encrypted PDFs unsupported, some blend modes missing. It sits behind
  `pdfaxe_core::render` so a second backend (or PDFium behind a feature flag as a
  rendering oracle for tests) can be added without touching callers.

## Roadmap

1. **Text editing (the wedge, Phase 1):** ~~v1 shipped~~ — positioned runs, in-place
   same-font edits, find/replace, `/Encoding` names + `/Differences` + symbolic-font
   protection (`pdfaxe-core/src/text.rs`). ~~Same-line reflow~~ (`metrics.rs` +
   `reflow_line`). Next: paragraph re-wrap across lines, then font re-embedding for
   arbitrary new glyphs (shaping via `rustybuzz`, font parsing/subsetting via
   `skrifa`/`klippa`).
2. App: ~~thumbnails~~, ~~zoom~~, ~~watermark/page-number dialogs~~ — remaining:
   multi-page continuous scroll viewport, texture cache eviction for huge documents.
3. More ops at parity with the web suite: images→PDF, PDF→images (have `render`),
   ~~watermark~~, ~~page numbers~~, redact (raster-burn, engine already renders).
4. Distribution: release profile is configured (thin LTO, stripped); add `cargo-wix`
   MSI packaging, file association + "Open with PDFAxe", icon, code signing.
5. Windows polish: jump lists, recent files, dark mode following system.

## License

[AGPL-3.0-or-later](LICENSE). Free forever — copyleft guarantees every fork and
hosted derivative stays open source.
