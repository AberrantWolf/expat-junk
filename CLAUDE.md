# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A desktop GUI for translating PDFs and images **region by region**. The user opens a
document, draws boxes over blocks of text, and each box is translated by a provider the
user configures with their own API key. Translations are baked back onto the original as
an overlay. Clicking a translated region opens an info panel with the source text, the
translation, and (provider permitting) alternatives and explanations.

This is an early scaffold: the module structure and types exist and compile, but the
backend functions are stubs (`anyhow::bail!("… not yet implemented")` / `TODO`). Build out
a module by replacing its stubs, not by re-architecting around them.

## Commands

```bash
cargo run                 # launch the egui app
cargo build               # default build; build.rs vendors PDFium on first run (see below)
cargo build --features ocr   # also build the Tesseract OCR path (see deps below)
cargo check               # fast type-check loop
cargo test                # incl. the headless PDF render test (no display needed)
cargo test renders_a_generated_pdf   # run that single test by name
```

The first build compiles the `junk-libs-pdfium` dependency, whose `build.rs`
downloads the matching PDFium binary (~a few MB) into this build's `OUT_DIR`. It
needs network access once per target dir; subsequent builds are offline
(idempotent — skips if the lib is present). The path dependency means
`../junk-libs` must be checked out alongside this repo.

## Architecture and the decisions behind it

The design was chosen deliberately; don't silently swap these out.

- **GUI: egui/eframe, not Tauri.** egui self-renders identically on every platform and
  ships as one binary. Tauri was rejected because its native-webview backend renders
  inconsistently across OSes (WebKitGTK on Linux is the weakest, and Linux is the dev
  target). The cost we accept is hand-building drag/resize/hit-test instead of getting it
  free from the DOM.
- **Only `src/app.rs` and `src/viewer.rs` touch egui.** Everything else (rendering, OCR,
  API calls, PDF generation) is GUI-agnostic backend code. Preserve this boundary — it's
  what keeps the egui-vs-anything-else decision reversible and the backend testable.
  `viewer.rs` is deliberately self-contained (takes `&mut Document` + the selection, owns
  only its texture and in-progress drag) so it can later be lifted into a shared
  `junk-libs-egui-pdfview` widget. `app.rs`'s top panel is **just a menu bar**
  (File/Edit/Settings); all translation controls (language pickers, the Translate/Re-translate
  button, status/error) live in the **right `info_panel`**, so the bar above the document holds
  only the viewer's own page/zoom controls. Menu/shortcut/panel clicks are collected into an
  `Action` enum (the panels return one) and executed *after* the panels render — this sidesteps
  borrow conflicts between the menu closures and the field-level `&mut` borrows the panels need.
- **eframe 0.34 API note:** the `App` trait method is `fn ui(&mut self, ui: &mut egui::Ui,
  …)` (not the older `update(&mut self, ctx, …)`), and panels use `.show_inside(ui, …)`
  (not `.show(ctx, …)`). Match this when adding UI.

### Data + pipeline

`document → ui → translate → export`, with `config` supplying provider choice and keys.

- **`junk-libs-pdfium`** (external crate, `../junk-libs/junk-libs-pdfium`, path dependency) —
  the GUI-agnostic render core, lifted out of this repo so print-junk can share it. Its own
  `build.rs` vendors the matching PDFium binary into `OUT_DIR` (no setup in this repo).
  `instance()` returns a process-wide `&'static Pdfium`, bound lazily with a system fallback.
  **PDFium can only be initialized once per process** — a second bind fails with
  `PdfiumLibraryBindingsAlreadyInitialized` — so always go through `instance()`, never bind
  directly. `render_pdf()` rasterizes every page to RGBA *and* extracts the text layer as
  `CharBox`es (via `fpdf_text` loose glyph bounds), flipping PDF's bottom-left coords into our
  top-left page space. `render_page_bitmap()` renders a single page at a given scale (px/pt),
  bitmap only — used by `Document::rerender_page` to re-render the current page crisply on zoom.
  `quantize_zoom()` is currently unused. To change PDFium
  versions, edit that crate (the `pdfium_NNNN` feature and `build.rs`'s `PDFIUM_VERSION`
  together). The crate's render/text tests live with it; `tests/pdfium_wiring.rs` here only
  guards that runtime binding survives being consumed as a dependency.
- **`document.rs`** — `Document`/`Page`/`Region`. `Document::open()` dispatches on extension:
  PDFs go through `pdfium::render_pdf`, images load via the `image` crate as a single page.
  `Page::char_boxes` holds `(char, Rect)` from the text layer, for snap-to-text selection and
  the native-text path. **`Rect` is in page coordinates (PDF points, top-left origin), never
  screen pixels** — the UI converts using its zoom; `CharBox` is already flipped to match.
  Regions carry a stable `RegionId` so UI state, cached translations, and export overlays all
  refer to the same box across edits.
- **`viewer.rs`** — the interactive page canvas, with its own control bar (zoom −/%/+, Fit
  width, page ◀/▶). Renders the current page to an egui texture and lets the user draw / select
  / move / resize / delete regions. All geometry stays in page points; the page↔screen transform
  is derived each frame from the displayed image rect, so it's correct at any zoom. **Crisp
  zoom:** each frame it computes a target render scale (`zoom × pixels_per_point`, quantized to
  0.5 steps, clamped 1–6) and, when it differs from the current page bitmap's scale, calls
  `Document::rerender_page` (→ `junk_libs_pdfium::render_page_bitmap`) so the raster matches the
  on-screen size instead of being upscaled. The texture rebuilds when the page or its render
  scale changes. Image sources can't re-render (raster), so they just display-scale. The
  re-render is synchronous on the UI thread (a brief hitch per zoom quantum at high res); a
  background-thread render would smooth that. The page lives in a `ScrollArea` whose content is
  laid out at `max(page, viewport)` with the page **centered** in it (egui would otherwise
  top-left-justify a page smaller than the view); the viewer **mirrors the scroll offset**
  (`scroll_offset`, read back from the `ScrollAreaOutput` each frame). **Input model:**
  **Ctrl+scroll / pinch** zooms (read via `i.zoom_delta()`, which egui keeps out of the scroll
  delta so the `ScrollArea` doesn't also pan) — **toward an anchor**: buttons and scroll share one
  `zoom_factor`, and the new offset `O' = f·O + (f−1)·(anchor − viewport.min)` is fed *into* the
  `ScrollArea` via `.scroll_offset()` (set before content layout, so it lands the **same frame** —
  no settle lag) to keep the anchor point fixed; the anchor is the cursor when over the view, else
  the viewport center (zoom buttons / keyboard). `O'` is **pre-clamped** to the scroll range
  `[0, max(0, page−viewport)]` per axis — an axis where the page fits pins to 0 (stays centered)
  instead of being positioned at the raw offset and snapped back a frame later. **Middle-button drag** pans (grab-scroll via
  `ui.scroll_with_delta`, gated by a `panning` flag so it never draws); the **primary button**
  draws/moves/resizes regions
  (the `_by(PointerButton::Primary)` response variants keep other buttons out of it). Drag start
  uses `pointer.press_origin()` (not `interact_pointer_pos`, which is only sampled after the drag
  threshold and offsets the start). A single hit-test, `hover_target` (corner handle → region
  body → nothing), backs **both** `decide_drag` and the **hover highlight** (brighter outline on
  the region/handle under the idle pointer, plus a resize/grab/crosshair cursor), so the
  highlight always matches what a press will do. **Live translation
  preview:** translated regions render their styled patch *over* the page via the **same**
  `export::render_region_patch` the exporter uses (not an egui-native re-implementation), so the
  preview is faithful to the output — including edited text, rotation, background override, and
  alignment. Patches are rasterized at the page-bitmap scale, uploaded as per-region textures
  cached by a `PatchKey` (text/rotate/bg/justify/size) and rebuilt only when that changes; the
  cache is pruned to live regions each frame, and a control-bar "Translations" toggle hides the
  overlay. **Deferred:** snapping region edges to `char_boxes`; lazy/bounded page-bitmap memory
  (all pages still eager-render at `DEFAULT_RENDER_ZOOM`, and visited pages keep their last render
  scale).
- **`translate.rs`** — one `Translator` trait, two impls:
  - `GeminiVision` (**default, implemented**): POSTs the region crop (PNG, base64) to Gemini's
    `generateContent` with a `responseSchema` that forces JSON, so we parse source text +
    translation + alternatives + explanation directly. No separate OCR; the only path that
    fully populates the info panel, and the default because Gemini's free tier includes vision.
  - `TesseractText` — the text-API path (`TextApi::{Google, DeepL, Kagi}`). Despite the name,
    it does **not** OCR: the source text is resolved upstream (PDF text layer, or OCR in the
    worker) and passed in via `req.source_text`; this translator just calls the text API with
    that string, so it works on digital PDFs with no `ocr` feature. **Google** is implemented
    (Cloud Translation API v2, API-key auth, source auto-detected); **DeepL/Kagi** are still
    stubs and are shown disabled ("coming soon") in the Settings provider picker. Text APIs
    return a bare string — `alternatives`/`explanation` stay empty; don't fabricate them.
- **`ocr.rs`** — local Tesseract OCR (`leptess`), behind the `ocr` feature. `recognize(image,
  lang)` returns text for regions with no PDF text layer (images/scanned PDFs); `AVAILABLE` is a
  compile-time flag the UI uses to say whether OCR will run. Errors are formatted via `Display`
  (not `?`) so the code doesn't depend on the leptess error types being `Send + Sync`. Source
  language is `Settings::source_lang` (a Tesseract code).
- **Translation runs on a background worker** in `app.rs` (`run_translation`, spawned per
  request; the call is blocking `reqwest`), result returned over an `mpsc` channel polled each
  frame — the UI never freezes. The worker resolves the source text (**text layer → OCR**),
  records the `SourceMethod` used, checks/populates the **shared** cache (`Arc<Mutex>`), then
  calls the translator. Doing the cache lookup *after* OCR is why the cache is shared with the
  worker rather than checked on the UI thread. The API key comes from `config::load_api_key`
  (`EXPAT_JUNK_API_KEY` env for now). The info panel shows the detected source text up front (a
  live `region_text` preview for text-layer regions) and a colored badge naming the method
  (PDF text layer / OCR / image-vision), so the user sees what's being sent. For a **translated**
  region the panel also exposes per-region presentation overrides (`Translated.style`, a
  `RegionStyle`): the source text and raw machine translation are shown read-only-but-copyable,
  while the rendered **text** is editable (defaults to the machine translation; `None` keeps
  tracking it, so re-translation updates it — and re-translating preserves the user's style),
  plus **rotate 90° (none / clockwise / counter-clockwise, CW default)**, **background color
  override**, **left/center/right justify**, **bold** (700 vs 400 weight), and an optional
  **font size** in points (`size_pt`; `None` auto-fits the box, `Some` sets the starting size,
  still shrunk to fit). These
  persist in the project (`results` is serialized; `#[serde(default)]` keeps old projects
  loadable) and feed both the live preview and the export.
- **`cache.rs`** — persistent translation cache so repeat translations are instant and spend no
  tokens. Two key namespaces (`sha2`-hashed, stored as JSON in the user's cache dir):
  `key_text` (provider, model, lang, **normalized source text**) is preferred — the same text
  hits regardless of how the box was drawn/sized/rendered; `key_pixels` (… **region pixels**)
  is the fallback when there's no source text at all. `Document::region_text` pulls text from
  `char_boxes` (chars whose box overlaps the region by ≥`CHAR_COVERAGE_THRESHOLD` — loose, since
  users draw imprecisely); OCR fills the same role for no-text-layer pages.
- **`export.rs`** — translated-PDF export, dispatched by source type in `export()`:
  - **PDF → vector-preserving** (`export_vector`): load the original with `lopdf`; for each
    translated region add an opaque `Image` XObject (background-matched fill + wrapped/auto-sized
    text via `ab_glyph` + the bundled CJK font) via `add_xobject` + `add_page_contents`, placed
    with a `cm`/`Do` and a top-left→bottom-left y-flip. **The original page's vector text/graphics
    stay untouched** — only the covered regions become raster. Assumes zero-origin `MediaBox`, no
    `/Rotate` (the common case).
  - **Image → full raster** (`export_raster`): bake cover boxes + text onto the page raster and
    embed each page via `printpdf` (the `images` feature; `from_dynamic_image` → `Op::UseXobject`
    at a page-filling DPI).
  Both build each region's patch through one shared `render_region_patch(w_px, h_px, bg, &Translated,
  regular, bold, px_per_pt)` (also called by `viewer.rs` for the live preview), which applies the
  region's `RegionStyle`: the displayed text (`Translated::display_text` — edited or machine),
  `justify` (per-line offset in `draw_text`), `rotate` (a `Rotate` of `None`/`Cw`/`Ccw` — lay out in a
  swapped-dim buffer then `rotate90` for clockwise / `rotate270` for counter-clockwise), `background`
  (override color, else `sample_background`'s flat fill), `bold` (picks the `bold` vs `regular` font),
  and `size_pt` (converted to buffer pixels via `px_per_pt` as `draw_text`'s starting size — `None`
  starts at the region height; either way `draw_text` shrinks to fit). The two fonts are
  weight-pinned instances of the one bundled variable font (`fonts::weighted(bold)`). Both are verified headless:
  valid PDF + upright orientation (raster), and overlay-placed-upright + valid + page-count-preserved
  (vector); a unit test (`patch_honors_style_overrides`) covers the style application. Background
  match is a flat fill (seams on texture).
- **`project.rs`** — save/resume. A `.expj` JSON file (`Project`) references the **source by
  path** (not the rendered pages — re-derived on load) and stores regions, the id counter,
  translations (`results`), page, and the source/target lang + backend. `project::open_document`
  reopens the source and re-applies the regions. `app.rs` tracks `source_path`/`project_path`/
  `dirty`; the window title reflects the file + a `*` when dirty.
- **`config.rs`** — non-secret `Settings` (`load`/`save` as JSON under the platform config dir
  via `directories`). `source_lang` (OCR, Tesseract code) and `target_lang` (ISO code) are picked
  from dropdowns in the right panel (backed by the curated `LANGUAGES` table, which pairs each
  language's `ocr`/`iso` codes) and persisted on change. `config::load_api_key(secrets, backend)` resolves the
  key: `EXPAT_JUNK_API_KEY` env overrides, else the secrets store. `Backend::secret_id()` names
  the credential per provider (`gemini`/`google`/`deepl`/`kagi`).
- **`secrets.rs`** — API keys in `secrets.json` (config dir, owner-only on Unix), **separate**
  from `settings.json` and out of git. Edited via the Settings window (`app::settings_window`),
  keyed by provider. Not the OS keychain yet (`keyring` is still a dep but unused) — that's the
  intended hardening.
- **`fonts.rs`** — installs a bundled Noto Sans JP (`assets/fonts/`, OFL) as the last fallback
  for both font families, so Japanese/CJK source and translated text render instead of tofu.
  Called once from `App::new` via `cc.egui_ctx`. The bundled TTF is the **variable** font (weight
  100–900) whose default instance is Thin; `weighted(bold)` returns an `ab_glyph::FontRef` with the
  `wght` axis pinned to 400/700 for the export/preview rasterizer, so translated text isn't spindly
  and bold is a real heavier weight (no second font file). egui's UI fallback still uses the Thin
  default — only the rasterized translation text is weight-pinned.

## Native dependency gotchas

These are the surprises to watch for — the rest is pure Rust.

- **PDFium**: bound dynamically at runtime by `junk-libs-pdfium`, so its Cargo feature
  (`pdfium_7763`) **must match the vendored binary** or you get `LoadLibraryError: undefined
  symbol FPDFText_*`. That crate's `build.rs` keeps them in lockstep (downloads `chromium/7763`).
  To move versions, change BOTH the `pdfium_NNNN` feature and `PDFIUM_VERSION` *in that crate*,
  to a number that exists as a tagged
  [bblanchon release](https://github.com/bblanchon/pdfium-binaries/releases). Never use the
  `pdfium_latest` (moving) or `pdfium_future` (needs an unreleased binary) features. This repo
  no longer carries a `build.rs` of its own.
- **Tesseract** (`leptess`, `ocr` feature): links the system Tesseract + Leptonica C
  libraries at **build** time (via `pkg-config` for `lept`/`tesseract`), so the default
  build leaves it off. To build `--features ocr` on Arch/CachyOS:
  `sudo pacman -S leptonica tesseract tesseract-data-eng` (add `tesseract-data-<lang>` for
  each OCR source language; `clang` is also needed for bindgen). Without the feature, `ocr.rs`
  still compiles — `recognize()` just reports unavailability and regions fall back to the
  vision/pixel path.
- **keyring v4** needs a platform backend feature enabled to actually persist secrets
  (e.g. secret-service on Linux). Until that's wired, `config::load_api_key` falls back to
  the `EXPAT_JUNK_API_KEY` env var — fine for local dev, not the real story.

## Provider notes (for whoever implements `translate.rs`)

- **Gemini Flash-Lite** is the cheap default: vision is in the free tier (~1,500 req/day),
  images ≈258 tokens each; paid is ~$0.10/$0.40 per 1M tokens. Use the `generateContent`
  endpoint with the crop as inline image data and a structured-output prompt.
- **Google Cloud Translation** and **DeepL Free** each give 500k chars/month free. DeepL's
  *free* tier may train on submitted text; Google's does not.
- **Kagi Translate** has a public API (~$15/M chars), worth supporting for existing Kagi
  users.
