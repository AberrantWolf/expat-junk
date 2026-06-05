# expat-junk

A desktop tool for translating PDFs and images **region by region**. Open a
document, draw boxes over blocks of text, and each box is translated by a
provider you configure with your own API key. Clicking a translated region shows
the source text, the translation, and (provider permitting) alternatives and
notes — with a badge telling you how the source text was obtained.

> Status: early development. The render + region-editing surface and the
> Gemini vision translation path (with a persistent, token-saving cache) work
> today. Classic text APIs, baking translations back into an output document,
> and zoom are still in progress.

## Prerequisites

- A recent stable Rust toolchain (`rustup`).
- **The [`junk-libs`](https://github.com/AberrantWolf/junk-libs) repo checked out
  as a sibling directory** (`../junk-libs`). expat-junk depends on its
  `junk-libs-pdfium` crate for PDF rendering via a path dependency:

  ```
  programming/
  ├── expat-junk/
  └── junk-libs/
  ```

- Network access on the **first** build: `junk-libs-pdfium`'s build script
  downloads a prebuilt PDFium binary into the build directory (once per target
  dir, then offline).

## Build & run

```bash
cargo run            # build and launch
cargo build          # build only
cargo test           # run the test suite
```

## Translating: API key

The default translator is **Google Gemini** (vision) — its free tier includes
image input, and it returns alternatives and notes. **Google Cloud Translation**
(v2) is also supported: pick it under **Settings → Provider** (it needs the Cloud
Translation API enabled on your project + an API key). Google translates text, so
it works on digital PDFs directly; image/scanned pages need the `ocr` feature to
extract text first. DeepL and Kagi are not implemented yet.

Add your own API key in the app via **Settings → API key** (stored locally in
`secrets.json` in your config dir, owner-only, never in git). Each provider has
its own key slot.

Alternatively, set an environment variable, which overrides the stored key
(handy for one-off runs):

```fish
set -x EXPAT_JUNK_API_KEY <your-gemini-api-key>   # fish
```
```bash
export EXPAT_JUNK_API_KEY=<your-gemini-api-key>   # bash/zsh
```

Then open a document, drag a box over some text, and click **Translate
selection**. Repeat translations of the same text are served from a local cache
(`~/.cache/expat-junk/translations.json`) — instant and free. Japanese and other
CJK text renders via a bundled Noto Sans JP font.

## Optional: local OCR (Tesseract)

Digital PDFs carry a text layer, so expat-junk reads the source text directly —
no OCR needed. For **images and scanned PDFs** (no text layer), enable local
Tesseract OCR to recover the source text:

```bash
cargo build --features ocr
cargo run --features ocr
```

This links the system Tesseract + Leptonica libraries at build time, so install
them first. On Arch / CachyOS:

```bash
sudo pacman -S leptonica tesseract tesseract-data-eng clang
```

Add a `tesseract-data-<lang>` package for each OCR **source** language you need
(e.g. `tesseract-data-jpn`, `tesseract-data-deu`), and set the source language
in your settings (Tesseract code, e.g. `jpn`). On other distributions install
the equivalent `leptonica`, `tesseract` (with dev headers), the language data,
and `clang` (for bindgen).

Without the `ocr` feature the app still builds and runs; no-text-layer regions
just send the image straight to the vision model.

## License

TBD.
