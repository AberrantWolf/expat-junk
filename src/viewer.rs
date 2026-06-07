//! Host adapter between the shared `junk-libs-egui-docview` widget and our
//! translation domain.
//!
//! The interactive page canvas (zoom/pan, draw/move/resize/delete regions) now
//! lives in the `junk-libs-egui-docview` crate as `DocView`, driven by the
//! `PageModel` our [`crate::document::Document`] implements. Everything
//! translation-specific that the old in-repo viewer carried lives here instead:
//! the bundled CJK fonts, the per-region rendered-patch cache, and the
//! [`RegionOverlay`] that paints each translated region's styled patch over the
//! page — the live, in-app preview of the export overlay.
//!
//! The patch is rasterized with the **same** [`crate::export::render_region_patch`]
//! the exporter uses, so the preview is faithful (edited text, rotation, bold,
//! background override, alignment, size). Patches are cached per region and
//! rebuilt only when the inputs that affect them change.

use std::collections::HashMap;

use ab_glyph::FontRef;
use egui::Color32;
use image::Rgba;
use junk_libs_egui_docview::{OverlayCtx, RegionOverlay};

use crate::app::{Justify, Rotate, Translated};
use crate::document::RegionId;
use crate::export::render_region_patch;

/// Persistent translation-overlay state, owned by the app across frames: the two
/// weight-pinned fonts and the GPU-uploaded patch cache.
pub struct TranslationOverlay {
    /// Fonts used to rasterize the preview patches — the same bundled CJK font the
    /// export uses (regular + bold, weight-pinned), so the preview matches output.
    font: FontRef<'static>,
    font_bold: FontRef<'static>,
    /// Cached, GPU-uploaded translation patches keyed by region, rebuilt only when
    /// a region's text/style/size changes.
    patches: HashMap<RegionId, CachedPatch>,
}

impl Default for TranslationOverlay {
    fn default() -> Self {
        Self {
            font: crate::fonts::weighted(false),
            font_bold: crate::fonts::weighted(true),
            patches: HashMap::new(),
        }
    }
}

impl TranslationOverlay {
    /// Invalidate the patch cache (e.g. when a new document is opened).
    pub fn reset(&mut self) {
        self.patches.clear();
    }

    /// Borrow the overlay for one frame's render. `show` toggles whether patches
    /// are drawn (false = show only the region boxes). Pass the current results.
    pub fn frame<'a>(
        &'a mut self,
        results: &'a HashMap<RegionId, Translated>,
        show: bool,
    ) -> TranslationFrame<'a> {
        TranslationFrame {
            results,
            show,
            font: &self.font,
            font_bold: &self.font_bold,
            patches: &mut self.patches,
        }
    }

    /// Drop cached patches whose regions no longer have a translation (deleted or
    /// cleared), releasing their texture memory. Call after the frame renders.
    pub fn prune(&mut self, results: &HashMap<RegionId, Translated>) {
        self.patches.retain(|id, _| results.contains_key(id));
    }
}

/// One region's rendered translation patch plus the inputs it was built from, so
/// we can skip re-rendering when nothing relevant changed.
struct CachedPatch {
    key: PatchKey,
    tex: egui::TextureHandle,
}

#[derive(Clone, PartialEq)]
struct PatchKey {
    text: String,
    rotate: Rotate,
    bg: [u8; 3],
    justify: Justify,
    bold: bool,
    size_pt: Option<f32>,
    w: u32,
    h: u32,
}

/// One frame's borrow of [`TranslationOverlay`] + the current results, wired into
/// the widget as its [`RegionOverlay`].
pub struct TranslationFrame<'a> {
    results: &'a HashMap<RegionId, Translated>,
    show: bool,
    font: &'a FontRef<'static>,
    font_bold: &'a FontRef<'static>,
    patches: &'a mut HashMap<RegionId, CachedPatch>,
}

impl RegionOverlay for TranslationFrame<'_> {
    fn paint(&mut self, ctx: &OverlayCtx) -> bool {
        if !self.show {
            return false;
        }
        let id = ctx.id();
        let Some(t) = self.results.get(&id) else {
            return false;
        };
        let pr = ctx.page_rect();
        let scale = ctx.bitmap_scale();
        let w_px = (pr.w * scale).round() as u32;
        let h_px = (pr.h * scale).round() as u32;
        if w_px == 0 || h_px == 0 {
            return false;
        }

        // Background override, else the page color sampled around the region (lazy
        // — only sampled when there's no override).
        let bg = match t.style.background {
            Some([r, g, b]) => Rgba([r, g, b, 255]),
            None => {
                let c = ctx.sample_background(pr);
                Rgba([c[0], c[1], c[2], 255])
            }
        };

        let key = PatchKey {
            text: t.display_text().trim().to_owned(),
            rotate: t.style.rotate,
            bg: [bg[0], bg[1], bg[2]],
            justify: t.style.justify,
            bold: t.style.bold,
            size_pt: t.style.size_pt,
            w: w_px,
            h: h_px,
        };
        if self.patches.get(&id).map(|c| &c.key) != Some(&key) {
            let patch = render_region_patch(w_px, h_px, bg, t, self.font, self.font_bold, scale);
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [patch.width() as usize, patch.height() as usize],
                patch.as_raw(),
            );
            let tex = ctx.egui_ctx().load_texture(
                format!("tr-{}", id.0),
                img,
                egui::TextureOptions::LINEAR,
            );
            self.patches.insert(id, CachedPatch { key, tex });
        }
        if let Some(c) = self.patches.get(&id) {
            ctx.painter().image(
                c.tex.id(),
                ctx.screen_rect(),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            true
        } else {
            false
        }
    }
}
