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

## Still awaiting human-in-the-loop verification

- Click-text-on-page selection + Apply edit in the GUI (engine path underneath
  is test-covered)
- Dialog Apply buttons end-to-end
