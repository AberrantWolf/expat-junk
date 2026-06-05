//! Project files: serialize a translation session so work can be saved and
//! resumed.
//!
//! A project references the **source document by path** (it does not embed the
//! rendered pages — those are re-derived on load) and stores the edits: the
//! regions, their translations, the page/zoom-independent settings, and the id
//! counter so region ids stay stable. Saved as pretty JSON with a `.expj`
//! extension.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::Translated;
use crate::config::Backend;
use crate::document::{Document, Region, RegionId};

/// File extension for project files.
pub const EXTENSION: &str = "expj";

/// Bumped if the on-disk shape changes incompatibly.
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    /// Path to the source PDF/image. Absolute so it resolves from anywhere.
    pub source_path: PathBuf,
    pub current_page: usize,
    pub source_lang: String,
    pub target_lang: String,
    pub backend: Backend,
    pub regions: Vec<Region>,
    pub next_region_id: u64,
    pub results: HashMap<RegionId, Translated>,
}

impl Project {
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("serializing project")?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let project: Project =
            serde_json::from_slice(&bytes).context("parsing project file")?;
        if project.version > FORMAT_VERSION {
            anyhow::bail!(
                "this project was saved by a newer version of expat-junk (format {})",
                project.version
            );
        }
        Ok(project)
    }
}

/// Build the document for a loaded project: open the source and apply the saved
/// regions. Returns an error if the source file can't be opened (e.g. moved).
pub fn open_document(project: &Project) -> Result<Document> {
    let mut doc = Document::open(&project.source_path).with_context(|| {
        format!(
            "reopening source document {}",
            project.source_path.display()
        )
    })?;
    doc.restore_regions(project.regions.clone(), project.next_region_id);
    Ok(doc)
}

/// Assemble a [`Project`] from the current session state.
#[allow(clippy::too_many_arguments)]
pub fn build(
    source_path: PathBuf,
    current_page: usize,
    source_lang: String,
    target_lang: String,
    backend: Backend,
    doc: &Document,
    results: HashMap<RegionId, Translated>,
) -> Project {
    Project {
        version: FORMAT_VERSION,
        source_path,
        current_page,
        source_lang,
        target_lang,
        backend,
        regions: doc.regions.clone(),
        next_region_id: doc.next_region_id(),
        results,
    }
}
