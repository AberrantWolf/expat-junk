//! Persistent translation cache.
//!
//! Re-translating the same region should be free: results are keyed by a stable
//! hash of (provider, model, target language, region pixels) and persisted under
//! the user's cache dir, so repeats are instant and don't spend API tokens. The
//! key hashes the actual bytes sent to the provider — identical crops hit;
//! visually different crops (different size/position) miss. (A future text-level
//! cache keyed on detected source text could catch more.)

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
    fn with_path(path: Option<PathBuf>) -> Self {
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
            let _ = std::fs::write(path, json);
        }
    }
}

/// Cache key from a region's **source text** — the preferred key: the same text
/// hits regardless of how the region was drawn, sized, or rendered. Whitespace
/// is collapsed so trivial layout differences don't cause misses.
pub fn key_text(provider: &str, model: &str, lang: &str, text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    make_key(provider, model, lang, 't', |h| h.update(normalized.as_bytes()))
}

/// Cache key from a region's **pixels** — fallback for pages with no text layer
/// (images / scanned PDFs, until OCR lands).
pub fn key_pixels(provider: &str, model: &str, lang: &str, image: &image::RgbaImage) -> String {
    make_key(provider, model, lang, 'p', |h| {
        h.update(image.width().to_le_bytes());
        h.update(image.height().to_le_bytes());
        h.update(image.as_raw());
    })
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
    fn pixel_key_is_stable_and_content_sensitive() {
        let a = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        let b = image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 2, 3, 255]));
        assert_eq!(key_pixels("gemini", "m", "en", &a), key_pixels("gemini", "m", "en", &a));
        assert_ne!(key_pixels("gemini", "m", "en", &a), key_pixels("gemini", "m", "en", &b));
        assert_ne!(key_pixels("gemini", "m", "en", &a), key_pixels("gemini", "m", "fr", &a));
        assert_ne!(key_pixels("gemini", "m", "en", &a), key_pixels("gemini", "n", "en", &a));
    }

    #[test]
    fn text_key_collapses_whitespace_and_stays_distinct() {
        // Whitespace-insensitive: same words hit regardless of spacing/newlines.
        assert_eq!(
            key_text("gemini", "m", "en", "Hello   world"),
            key_text("gemini", "m", "en", "Hello world\n"),
        );
        // …but sensitive to the actual text and language.
        assert_ne!(
            key_text("gemini", "m", "en", "Hello"),
            key_text("gemini", "m", "en", "Goodbye"),
        );
        assert_ne!(
            key_text("gemini", "m", "en", "Hello"),
            key_text("gemini", "m", "fr", "Hello"),
        );
        // Text and pixel keys live in separate namespaces — never collide.
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        assert_ne!(
            key_text("gemini", "m", "en", "Hello"),
            key_pixels("gemini", "m", "en", &img),
        );
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join("expat-junk-cache-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("translations.json");

        let mut cache = TranslationCache::with_path(Some(path.clone()));
        assert!(cache.get("k").is_none());
        cache.insert("k".to_string(), sample());

        // A fresh instance reads the persisted entry back.
        let reloaded = TranslationCache::with_path(Some(path.clone()));
        assert_eq!(reloaded.get("k").unwrap().translation, "hello");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
