//! Translation backends behind one [`Translator`] trait, so the UI never knows
//! which provider produced a result.
//!
//! Two paths ship from day one:
//!   - [`GeminiVision`]: send the region's image crop to Gemini Flash-Lite and
//!     get source text + translation + alternatives + explanation in one call.
//!     This is the only path that fully populates the info panel; it's also the
//!     default because Gemini's free tier includes vision.
//!   - [`TesseractText`]: OCR the crop locally (Tesseract, `ocr` feature) and
//!     send the text to a classic API (Google / DeepL / Kagi). Cheap and
//!     familiar, but returns just a string -- no alternatives/explanations.
//!
//! Users supply their own API keys (see [`crate::config`]).

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// What the UI asks a backend to translate.
pub struct TranslationRequest<'a> {
    /// Image crop of the region (always available).
    pub image: &'a image::RgbaImage,
    /// Pre-extracted source text, if the region came from a PDF text layer.
    pub source_text: Option<&'a str>,
    pub target_lang: &'a str,
}

/// What a backend returns. `alternatives`/`explanation` are best-effort: the
/// text-API path leaves them empty, the vision path fills them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslationResult {
    pub source_text: String,
    pub translation: String,
    pub alternatives: Vec<String>,
    pub explanation: Option<String>,
}

/// A configured translation backend.
pub trait Translator {
    fn translate(&self, req: &TranslationRequest) -> Result<TranslationResult>;
}

/// Default backend: image crop -> Gemini Flash-Lite -> structured result.
pub struct GeminiVision {
    pub api_key: String,
    pub model: String,
}

impl Translator for GeminiVision {
    fn translate(&self, _req: &TranslationRequest) -> Result<TranslationResult> {
        // TODO: reqwest (blocking) POST to Gemini generateContent with the crop
        // as inline image data + a structured-output prompt; parse JSON.
        anyhow::bail!("GeminiVision::translate not yet implemented")
    }
}

/// Classic text APIs the OCR path can target.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TextApi {
    Google,
    DeepL,
    Kagi,
}

/// OCR-then-translate backend. The OCR step requires the `ocr` feature.
pub struct TesseractText {
    pub api: TextApi,
    pub api_key: String,
}

impl Translator for TesseractText {
    fn translate(&self, _req: &TranslationRequest) -> Result<TranslationResult> {
        // TODO: leptess OCR on the crop (or use req.source_text when present),
        // then call the selected TextApi. alternatives/explanation stay empty.
        anyhow::bail!("TesseractText::translate not yet implemented")
    }
}
