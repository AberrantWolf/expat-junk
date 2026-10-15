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

use crate::config::Backend;
use crate::document::{Document, Region, RegionId};
use crate::translation::Translated;

/// File extension for project files.
pub const EXTENSION: &str = "expj";

/// Bumped if the on-disk shape changes incompatibly.
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    /// Path to the source PDF/image. Absolute so it resolves from anywhere.
    pub source_path: PathBuf,
    #[serde(default)]
    pub source_fingerprint: Option<String>,
    pub current_page: usize,
    pub source_lang: String,
    #[serde(default)]
    pub reading_direction: junk_libs_platen::reading_order::Direction,
    pub target_lang: String,
    #[serde(default)]
    pub context: String,
    pub backend: Backend,
    pub regions: Vec<Region>,
    pub next_region_id: u64,
    pub results: HashMap<RegionId, Translated>,
}

impl Project {
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("serializing project")?;
        crate::persistence::write(path, &json)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let mut project: Project =
            serde_json::from_slice(&bytes).context("parsing project file")?;
        if project.version != FORMAT_VERSION {
            anyhow::bail!("unsupported project format {}", project.version);
        }
        if project.source_path.is_relative() {
            project.source_path = path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&project.source_path);
        }
        let mut seen = std::collections::HashSet::new();
        for region in &project.regions {
            anyhow::ensure!(seen.insert(region.id), "duplicate region id in project");
            let r = region.rect;
            anyhow::ensure!(
                [r.x, r.y, r.w, r.h].iter().all(|n| n.is_finite()) && r.w > 0.0 && r.h > 0.0,
                "invalid region geometry in project"
            );
        }
        if let Some(max_id) = project.regions.iter().map(|r| r.id.0).max() {
            project.next_region_id = project
                .next_region_id
                .max(max_id.checked_add(1).context("region ids exhausted")?);
        }
        project.results.retain(|id, _| seen.contains(id));
        anyhow::ensure!(
            project.results.values().all(|t| t
                .style
                .size_pt
                .is_none_or(|s| s.is_finite() && s > 0.0 && s <= 400.0)),
            "invalid font size in project"
        );
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
    if let Some(expected) = &project.source_fingerprint {
        anyhow::ensure!(
            expected == doc.source_fingerprint(),
            "project source document has changed; restore the original source file"
        );
    }
    for region in &project.regions {
        let page = doc
            .pages
            .get(region.page)
            .context("project region refers to a missing page")?;
        let r = region.rect;
        anyhow::ensure!(
            r.x >= 0.0
                && r.y >= 0.0
                && r.x + r.w <= page.size.0 + 0.1
                && r.y + r.h <= page.size.1 + 0.1,
            "project region falls outside its source page"
        );
    }
    doc.restore_regions(project.regions.clone(), project.next_region_id);
    Ok(doc)
}

/// Assemble a [`Project`] from the current session state.
#[allow(clippy::too_many_arguments)]
pub fn build(
    source_path: PathBuf,
    current_page: usize,
    source_lang: String,
    reading_direction: junk_libs_platen::reading_order::Direction,
    target_lang: String,
    context: String,
    backend: Backend,
    doc: &Document,
    results: HashMap<RegionId, Translated>,
) -> Project {
    Project {
        version: FORMAT_VERSION,
        source_path: std::fs::canonicalize(&source_path).unwrap_or(source_path),
        source_fingerprint: Some(doc.source_fingerprint().to_owned()),
        current_page,
        source_lang,
        reading_direction,
        target_lang,
        context,
        backend,
        regions: doc.regions.clone(),
        next_region_id: doc.next_region_id(),
        results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Rect;

    fn saved_project(dir: &Path) -> PathBuf {
        let image = dir.join("source.png");
        image::RgbaImage::new(100, 100).save(&image).unwrap();
        let mut doc = Document::open(&image).unwrap();
        doc.add_region(
            0,
            Rect {
                x: 10.0,
                y: 10.0,
                w: 20.0,
                h: 20.0,
            },
        );
        let project = build(
            image,
            0,
            "eng".into(),
            Default::default(),
            "en".into(),
            String::new(),
            Backend::default(),
            &doc,
            HashMap::new(),
        );
        let path = dir.join("session.expj");
        project.save(&path).unwrap();
        path
    }

    #[test]
    fn old_projects_load_and_region_ids_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let path = saved_project(dir.path());
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("context");
        value.as_object_mut().unwrap().remove("reading_direction");
        value.as_object_mut().unwrap().remove("source_fingerprint");
        value["next_region_id"] = 0.into();
        value["source_path"] = "source.png".into();
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let loaded = Project::load(&path).unwrap();
        assert!(loaded.context.is_empty());
        let mut doc = open_document(&loaded).unwrap();
        let next = doc.add_region(
            0,
            Rect {
                x: 30.0,
                y: 30.0,
                w: 10.0,
                h: 10.0,
            },
        );
        assert_ne!(next, loaded.regions[0].id);
    }

    #[test]
    fn refuses_a_changed_source_even_when_page_dimensions_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = saved_project(dir.path());
        let loaded = Project::load(&path).unwrap();
        image::RgbaImage::from_pixel(100, 100, image::Rgba([255, 0, 0, 255]))
            .save(&loaded.source_path)
            .unwrap();
        assert!(open_document(&loaded).is_err());
    }

    #[test]
    fn rejects_duplicate_ids_invalid_geometry_and_missing_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = saved_project(dir.path());
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut duplicate = original.clone();
        let region = duplicate["regions"][0].clone();
        duplicate["regions"].as_array_mut().unwrap().push(region);
        std::fs::write(&path, serde_json::to_vec(&duplicate).unwrap()).unwrap();
        assert!(Project::load(&path).is_err());
        let mut invalid = original.clone();
        invalid["regions"][0]["rect"]["w"] = (-1).into();
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(Project::load(&path).is_err());
        let mut missing = original;
        missing["regions"][0]["page"] = 50.into();
        std::fs::write(&path, serde_json::to_vec(&missing).unwrap()).unwrap();
        assert!(open_document(&Project::load(&path).unwrap()).is_err());
    }
}
