//! Local OCR via Tesseract, behind the `ocr` feature.
//!
//! Used to recover source text from regions that have no PDF text layer (images
//! and scanned PDFs), so they can join the text-keyed cache and give the
//! translator a `source_text` hint. `leptess` links the system Tesseract +
//! Leptonica C libraries at build time, so this whole path is off by default;
//! build with `--features ocr` (and have the relevant tessdata installed).
//!
//! When the feature is off, [`recognize`] still exists but reports unavailability,
//! so callers can treat OCR as a best-effort step without `cfg` at every site.

/// Recognize text in `image` for Tesseract language code `lang` (e.g. `eng`).
#[cfg(feature = "ocr")]
pub fn recognize(image: &image::RgbaImage, lang: &str) -> anyhow::Result<String> {
    use image::ImageEncoder as _;

    // leptonica reads encoded images from memory; PNG is lossless and cheap.
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| anyhow::anyhow!("encoding region for OCR: {e}"))?;

    // Errors are formatted via Display rather than `?` so we don't depend on the
    // leptess error types being Send + Sync.
    let mut tess = leptess::LepTess::new(None, lang).map_err(|e| {
        anyhow::anyhow!("starting Tesseract for '{lang}' (is its tessdata installed?): {e}")
    })?;
    tess.set_image_from_mem(&png)
        .map_err(|e| anyhow::anyhow!("loading region into Tesseract: {e}"))?;
    let text = tess
        .get_utf8_text()
        .map_err(|e| anyhow::anyhow!("running OCR: {e}"))?;
    Ok(text)
}

#[cfg(not(feature = "ocr"))]
pub fn recognize(_image: &image::RgbaImage, _lang: &str) -> anyhow::Result<String> {
    anyhow::bail!("OCR is not built in; rebuild with `--features ocr`")
}

/// Whether this build can OCR (the `ocr` feature is enabled). Lets the UI tell
/// the user what will happen to a region with no text layer.
pub const AVAILABLE: bool = cfg!(feature = "ocr");
