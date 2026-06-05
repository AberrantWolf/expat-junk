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
  `junk-libs-egui-pdfview` widget. `app.rs` has a **menu bar** (File/Edit/Settings) over a
  toolbar; menu/shortcut clicks are collected into an `Action` enum and executed *after* the
  panels render — this sidesteps borrow conflicts between the menu closures and the field-level
  `&mut` borrows the panels need.
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
  top-left page space. `quantize_zoom()` is there for the future render cache. To change PDFium
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
  is derived each frame from the displayed image rect, so it's correct at any zoom. Zoom is
  **display scaling** of the base-resolution texture (Ctrl+scroll or the buttons) — it changes
  region-selection precision but not raster crispness; true **re-render at zoom×DPI is still
  deferred** (pages render once at `DEFAULT_RENDER_ZOOM`). Drag start uses `pointer.press_origin()`
  (not `interact_pointer_pos`, which is only sampled after the drag threshold and offsets the
  start). **Deferred:** crisp high-zoom re-render, and snapping region edges to `char_boxes`.
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
  (PDF text layer / OCR / image-vision), so the user sees what's being sent.
- **`cache.rs`** — persistent translation cache so repeat translations are instant and spend no
  tokens. Two key namespaces (`sha2`-hashed, stored as JSON in the user's cache dir):
  `key_text` (provider, model, lang, **normalized source text**) is preferred — the same text
  hits regardless of how the box was drawn/sized/rendered; `key_pixels` (… **region pixels**)
  is the fallback when there's no source text at all. `Document::region_text` pulls text from
  `char_boxes` (chars whose box overlaps the region by ≥`CHAR_COVERAGE_THRESHOLD` — loose, since
  users draw imprecisely); OCR fills the same role for no-text-layer pages.
- **`export.rs`** — **overlay on the rendered raster → PDF** (implemented). For each translated
  region: sample the background (1px ring around it) → fill a flat cover rectangle → draw the
  translation wrapped and auto-sized with `ab_glyph` + the bundled CJK font (so Japanese exports
  correctly). Each annotated page raster is embedded full-size into a `printpdf` page (the
  `images` feature; `from_dynamic_image` from raw RGBA → `Op::UseXobject` at a DPI that fills the
  page). One multi-page PDF out. Trade-offs: everything becomes raster (vector text not retained),
  flat fill seams on textured backgrounds — accepted per "no loss if we can't". **Verify export
  orientation** the first time (PDF image Y-axis); flip the image before embedding if upside down.
  `lopdf` is still a dep but unused here (a future vector-overlay path could keep original text).
- **`project.rs`** — save/resume. A `.expj` JSON file (`Project`) references the **source by
  path** (not the rendered pages — re-derived on load) and stores regions, the id counter,
  translations (`results`), page, and the source/target lang + backend. `project::open_document`
  reopens the source and re-applies the regions. `app.rs` tracks `source_path`/`project_path`/
  `dirty`; the window title reflects the file + a `*` when dirty.
- **`config.rs`** — non-secret `Settings` (`load`/`save` as JSON under the platform config dir
  via `directories`). `source_lang` (OCR, Tesseract code) and `target_lang` are editable inline
  in the toolbar and persisted on change. `config::load_api_key(secrets, backend)` resolves the
  key: `EXPAT_JUNK_API_KEY` env overrides, else the secrets store. `Backend::secret_id()` names
  the credential per provider (`gemini`/`google`/`deepl`/`kagi`).
- **`secrets.rs`** — API keys in `secrets.json` (config dir, owner-only on Unix), **separate**
  from `settings.json` and out of git. Edited via the Settings window (`app::settings_window`),
  keyed by provider. Not the OS keychain yet (`keyring` is still a dep but unused) — that's the
  intended hardening.
- **`fonts.rs`** — installs a bundled Noto Sans JP (`assets/fonts/`, OFL) as the last fallback
  for both font families, so Japanese/CJK source and translated text render instead of tofu.
  Called once from `App::new` via `cc.egui_ctx`.

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
