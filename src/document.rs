//! Document model + rendering.
//!
//! A [`Document`] is one or more [`Page`]s, each backed by a rasterized bitmap
//! the UI displays and over which the user draws [`Region`]s. Rendering and
//! text-box extraction are done with `pdfium-render` (PDFium loads a native
//! `libpdfium` at runtime — see CLAUDE.md). Plain images are a single-page
//! document with no extractable text layer.
//!
//! Nothing here depends on egui: the UI consumes this model, it does not own it.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default render zoom for newly opened pages (≈144 DPI). The interactive
/// viewer will re-render at the user's zoom; this is just the initial raster.
const DEFAULT_RENDER_ZOOM: f32 = 2.0;

/// A rectangle in *page coordinates* (PDF points, origin top-left), independent
/// of zoom/scroll. The UI converts to/from screen space when painting.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Stable identifier for a region within a document, so UI state and
/// translation results can refer to the same region across edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RegionId(pub u64);

/// A user-drawn block to translate. The user can move/resize it; `rect` is the
/// live geometry. `marked_native` flags regions the PDF can translate directly
/// (selectable text) versus those that must go through image translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    pub id: RegionId,
    pub page: usize,
    pub rect: Rect,
    pub marked_native: bool,
}

/// One rendered page plus its source-text geometry.
pub struct Page {
    /// Page size in PDF points.
    pub size: (f32, f32),
    /// Rasterized page, RGBA. Re-rendered on zoom changes.
    pub bitmap: image::RgbaImage,
    /// Per-character boxes in page coordinates, used to snap region selection
    /// to text blocks and to extract the native text layer. Empty for images.
    pub char_boxes: Vec<(char, Rect)>,
}

/// A loaded source file (PDF or image) and the regions drawn on it.
pub struct Document {
    pub pages: Vec<Page>,
    pub regions: Vec<Region>,
    next_region_id: u64,
}

impl Document {
    /// Load a PDF or image. Dispatches on file extension.
    pub fn open(path: &Path) -> Result<Self> {
        let is_pdf = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        let pages = if is_pdf {
            Self::open_pdf(path)?
        } else {
            vec![Self::open_image(path)?]
        };
        Ok(Self {
            pages,
            regions: Vec::new(),
            next_region_id: 0,
        })
    }

    fn open_pdf(path: &Path) -> Result<Vec<Page>> {
        let pdfium = junk_libs_pdfium::instance()?;
        let rendered = junk_libs_pdfium::render_pdf(pdfium, path, DEFAULT_RENDER_ZOOM)?;
        Ok(rendered
            .into_iter()
            .map(|r| Page {
                size: r.size_pts,
                bitmap: r.image,
                char_boxes: r
                    .char_boxes
                    .into_iter()
                    .map(|c| {
                        (
                            c.ch,
                            Rect {
                                x: c.x,
                                y: c.y,
                                w: c.w,
                                h: c.h,
                            },
                        )
                    })
                    .collect(),
            })
            .collect())
    }

    fn open_image(path: &Path) -> Result<Page> {
        let bitmap = image::open(path)
            .with_context(|| format!("loading image {}", path.display()))?
            .into_rgba8();
        // Images have no PDF point space; treat one pixel as one point.
        let size = (bitmap.width() as f32, bitmap.height() as f32);
        Ok(Page {
            size,
            bitmap,
            char_boxes: Vec::new(),
        })
    }

    /// Allocate a new region on `page` with initial geometry `rect`.
    pub fn add_region(&mut self, page: usize, rect: Rect) -> RegionId {
        let id = RegionId(self.next_region_id);
        self.next_region_id += 1;
        self.regions.push(Region {
            id,
            page,
            rect,
            marked_native: false,
        });
        id
    }

    /// Mutable access to a region by id, for move/resize edits.
    pub fn region_mut(&mut self, id: RegionId) -> Option<&mut Region> {
        self.regions.iter_mut().find(|r| r.id == id)
    }

    /// Remove a region (e.g. on delete).
    pub fn remove_region(&mut self, id: RegionId) {
        self.regions.retain(|r| r.id != id);
    }

    /// Crop the rendered bitmap to a region, for sending to a vision Translator.
    pub fn crop(&self, _id: RegionId) -> Result<image::RgbaImage> {
        // TODO: map region rect -> bitmap pixels at current render scale, crop.
        anyhow::bail!("Document::crop not yet implemented")
    }
}
