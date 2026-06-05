//! User settings and credentials.
//!
//! Non-secret settings (chosen provider, default target language, model name)
//! live in a JSON file under the platform config dir (`directories`). API keys
//! are stored separately via the OS keychain (`keyring`) so they never touch
//! the settings file or version control.
//!
//! NOTE: `keyring` v4 needs a platform backend feature enabled to actually
//! persist secrets (e.g. secret-service on Linux). Until one is wired up,
//! [`load_api_key`] falls back to the `EXPAT_JUNK_API_KEY` env var.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::translate::TextApi;

/// Which backend the user has selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Backend {
    GeminiVision { model: String },
    TesseractText { api: TextApi },
}

impl Default for Backend {
    fn default() -> Self {
        Backend::GeminiVision {
            model: "gemini-flash-lite-latest".to_string(),
        }
    }
}

/// Persisted, non-secret settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub backend: Backend,
    pub target_lang: String,
    /// OCR source language as a Tesseract code (e.g. `eng`, `jpn`, `deu`). Only
    /// used for the OCR fallback on pages with no text layer; the matching
    /// tessdata must be installed. Vision/text-layer paths don't need it.
    pub source_lang: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            backend: Backend::default(),
            target_lang: "en".to_string(),
            source_lang: "eng".to_string(),
        }
    }
}

impl Settings {
    fn config_path() -> Option<std::path::PathBuf> {
        directories::ProjectDirs::from("", "", "expat-junk")
            .map(|d| d.config_dir().join("settings.json"))
    }

    /// Load settings from the platform config dir, or defaults if absent/unreadable.
    pub fn load() -> Self {
        Self::config_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Persist settings to the platform config dir (best-effort).
    pub fn save(&self) {
        let Some(path) = Self::config_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Fetch the API key for the active backend from the OS keychain.
pub fn load_api_key(_backend: &Backend) -> Result<String> {
    // TODO: keyring::Entry once a backend feature is enabled.
    std::env::var("EXPAT_JUNK_API_KEY")
        .map_err(|_| anyhow::anyhow!("no API key configured (set EXPAT_JUNK_API_KEY for now)"))
}
