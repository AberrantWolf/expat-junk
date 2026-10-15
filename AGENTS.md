# Repository guidance

Expat Junk is an egui desktop app for translating selected PDF/image regions,
editing the translations, saving `.expj` projects, and exporting PDF overlays.

## Constraints

- Keep egui UI in `app.rs`, `viewer.rs`, and font installation. Backend code
  must remain usable without a display. Translation data belongs in a backend
  module, not in the UI shell.
- Reuse the shared `junk-libs` document/viewer/rendering crates. Their git
  revisions are pinned by `Cargo.lock`; never commit sibling path dependencies.
  The commented Cargo patch is available for local co-development.
- Region geometry uses page points with a top-left origin. Account for PDF
  CropBox/MediaBox origins, inherited page properties, and rotation at export.
- Preview and export must use the same translation patch renderer. Preserve
  original PDF vectors outside overlays.
- Run network calls and expensive document work off the UI thread. Reject stale
  worker results after a region or session changes.
- Maintain compatibility with existing `.expj` files using serde defaults for
  new fields. Save user work atomically and protect unsaved work on open/close.
- Cache keys must cover all translation inputs, including image inputs for
  vision providers and the prompt version. Re-translate must bypass the cache.
- Never put API keys in URLs, logs, projects, or committed files. Credentials
  are separate from settings; report persistence failures to the user.

## Validation

`cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, and
`cargo test --locked` match CI. Add targeted regressions for behavior changes.
The optional `ocr` feature needs system Tesseract/Leptonica and language data;
the default build needs no native PDF renderer.

Use `README.md` for user documentation and source/dependency code for current
implementation details. Check provider documentation before changing APIs or
model defaults; avoid embedding volatile pricing claims here.
