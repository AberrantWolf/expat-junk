//! User settings and credentials.
//!
//! Settings live in settings.json; credentials live separately in secrets.json.
//! EXPAT_JUNK_API_KEY overrides the selected provider's saved credential.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::translate::TextApi;

/// Which backend the user has selected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Backend {
    GeminiVision { model: String },
    TesseractText { api: TextApi },
}

impl Default for Backend {
    fn default() -> Self {
        Backend::GeminiVision {
            model: "gemini-3.5-flash-lite".to_string(),
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

/// A language offered in the source/target pickers. `ocr` is the Tesseract code
/// the OCR source path expects; `iso` is the ISO-639 code translation APIs (e.g.
/// Google) expect as the target — the two differ for many languages (Japanese is
/// `jpn` vs `ja`), which is why both are kept.
pub struct Language {
    pub name: &'static str,
    pub ocr: &'static str,
    pub iso: &'static str,
}

/// Curated list of languages with installable Tesseract data, for the pickers.
pub const LANGUAGES: &[Language] = &[
    Language {
        name: "English",
        ocr: "eng",
        iso: "en",
    },
    Language {
        name: "Japanese",
        ocr: "jpn",
        iso: "ja",
    },
    Language {
        name: "Chinese (Simplified)",
        ocr: "chi_sim",
        iso: "zh-CN",
    },
    Language {
        name: "Chinese (Traditional)",
        ocr: "chi_tra",
        iso: "zh-TW",
    },
    Language {
        name: "Korean",
        ocr: "kor",
        iso: "ko",
    },
    Language {
        name: "French",
        ocr: "fra",
        iso: "fr",
    },
    Language {
        name: "German",
        ocr: "deu",
        iso: "de",
    },
    Language {
        name: "Spanish",
        ocr: "spa",
        iso: "es",
    },
    Language {
        name: "Italian",
        ocr: "ita",
        iso: "it",
    },
    Language {
        name: "Portuguese",
        ocr: "por",
        iso: "pt",
    },
    Language {
        name: "Russian",
        ocr: "rus",
        iso: "ru",
    },
    Language {
        name: "Dutch",
        ocr: "nld",
        iso: "nl",
    },
    Language {
        name: "Arabic",
        ocr: "ara",
        iso: "ar",
    },
    Language {
        name: "Hindi",
        ocr: "hin",
        iso: "hi",
    },
    Language {
        name: "Vietnamese",
        ocr: "vie",
        iso: "vi",
    },
    Language {
        name: "Thai",
        ocr: "tha",
        iso: "th",
    },
];

impl Language {
    /// Display name for an OCR (Tesseract) code, falling back to the raw code so
    /// a custom value set outside the picker still shows something sensible.
    pub fn name_for_ocr(code: &str) -> &str {
        LANGUAGES
            .iter()
            .find(|l| l.ocr == code)
            .map_or(code, |l| l.name)
    }

    /// Display name for an ISO target code, falling back to the raw code.
    pub fn name_for_iso(code: &str) -> &str {
        LANGUAGES
            .iter()
            .find(|l| l.iso == code)
            .map_or(code, |l| l.name)
    }
}

/// Persisted, non-secret settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub backend: Backend,
    pub target_lang: String,
    /// Optional document context / terminology sent to Gemini and DeepL.
    #[serde(default)]
    pub context: String,
    /// OCR source language as a Tesseract code (e.g. `eng`, `jpn`, `deu`). Only
    /// used for the OCR fallback on pages with no text layer; the matching
    /// tessdata must be installed. Vision/text-layer paths don't need it.
    pub source_lang: String,
    #[serde(default)]
    pub reading_direction: junk_libs_platen::reading_order::Direction,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            backend: Backend::default(),
            target_lang: "en".to_string(),
            context: String::new(),
            source_lang: "eng".to_string(),
            reading_direction: Default::default(),
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
        let Some(path) = Self::config_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = crate::persistence::write(&path, &json);
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
