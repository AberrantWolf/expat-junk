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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            backend: Backend::default(),
            target_lang: "en".to_string(),
        }
    }
}

impl Settings {
    /// Load settings from the platform config dir, or defaults if absent.
    pub fn load() -> Result<Self> {
        // TODO: read JSON from directories::ProjectDirs config_dir().
        Ok(Settings::default())
    }
}

/// Fetch the API key for the active backend from the OS keychain.
pub fn load_api_key(_backend: &Backend) -> Result<String> {
    // TODO: keyring::Entry once a backend feature is enabled.
    std::env::var("EXPAT_JUNK_API_KEY")
        .map_err(|_| anyhow::anyhow!("no API key configured (set EXPAT_JUNK_API_KEY for now)"))
}
