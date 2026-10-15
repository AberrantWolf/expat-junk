# Expat Junk 🗺️

**Do PDFs and image translations on your own.**

Look, I got tired of having to file translated documents where my only options
were to pay someone potentially hundreds of dollars, or let some automated
website handle it for free (and give me back usually-unusable word salad).

So I took some of that money and used it instead to vibe code a tool that lets
me manually translate a document in-place. I don't have to copy/paste into a
new document or play with mimicking the layout. I just drag a rectangle over a
chunk of text, the system finds the text, and I click a button to translate
that. I can fix problems, resize the fonts, rotate them... like it's just the
right amount of work I'm willing to put in without starting to consider paying
way more than it's worth for "professional" tools or translation.

> 🚧 **Status: early development.**
> Working today: open PDFs/images, draw / zoom / page through and edit
> translation regions, translate via **Gemini (vision)** or **Google Cloud
> Translation** with a persistent token-saving cache, save and resume your work
> as a project file, and export a translated PDF. DeepL is also supported; Kagi
> is planned.

---

## ✨ What it does

- 🖱️ **Region-by-region selection** — draw, move, resize, and delete boxes
  directly on the page. Everything lives in page coordinates, so it stays
  correct at any zoom.
- 🌐 **Bring-your-own-key translation** — Gemini vision (the default, returns
  alternatives + notes), Google Cloud Translation v2, or DeepL API Free/Pro.
- 🔍 **Smart source-text detection** — reconstruct digital PDF text by spatial
  lines and columns, including RTL and vertical Japanese, with a reading-order
  override for ambiguous pages. Images and scans use vision or optional OCR.
- 📚 **Lazy PDF pages** — opening reads page metadata, then renders pages on
  demand with a cache limited to three pages and 64 MiB of bitmap/text data.
  Source bytes, transient render workers, fonts and GPU textures use additional
  memory. Off-page translation textures are released.
- 💾 **Token-saving cache** — repeat translations of the same text are instant
  and free, served from a local cache.
- 📐 **Live, faithful preview** — translated regions render on the page exactly
  as they'll export, including rotation, alignment, bold, font size, and
  background color.
- 📄 **Vector-preserving export** — for PDF sources, the original text and
  graphics stay sharp and selectable; only the translated regions become an
  overlay.
- 🗂️ **Save & resume** — your regions and translations persist in a small
  project file.

---

## 🧰 Prerequisites

- A recent **stable Rust toolchain** (install via [`rustup`](https://rustup.rs)).
- 🧱 The shared [`junk-libs`](https://github.com/AberrantWolf/junk-libs) crates
  (including `junk-libs-platen`, the pure-Rust PDF renderer) are ordinary git
  dependencies fetched by Cargo — no sibling checkout or binary downloads
  needed. For co-developing against a local `../junk-libs` working copy,
  uncomment the `[patch]` block at the bottom of `Cargo.toml`.

---

## 🚀 Build & run

```bash
cargo run            # build and launch the app
cargo build          # build only
cargo test           # run the test suite (incl. a headless PDF render test)
```

---

## 🔑 Translating: your API key

The default translator is **Google Gemini** (vision), using the stable
`gemini-3.5-flash-lite` model for new settings. Existing model choices are
preserved. You can edit the model under Settings. Gemini returns alternatives
and notes. **Google Cloud Translation**
(v2) is also supported: pick it under **Settings → Provider**. (It needs the
Cloud Translation API enabled on your project plus an API key. Because it
translates *text*, it works on digital PDFs directly; image/scanned pages need
the `ocr` feature to extract text first.) DeepL API Free and Pro are also supported (Free keys end in `:fx`). Kagi is not
implemented yet.

**Add your key in the app** via **Settings → API key**. It's stored locally in
`secrets.json` in your config dir (owner-only, never committed to git), and each
provider gets its own key slot.

**Or use an environment variable**, which overrides the stored key — handy for
one-off runs:

```fish
set -x EXPAT_JUNK_API_KEY <your-gemini-api-key>   # fish
```
```bash
export EXPAT_JUNK_API_KEY=<your-gemini-api-key>   # bash/zsh
```

Then open a document, drag a box over some text, and click **Translate
selection** — or draw several regions and use **Edit → Translate all** to do
them in one go. Batch translations reuse a local cache in the platform cache directory
(`~/.cache/expat-junk/translations.json` on Linux). Text providers reuse exact
source text; vision keys also include the image. Translation crops are rendered
at a fixed source scale, so viewer zoom does not change their cache keys. Keys cover the model, languages,
document context, and prompt version. **Re-translate** requests a fresh API
result and preserves your presentation edits; it can incur provider charges.
Bundled Noto fonts cover Japanese, Chinese, Korean, Arabic, Hebrew, Hindi, Thai,
Vietnamese, Cyrillic and Latin. Preview and export share Unicode shaping, bidi
layout, combining-mark positioning, font fallback and line breaking, without
requiring installed fonts.

Use **Document context / terminology** in the right panel to supply the document
type, tone, or terms to preserve. Gemini and DeepL use this context; Google Cloud
Translation v2 does not. Context is saved in the project. **PDF reading order** can be set to Automatic,
Left to right, Right to left, or Vertical Japanese. Reconstruction of untagged
PDFs is heuristic; review extracted text for unusual layouts and complex tables.
Translations made with
previous settings are marked in the panel and included in **Translate all**.

---

## 💾 Saving & exporting

- 📁 **Save / resume** — **File → Save project** writes a `.expj` file (your
  regions and translations; it references the source document *by path* rather
  than embedding it). **File → Open project** restores a session. The title bar
  shows a `*` when there are unsaved changes. Opening another file or closing
  the app prompts before discarding unsaved work. Saves replace the project
  atomically. New saves record a source fingerprint, so replacing the source
  document is detected on resume and export.
  - Shortcuts: `Ctrl/Cmd+S` save · `Ctrl/Cmd+O` open document · `Ctrl/Cmd+E`
    export.
- 📤 **Export** — **File → Export translated…** writes a PDF where each
  translated region is covered with a background-matched box and the translation
  is drawn on top (CJK included). For **PDF sources the original vector content
  is preserved** — untranslated text stays sharp and selectable, and only the
  translated regions are overlaid, including on cropped, offset, and rotated
  PDF pages. Export renders one source page at a time on a background worker,
  using a snapshot of regions and translations. Image sources export as a full-page raster. Exporting over the source
  document is refused. Nonstandard PDF `UserUnit` values are currently unsupported.
  *(Background matching is a flat fill, so textured or photo backgrounds may show
  a faint seam.)*

---

## 🔡 Optional: local OCR (Tesseract)

Digital PDFs carry a text layer, so expat-junk can read the source text directly.
For **images and scanned PDFs** (no text layer), you can enable local Tesseract
OCR to recover the source text:

```bash
cargo build --features ocr
cargo run --features ocr
```

This links the system Tesseract + Leptonica libraries at build time, so install
them first. On **Arch / CachyOS**:

```bash
sudo pacman -S leptonica tesseract tesseract-data-eng clang
```

Add a `tesseract-data-<lang>` package for each OCR **source** language you need
(e.g. `tesseract-data-jpn`, `tesseract-data-deu`), and set the source language
in your settings (a Tesseract code, e.g. `jpn`). On other distributions, install
the equivalent `leptonica`, `tesseract` (with dev headers), the language data,
and `clang` (for bindgen).

> Without the `ocr` feature the app still builds and runs fine — no-text-layer
> regions just send the image straight to the vision model.

---

## ⚠️ Caution

This system uses machine translation, which CAN make mistakes. Translation is
often contextual in weird and surprising ways. I don't recommend blindly using
this tool to translate from/into languages you're not familiar with. While I
would like to add a feature to supply machine translation tools with more
context about the file, which SHOULD help get better, more natural translations,
it can still make mistakes.

Heaven help you if you're translating an official document and the auto result
is wrong in the worst way. Google sure isn't going to take responsibility for
that, and neither am I.

PLEASE seek help from native-speaking friends or professionals if you're not
100% confident that you COULD do this on your own given enough time. This tool
is ONLY to make it easier, not to enable folks to do something they're not
qualified for.

Put differently, your translations are on you, and nobody else. So. Be smart.

---

## 📜 License

Licensed under either of:

- **MIT license** ([LICENSE-MIT](LICENSE-MIT))
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE))

at your option. Unless you explicitly state otherwise, any contribution you
submit for inclusion shall be dual-licensed as above, without any additional
terms or conditions.

🔤 **Third-party components** (the bundled Noto Sans JP font) carry their own
licenses — see [THIRD-PARTY.md](THIRD-PARTY.md).
