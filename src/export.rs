//! Export: bake translations back onto the original document.
//!
//! Strategy (v1): **overlay on original**, not reflow. For each translated
//! region we keep the original page and:
//!   1. sample the background color around the region's source block,
//!   2. paint a solid cover rectangle of that color over the source text,
//!   3. overprint the translated text on top, fit to the box.
//!
//! Layout is preserved; on photo/textured backgrounds the solid fill leaves
//! visible seams -- an accepted loss. Implemented with `lopdf` (load + mutate
//! the original) and `printpdf` (compose new content). Images export as PNG
//! with the same cover-and-overprint done on the raster.

use std::path::Path;

use anyhow::Result;

use crate::document::Document;
use crate::translate::TranslationResult;

/// A region's geometry paired with the translation to stamp into it.
pub struct Overlay<'a> {
    pub region: &'a crate::document::Region,
    pub result: &'a TranslationResult,
}

/// Write `doc` with `overlays` applied to `out_path` (PDF or PNG by extension).
pub fn export(_doc: &Document, _overlays: &[Overlay], _out_path: &Path) -> Result<()> {
    // TODO: PDF path via lopdf/printpdf; image path via the `image` crate.
    anyhow::bail!("export not yet implemented")
}
