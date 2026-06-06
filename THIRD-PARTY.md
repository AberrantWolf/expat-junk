# Third-party notices

expat-junk bundles and depends on third-party components with their own
licenses. This file lists the ones that ship inside this repository or in the
built binary; the licenses of crate dependencies are recorded in their
respective packages (see `Cargo.lock`).

## 📦 Bundled assets

### Noto Sans JP — `assets/fonts/NotoSansJP.ttf`

Bundled as the CJK fallback font for rendering Japanese (and other CJK) source
and translated text. Licensed under the **SIL Open Font License, Version 1.1**.
The full license text ships alongside the font at
[`assets/fonts/OFL.txt`](assets/fonts/OFL.txt).

## 🔧 Runtime native component

### PDFium

PDF rendering uses **PDFium**, downloaded at build time by the
`junk-libs-pdfium` crate (a prebuilt binary from the
[bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries)
project). PDFium is licensed under the **BSD-3-Clause** license and is **not**
checked into this repository — it is fetched into the build directory on the
first build.
