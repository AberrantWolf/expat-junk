//! Local API-key storage.
//!
//! Keys live in a separate `secrets.json` in the config dir — not in
//! `settings.json`, not in git — with owner-only permissions on Unix. This is a
//! pragmatic local store; moving to the OS keychain (`keyring`) is a future
//! hardening. The `EXPAT_JUNK_API_KEY` env var still overrides stored keys (see
//! [`crate::config::load_api_key`]).

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Default)]
pub struct Secrets {
    /// Keyed by provider id (`gemini`, `google`, `deepl`, `kagi`).
    keys: HashMap<String, String>,
}

impl Secrets {
    fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "expat-junk")
            .map(|d| d.config_dir().join("secrets.json"))
    }

    pub fn load() -> Self {
        let keys = Self::path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { keys }
    }

    /// The stored key for a provider, if any (empty strings count as unset).
    pub fn get(&self, provider: &str) -> Option<&str> {
        self.keys
            .get(provider)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    /// Set (or, with an empty value, clear) a provider's key, and persist.
    pub fn set(&mut self, provider: &str, key: String) -> Result<()> {
        let mut keys = self.keys.clone();
        let key = key.trim().to_owned();
        if key.is_empty() {
            keys.remove(provider);
        } else {
            keys.insert(provider.to_owned(), key);
        }
        let path = Self::path().context("no config directory available")?;
        let json = serde_json::to_vec_pretty(&keys)?;
        crate::persistence::write(&path, &json)?;
        self.keys = keys;
        Ok(())
    }
}
