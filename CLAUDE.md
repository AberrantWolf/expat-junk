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
cargo build --features ocr   # also build the Tesseract OCR path (needs native libs)
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
  `junk-libs-egui-pdfview` widget.
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
- **`viewer.rs`** — the interactive page canvas. Renders the current page to an egui texture
  and lets the user draw / select / move / resize / delete regions over it. All geometry stays
  in page points; the page↔screen transform is recomputed each frame from the displayed image
  rect, so it stays correct when zoom lands. **Deferred:** real zoom + re-render at
  zoom×DPI (pages currently render once at a fixed scale, displayed near native size — crisp
  but not yet zoomable), and snapping region edges to `char_boxes`.
- **`translate.rs`** — one `Translator` trait, two shipped impls:
  - `GeminiVision` (**default**): sends the region's image crop to Gemini Flash-Lite and
    gets source text + translation + alternatives + explanation in one structured call.
    No separate OCR. This is the only path that fully populates the info panel, and it's
    the default because Gemini's free tier includes vision.
  - `TesseractText`: OCR the crop locally (Tesseract, behind the `ocr` feature) and send
    text to a classic API (`TextApi::{Google, DeepL, Kagi}`). Cheap/familiar, but returns
    a bare string — `alternatives`/`explanation` stay empty. Honor that in the UI; don't
    fabricate them.
- **`export.rs`** — **overlay on original, not reflow.** For each region: sample the
  background color around the source block, paint a solid cover rectangle, overprint the
  translated text. Layout is preserved; solid fill leaves visible seams on photo/textured
  backgrounds, which is an accepted loss ("no loss if we can't"). PDFs via
  `lopdf` + `printpdf`; images re-rastered via the `image` crate. Don't reach for full
  page regeneration/reflow — it was explicitly deferred as an open-ended fidelity problem.
- **`config.rs`** — non-secret `Settings` as JSON under the platform config dir
  (`directories`); API keys via the OS keychain (`keyring`), never in the settings file or
  git. Users always supply their own keys.

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
  libraries at **build** time, so the default build leaves it off. Enable with
  `--features ocr` only when those libs are installed.
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
