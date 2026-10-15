//! Persistent translation cache.
//!
//! Keys cover provider, model, prompt version, language, exact source text,
//! and (for vision) the image. Whitespace is translation input, not discarded.

use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::translate::TranslationResult;

pub struct TranslationCache {
    entries: HashMap<String, TranslationResult>,
    /// Where to persist; `None` if no cache dir could be resolved (in-memory only).
    path: Option<PathBuf>,
}

impl TranslationCache {
    /// Load the cache from the user's cache dir, or start empty if absent.
    pub fn load() -> Self {
        let path = directories::ProjectDirs::from("", "", "expat-junk")
            .map(|d| d.cache_dir().join("translations.json"));
        Self::with_path(path)
    }

    /// Construct from an explicit path (or `None` for in-memory only).
    pub(crate) fn with_path(path: Option<PathBuf>) -> Self {
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { entries, path }
    }

    pub fn get(&self, key: &str) -> Option<TranslationResult> {
        self.entries.get(key).cloned()
    }

    /// Store a result and persist to disk (best-effort).
    pub fn insert(&mut self, key: String, result: TranslationResult) {
        self.entries.insert(key, result);
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec(&self.entries) {
            let _ = crate::persistence::write(path, &json);
        }
    }
}

/// Key every input actually sent to the backend. Vision requests always include
/// pixels, even when a text layer or OCR hint is available.
pub fn key_request(
    provider: &str,
    model: &str,
    lang: &str,
    source_lang: &str,
    context: &str,
    text: Option<&str>,
    image: Option<&image::RgbaImage>,
) -> String {
    make_key(
        provider,
        model,
        lang,
        if image.is_some() { 'p' } else { 't' },
        |h| {
            for part in [
                crate::translate::PROMPT_VERSION,
                source_lang,
                context,
                text.unwrap_or(""),
            ] {
                h.update((part.len() as u64).to_le_bytes());
                h.update(part.as_bytes());
            }
            if let Some(image) = image {
                h.update(image.width().to_le_bytes());
                h.update(image.height().to_le_bytes());
                h.update(image.as_raw());
            }
        },
    )
}

/// Hash the (provider, model, lang) preamble plus a `kind`-tagged payload. The
/// `kind` ('t'/'p') namespaces text vs pixel keys so they can never collide.
fn make_key(
    provider: &str,
    model: &str,
    lang: &str,
    kind: char,
    payload: impl FnOnce(&mut Sha256),
) -> String {
    let mut hasher = Sha256::new();
    for part in [provider.as_bytes(), model.as_bytes(), lang.as_bytes()] {
        hasher.update(part);
        hasher.update([0u8]); // domain separator
    }
    hasher.update([kind as u8, 0]);
    payload(&mut hasher);
    let hash = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
    format!("{provider}/{lang}/{kind}/{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TranslationResult {
        TranslationResult {
            source_text: "こんにちは".to_string(),
            translation: "hello".to_string(),
            alternatives: vec!["hi".to_string()],
            explanation: None,
        }
    }

    #[test]
    fn request_keys_cover_text_pixels_and_language() {
        let a = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        let b = image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 2, 3, 255]));
        let key =
            |text, image, source| key_request("gemini", "m", "en", source, "", Some(text), image);
        assert_eq!(key("hello", Some(&a), "eng"), key("hello", Some(&a), "eng"));
        assert_ne!(key("hello", Some(&a), "eng"), key("hello", Some(&b), "eng"));
        assert_ne!(key("hello", Some(&a), "eng"), key("hello", Some(&a), "jpn"));
        assert_ne!(key("a b", None, "eng"), key("a\nb", None, "eng"));
        assert_ne!(key("hello", None, "eng"), key("hello", Some(&a), "eng"));
        assert_ne!(
            key("hello", None, "eng"),
            key_request("gemini", "m", "en", "eng", "form", Some("hello"), None)
        );
        assert_ne!(
            key("hello", None, "eng"),
            key_request("gemini", "n", "en", "eng", "", Some("hello"), None)
        );
        assert_ne!(
            key("hello", None, "eng"),
            key_request("gemini", "m", "ja", "eng", "", Some("hello"), None)
        );
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("translations.json");

        let mut cache = TranslationCache::with_path(Some(path.clone()));
        assert!(cache.get("k").is_none());
        cache.insert("k".to_string(), sample());

        // A fresh instance reads the persisted entry back.
        let reloaded = TranslationCache::with_path(Some(path.clone()));
        assert_eq!(reloaded.get("k").unwrap().translation, "hello");
    }
}
