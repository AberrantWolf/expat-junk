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

PDF rendering is pure Rust ([hayro](https://github.com/LaurenzV/hayro) via the
`junk-libs-platen` crate, MIT/Apache-2.0) — an ordinary crate dependency like
the rest, with no separately-licensed native component.

### Multilingual Noto fallback fonts

The shared `junk-libs-text` renderer bundles Noto Sans, Noto Sans Arabic,
Noto Sans Hebrew, Noto Sans Devanagari and Noto Sans Thai. Expat Junk additionally
bundles Noto Sans KR for Hangul coverage and retains Noto Sans JP for Japanese
and Chinese. All are licensed under SIL Open Font License 1.1. The exact notices
are copied into `assets/fonts/NotoSans*-OFL.txt` and included in release bundles.
The fonts originate from the official [Google Fonts repository](https://github.com/google/fonts).

Unicode shaping and rasterization use [COSMIC Text](https://github.com/pop-os/cosmic-text)
(MIT OR Apache-2.0), with HarfRust, Unicode bidi/line-breaking and Swash through
that crate. No installed system fonts or native shaping libraries are required.
