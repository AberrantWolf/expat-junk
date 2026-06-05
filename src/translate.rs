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

use anyhow::{Context, Result};
use base64::Engine as _;
use image::ImageEncoder as _;
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
    fn translate(&self, req: &TranslationRequest) -> Result<TranslationResult> {
        let png = encode_png(req.image)?;
        let image_b64 = base64::engine::general_purpose::STANDARD.encode(&png);

        let mut prompt = format!(
            "The image is a cropped region of a document. Read the text in it and \
             translate that text into {lang}. Return the detected source text, the \
             translation, up to three alternative translations, and a short note on \
             any nuance or ambiguity.",
            lang = req.target_lang,
        );
        if let Some(src) = req.source_text {
            prompt.push_str(&format!(
                "\n\nThe source text was also extracted as: {src:?}. Prefer it if it \
                 matches the image."
            ));
        }

        // Force structured JSON so we don't have to scrape prose.
        let body = serde_json::json!({
            "contents": [{
                "parts": [
                    { "text": prompt },
                    { "inlineData": { "mimeType": "image/png", "data": image_b64 } }
                ]
            }],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseSchema": {
                    "type": "object",
                    "properties": {
                        "source_text": { "type": "string" },
                        "translation": { "type": "string" },
                        "alternatives": { "type": "array", "items": { "type": "string" } },
                        "explanation": { "type": "string" }
                    },
                    "required": ["source_text", "translation"]
                }
            }
        });

        // Gemini takes the API key as a query parameter; keys are URL-safe.
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .context("calling the Gemini API")?;

        let status = resp.status();
        let value: serde_json::Value = resp.json().context("reading the Gemini response")?;
        if !status.is_success() {
            let msg = value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            anyhow::bail!("Gemini API error ({status}): {msg}");
        }

        let text = value
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(serde_json::Value::as_str)
            .context("Gemini response contained no text (blocked or empty?)")?;
        let payload: GeminiPayload = serde_json::from_str(text)
            .with_context(|| format!("parsing structured translation from: {text}"))?;

        Ok(TranslationResult {
            source_text: payload.source_text,
            translation: payload.translation,
            alternatives: payload.alternatives.unwrap_or_default(),
            explanation: payload.explanation,
        })
    }
}

/// The structured payload we ask Gemini to return (matches `responseSchema`).
#[derive(Deserialize)]
struct GeminiPayload {
    source_text: String,
    translation: String,
    alternatives: Option<Vec<String>>,
    explanation: Option<String>,
}

/// Encode an RGBA image as PNG bytes for upload.
fn encode_png(img: &image::RgbaImage) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgba8,
        )
        .context("encoding region as PNG")?;
    Ok(buf)
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
