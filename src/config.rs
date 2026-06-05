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

impl Backend {
    /// Identifier for the API credential this backend needs (the secrets key).
    pub fn secret_id(&self) -> &'static str {
        match self {
            Backend::GeminiVision { .. } => "gemini",
            Backend::TesseractText { api } => match api {
                TextApi::Google => "google",
                TextApi::DeepL => "deepl",
                TextApi::Kagi => "kagi",
            },
        }
    }

    /// Human-readable provider name for the settings UI.
    pub fn provider_label(&self) -> &'static str {
        match self {
            Backend::GeminiVision { .. } => "Gemini (vision)",
            Backend::TesseractText { api } => match api {
                TextApi::Google => "Google Translate",
                TextApi::DeepL => "DeepL",
                TextApi::Kagi => "Kagi",
            },
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

/// Fetch the API key for the active backend. The `EXPAT_JUNK_API_KEY` env var
/// overrides everything (handy for dev); otherwise it comes from the local
/// secrets store, set via the Settings window.
pub fn load_api_key(secrets: &crate::secrets::Secrets, backend: &Backend) -> Result<String> {
    if let Ok(key) = std::env::var("EXPAT_JUNK_API_KEY")
        && !key.is_empty()
    {
        return Ok(key);
    }
    secrets
        .get(backend.secret_id())
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no API key set for {} — open Settings to add one",
                backend.provider_label()
            )
        })
}
