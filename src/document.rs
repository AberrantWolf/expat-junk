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

/// A character counts as "inside" a region when at least this fraction of its
/// box area is covered. Loose on purpose: users draw imprecisely and glyph
/// boxes can be oversized or offset for some fonts, so strict containment would
/// drop edge characters.
const CHAR_COVERAGE_THRESHOLD: f32 = 0.5;

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

    /// The next region id to be allocated — saved in the project file so ids
    /// stay stable across save/resume.
    pub fn next_region_id(&self) -> u64 {
        self.next_region_id
    }

    /// Replace the regions (and id counter) when loading a project onto a freshly
    /// opened source document.
    pub fn restore_regions(&mut self, regions: Vec<Region>, next_region_id: u64) {
        self.regions = regions;
        self.next_region_id = next_region_id;
    }

    /// Crop the rendered bitmap to a region, for sending to a vision Translator.
    /// Maps the region's page-point rect to bitmap pixels via the page's render
    /// scale (`bitmap pixels / page points`).
    pub fn crop(&self, id: RegionId) -> Result<image::RgbaImage> {
        let region = self
            .regions
            .iter()
            .find(|r| r.id == id)
            .context("region not found")?;
        let page = self.pages.get(region.page).context("page not found")?;

        let scale_x = page.bitmap.width() as f32 / page.size.0;
        let scale_y = page.bitmap.height() as f32 / page.size.1;
        let x = (region.rect.x * scale_x).round().max(0.0) as u32;
        let y = (region.rect.y * scale_y).round().max(0.0) as u32;
        let w = ((region.rect.w * scale_x).round() as u32).min(page.bitmap.width().saturating_sub(x));
        let h =
            ((region.rect.h * scale_y).round() as u32).min(page.bitmap.height().saturating_sub(y));
        if w == 0 || h == 0 {
            anyhow::bail!("region is empty");
        }
        Ok(image::imageops::crop_imm(&page.bitmap, x, y, w, h).to_image())
    }

    /// The text under a region, taken from the PDF text layer (`char_boxes`) —
    /// no OCR. A character is included when at least [`CHAR_COVERAGE_THRESHOLD`]
    /// of its box overlaps the region; zero-area boxes (some spaces) fall back to
    /// a center test. Characters keep their reading order. Empty when the page
    /// has no text layer (images / scanned PDFs) — callers fall back accordingly.
    pub fn region_text(&self, id: RegionId) -> String {
        let Some(region) = self.regions.iter().find(|r| r.id == id) else {
            return String::new();
        };
        let Some(page) = self.pages.get(region.page) else {
            return String::new();
        };

        let mut text = String::new();
        for (ch, b) in &page.char_boxes {
            let area = b.w * b.h;
            let inside = if area > 0.0 {
                intersect_area(&region.rect, b) / area >= CHAR_COVERAGE_THRESHOLD
            } else {
                let (cx, cy) = (b.x + b.w * 0.5, b.y + b.h * 0.5);
                cx >= region.rect.x
                    && cx <= region.rect.x + region.rect.w
                    && cy >= region.rect.y
                    && cy <= region.rect.y + region.rect.h
            };
            if inside {
                text.push(*ch);
            }
        }
        text
    }
}

/// Area of the overlap between two page-space rectangles (0 if disjoint).
fn intersect_area(a: &Rect, b: &Rect) -> f32 {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    (x1 - x0).max(0.0) * (y1 - y0).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_with(char_boxes: Vec<(char, Rect)>) -> Page {
        Page {
            size: (100.0, 100.0),
            bitmap: image::RgbaImage::new(1, 1),
            char_boxes,
        }
    }

    fn ch(c: char, x: f32, w: f32) -> (char, Rect) {
        (c, Rect { x, y: 10.0, w, h: 10.0 })
    }

    #[test]
    fn region_text_uses_partial_coverage_and_order() {
        // Region covers x in [0, 50], y in [0, 30].
        let chars = vec![
            ch('H', 10.0, 8.0), // fully inside
            ch('i', 18.0, 8.0), // fully inside
            ch('X', 46.0, 8.0), // x 46..54, covered 46..50 = 4/8 = 50% -> included
            ch('Z', 52.0, 8.0), // x 52..60, no overlap -> excluded
        ];
        let mut doc = Document {
            pages: vec![page_with(chars)],
            regions: Vec::new(),
            next_region_id: 0,
        };
        let id = doc.add_region(
            0,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 50.0,
                h: 30.0,
            },
        );
        assert_eq!(doc.region_text(id), "HiX");
    }

    #[test]
    fn region_text_empty_without_text_layer() {
        let mut doc = Document {
            pages: vec![page_with(Vec::new())],
            regions: Vec::new(),
            next_region_id: 0,
        };
        let id = doc.add_region(
            0,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 50.0,
                h: 30.0,
            },
        );
        assert!(doc.region_text(id).is_empty());
    }
}
