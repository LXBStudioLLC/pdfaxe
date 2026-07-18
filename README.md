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
crates/pdfaxe-app    the Windows GUI (open/drop a PDF — or drop images to build one,
                     thumbnail page list with reorder/rotate/delete, continuous
                     multi-page scroll view with zoom + PgUp/PgDn/Home/End, text-edit
                     mode with click-on-page selection, redact mode with drag-to-box
                     burning, in-document search (Ctrl+F) with match stepping,
                     watermark + page-number dialogs, save-as; flags: --dark --edit
                     --redact --watermark --page-numbers --page N --find TEXT)
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
cargo run -p pdfaxe-cli -- redact in.pdf -o out.pdf --page 1 --rect "38,635,130,22"
cargo run -p pdfaxe-cli -- render in.pdf -o pg.png --pages "1-3" --width 800   # pg-1.png…
cargo run -p pdfaxe-cli -- images -o out.pdf scan1.jpg scan2.png
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
- Continuous multi-page scroll (July 13): all pages in one scroll viewport, layout
  pre-sized from MediaBoxes (`page_sizes()`), 1 rasterization/frame budget, textures
  evicted outside a viewport-height band, page list follows scroll and vice versa;
  screenshot-verified via `--page 4` deep-link (viewport and thumbnail both land on
  page 4).
- Paragraph re-wrap (July 13): edits redistribute words across the paragraph's
  existing lines (positions untouched — only strings change), overflow cascades
  down and shrink pulls words up; recognition requires uniform font/size/margin/
  leading and near-full lines, so ragged line-lists (addresses) refuse and fall
  back to same-line reflow. Render-verified: a 40-char-longer replacement wraps
  onto the next line instead of overflowing the margin.
- Redaction by raster burn (July 13): the page is re-rendered with the boxes
  burned black and its content replaced by that bitmap — extract_text on the
  redacted page returns nothing, other pages untouched, annotations dropped;
  CLI + render verified (black box exactly over SECRET-CODE-42, ~56 KB page at
  200 dpi).
- Images↔PDF (July 13): `images` builds one page per image (JPEG embedded
  as-is via DCTDecode — lossless; PNG re-encoded Flate RGB, alpha over white),
  `render --pages "1-3"` exports numbered PNGs; round-trip verified (PDF →
  PNGs → PDF → faithful render).
- GUI redact mode + image drop zone + keyboard nav (July 13): drag boxes on
  any page (live preview, per-box remove, burn button drives the test-covered
  redact_page), dropping PNG/JPEG files builds and opens a fresh PDF,
  PgUp/PgDn/Home/End navigate pages. Mode UI screenshot-verified via
  `--redact`; the drag gesture and drop gesture still need a human hand.
- In-document search (July 13): Ctrl+F box in the top bar, case-insensitive
  across all pages' runs, ◀ ▶ match stepping, each jump scrolls to the page
  and highlights the run. Screenshot-verified via `--find SECRET` (counter
  1/5, highlight box on the match).
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
   `reflow_line`). ~~Paragraph re-wrap~~ (`try_rewrap` — conservative block
   detection, strings-only redistribution). Next: font re-embedding for arbitrary
   new glyphs (shaping via `rustybuzz`, font parsing/subsetting via
   `skrifa`/`klippa`), then cross-block re-wrap (new-line creation).
2. ~~App~~: ~~thumbnails~~, ~~zoom~~, ~~watermark/page-number dialogs~~,
   ~~multi-page continuous scroll + texture eviction~~, ~~keyboard navigation~~,
   ~~redact box-drawing~~, ~~image drop zone~~, ~~search-in-document~~ — done.
3. ~~More ops at parity with the web suite~~: ~~images→PDF~~, ~~PDF→images~~,
   ~~watermark~~, ~~page numbers~~, ~~redact~~ — engine, CLI and GUI all done.
4. ~~Distribution~~: signed per-user MSI with AGPL license dialog, "Open with
   PDFAxe" association, Start Menu + PATH; CI + tagged releases (see below).
5. Windows polish: jump lists, recent files, dark mode following system.

## Installer + releases

`build-msi.ps1` builds the per-user MSI (WiX 3.14 binaries + `cargo-wix`; set
`PDFAXE_WIX_BIN` if WiX lives elsewhere). Both exes and the MSI are signed via
Azure Trusted Signing (`ci/sign.ps1`, override with `LXB_SIGN_SCRIPT`); without
`AZURE_TENANT_ID`/`AZURE_CLIENT_ID`/`AZURE_CLIENT_SECRET` in the environment the
build completes unsigned with a warning. The installer opens with the AGPL
license dialog — `wix/License.rtf` and the dialog art are committed, regenerated
from `LICENSE` / `assets/logo.png` by `wix/make-license-rtf.ps1` and
`wix/make-installer-art.ps1`.

CI (`.github/workflows/ci.yml`) runs the test suite and packages an unsigned
MSI artifact on every push and PR. Pushing a `vX.Y.Z` tag (it must match the
workspace version in `Cargo.toml`) runs `release.yml`: signed build, hard
signature verification, sha256 checksum, and a **draft** GitHub release for
human review before publishing.

## License

[AGPL-3.0-or-later](LICENSE). Free forever — copyleft guarantees every fork and
hosted derivative stays open source.
