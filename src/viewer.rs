//! Interactive page canvas: a zoomable, pageable view of the document over which
//! the user draws, selects, moves, resizes, and deletes translation regions.
//!
//! This is the seed of the eventual shared `junk-libs-egui-pdfview` widget, so it
//! stays self-contained: it takes the `Document`, the current page, and the
//! selection, and owns only its texture, zoom, and in-progress drag. All geometry
//! is kept in page points; the page↔screen transform is derived from the
//! displayed image rect each frame, so it's correct at any zoom.

use std::collections::HashMap;

use ab_glyph::FontRef;
use egui::{Color32, Pos2, Rect as ERect, Sense, Stroke, Vec2};
use image::Rgba;

use crate::app::{Justify, Rotate, Translated};
use crate::document::{Document, Rect, RegionId};
use crate::export::{render_region_patch, sample_background, PixelRect};

/// Screen-space size of a corner resize handle, and the grab tolerance.
const HANDLE: f32 = 7.0;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.0;
const ZOOM_STEP: f32 = 1.25;
/// Bounds on the page render scale (pixels per point) for on-demand re-rendering.
const MIN_RENDER_SCALE: f32 = 1.0;
const MAX_RENDER_SCALE: f32 = 6.0;
const REGION_STROKE: Color32 = Color32::from_rgb(0xff, 0xa5, 0x00);
const REGION_FILL: Color32 = Color32::from_rgba_premultiplied(0x30, 0x20, 0x00, 0x20);
const ACCENT: Color32 = Color32::from_rgb(0x4e, 0x9a, 0xff);
const ACCENT_FILL: Color32 = Color32::from_rgba_premultiplied(0x12, 0x25, 0x40, 0x30);
/// Highlight for the region/handle the idle pointer is over.
const HOVER: Color32 = Color32::from_rgb(0xff, 0xcc, 0x66);

/// What the pointer is doing between press and release. Coordinates are in page
/// points.
#[derive(Default)]
enum Drag {
    #[default]
    Idle,
    /// Rubber-banding a new region from `start` to `current`.
    Drawing { start: Pos2, current: Pos2 },
    /// Moving an existing region; `grab` is the pointer offset from its corner.
    Moving { id: RegionId, grab: Vec2 },
    /// Resizing a region; `fixed` is the opposite corner that stays put.
    Resizing { id: RegionId, fixed: Pos2 },
}

/// What the pointer is hovering over when idle — drives the highlight + cursor so
/// the user can tell a click will grab a widget rather than draw a new region. It
/// is the same hit-test [`decide_drag`] uses, so the highlight matches the action.
#[derive(Clone, Copy, PartialEq)]
enum Hover {
    None,
    /// Over a region body (a press would move it).
    Body(RegionId),
    /// Over a corner of the selected region (a press would resize it). The index
    /// is the corner: 0=TL, 1=TR, 2=BL, 3=BR.
    Handle(RegionId, usize),
}

pub struct Viewer {
    texture: Option<egui::TextureHandle>,
    /// Which page `texture` holds, so we rebuild it on page changes.
    texture_page: usize,
    /// Render scale (px/pt) the current texture was built at, so we rebuild it
    /// when the page is re-rendered at a different resolution.
    texture_scale: f32,
    /// On-screen magnification (1.0 = page shown at its native point size).
    zoom: f32,
    /// When true, zoom is recomputed each frame to fit the viewport width.
    fit: bool,
    /// When true, translated regions render their text over the page (a live,
    /// in-app preview of the export overlay); when false, only the region boxes
    /// show so the original is unobstructed.
    show_translations: bool,
    /// Fonts used to rasterize the preview patches — the same bundled CJK font
    /// the export uses (regular + bold, weight-pinned), so the preview matches
    /// the output.
    font: FontRef<'static>,
    font_bold: FontRef<'static>,
    /// Cached, GPU-uploaded translation patches keyed by region, rebuilt only
    /// when a region's text/style/size changes.
    patches: HashMap<RegionId, CachedPatch>,
    drag: Drag,
    /// Middle-button panning in progress (drag-scrolls the view, never draws).
    panning: bool,
    /// Last frame's scroll offset, mirrored from the `ScrollArea` so we can drive
    /// it directly when zooming (to anchor the zoom the same frame, no settle lag).
    scroll_offset: Vec2,
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

impl Default for Viewer {
    fn default() -> Self {
        Self {
            texture: None,
            texture_page: 0,
            texture_scale: 0.0,
            zoom: 1.0,
            fit: true,
            show_translations: true,
            font: crate::fonts::weighted(false),
            font_bold: crate::fonts::weighted(true),
            patches: HashMap::new(),
            drag: Drag::Idle,
            panning: false,
            scroll_offset: Vec2::ZERO,
        }
    }
}

impl Viewer {
    /// Invalidate cached state (e.g. when a new document is opened).
    pub fn reset(&mut self) {
        self.texture = None;
        self.texture_scale = 0.0;
        self.patches.clear();
        self.drag = Drag::Idle;
        self.panning = false;
        self.scroll_offset = Vec2::ZERO;
        self.fit = true;
        self.zoom = 1.0;
    }

    /// Render the page and handle interaction. Returns `true` if a region was
    /// added, moved, resized, or deleted this frame (so the caller can mark the
    /// project dirty).
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        doc: &mut Document,
        current_page: &mut usize,
        selected: &mut Option<RegionId>,
        results: &HashMap<RegionId, Translated>,
    ) -> bool {
        let total_pages = doc.pages.len();
        let mut edited = false;

        // Accumulates this frame's zoom change (from buttons and/or Ctrl+scroll) as
        // a single ratio, applied below to keep a fixed point (cursor or viewport
        // center) pinned while zooming.
        let mut zoom_factor = 1.0;

        // --- control bar: zoom + page navigation ---------------------------
        ui.horizontal(|ui| {
            if ui.button("−").on_hover_text("Zoom out").clicked() {
                let new = (self.zoom / ZOOM_STEP).max(MIN_ZOOM);
                zoom_factor *= new / self.zoom;
                self.zoom = new;
                self.fit = false;
            }
            ui.label(format!("{:.0}%", self.zoom * 100.0));
            if ui.button("+").on_hover_text("Zoom in").clicked() {
                let new = (self.zoom * ZOOM_STEP).min(MAX_ZOOM);
                zoom_factor *= new / self.zoom;
                self.zoom = new;
                self.fit = false;
            }
            if ui.selectable_label(self.fit, "Fit width").clicked() {
                self.fit = true;
            }

            ui.separator();
            ui.add_enabled_ui(!results.is_empty(), |ui| {
                ui.selectable_label(self.show_translations, "Translations")
                    .on_hover_text("Overlay translated text on the page (preview of the export)")
                    .clicked()
                    .then(|| self.show_translations = !self.show_translations);
            });

            if total_pages > 1 {
                ui.separator();
                if ui
                    .add_enabled(*current_page > 0, egui::Button::new("◀"))
                    .clicked()
                {
                    *current_page -= 1;
                }
                ui.label(format!("Page {} / {}", *current_page + 1, total_pages));
                if ui
                    .add_enabled(*current_page + 1 < total_pages, egui::Button::new("▶"))
                    .clicked()
                {
                    *current_page += 1;
                }
            }
        });
        ui.separator();

        let Some(page_size_pts) = doc.pages.get(*current_page).map(|p| p.size) else {
            self.texture = None;
            ui.centered_and_justified(|ui| {
                ui.label("Open a PDF or image to begin.");
            });
            return edited;
        };
        let page_size = egui::vec2(page_size_pts.0, page_size_pts.1);

        // Resolve the on-screen zoom (fit-to-width / Ctrl+scroll) before rendering.
        if self.fit && page_size.x > 0.0 {
            self.zoom = (ui.available_width() / page_size.x).clamp(MIN_ZOOM, MAX_ZOOM);
        }
        // Ctrl+scroll (and trackpad pinch) zooms. egui routes the ctrl-modified
        // wheel into `zoom_delta` (a multiplicative factor) and keeps it out of the
        // scroll delta, so the ScrollArea below won't also pan — read it directly.
        let zoom_delta = ui.input(|i| i.zoom_delta());
        if (zoom_delta - 1.0).abs() > f32::EPSILON {
            let old = self.zoom;
            self.zoom = (self.zoom * zoom_delta).clamp(MIN_ZOOM, MAX_ZOOM);
            self.fit = false;
            zoom_factor *= self.zoom / old;
        }

        // Re-render the current page so its raster matches the on-screen size
        // (zoom × device pixels-per-point), keeping it crisp at high zoom.
        // Quantized so scrolling the zoom doesn't re-render every frame; a no-op
        // for image sources.
        let ppp = ui.ctx().pixels_per_point();
        let target_scale = render_scale_for(self.zoom, ppp);
        if (page_scale(doc, *current_page) - target_scale).abs() > 0.1 {
            let _ = doc.rerender_page(*current_page, target_scale);
        }

        // (Re)build the texture when the page changed or its raster scale changed.
        let bitmap_scale = page_scale(doc, *current_page);
        if self.texture.is_none()
            || self.texture_page != *current_page
            || (self.texture_scale - bitmap_scale).abs() > 0.01
        {
            if let Some(page) = doc.pages.get(*current_page) {
                let size = [page.bitmap.width() as usize, page.bitmap.height() as usize];
                let img = egui::ColorImage::from_rgba_unmultiplied(size, page.bitmap.as_raw());
                self.texture =
                    Some(ui.ctx().load_texture("page", img, egui::TextureOptions::LINEAR));
                self.texture_page = *current_page;
                self.texture_scale = bitmap_scale;
            }
        }
        let Some(texture) = self.texture.as_ref() else {
            return edited;
        };
        let tex_id = texture.id();

        let display = page_size * self.zoom;

        // Zoom keeps a fixed anchor pinned. Rather than nudging the offset *after*
        // layout (which lands a frame late and visibly drifts into place), we drive
        // the `ScrollArea` offset directly so the page is in its final position the
        // same frame. The anchor is the cursor when it's over the view (Ctrl+scroll),
        // else the view's center (zoom buttons / keyboard). With `O` the old offset
        // and `f` the zoom ratio, `O' = f·O + (f−1)·(anchor − viewport.min)` keeps
        // the page-point under the anchor fixed.
        let viewport = ui.available_rect_before_wrap();
        let forced_offset = ((zoom_factor - 1.0).abs() > f32::EPSILON).then(|| {
            let anchor = ui
                .input(|i| i.pointer.hover_pos())
                .filter(|p| viewport.contains(*p))
                .unwrap_or_else(|| viewport.center());
            let target =
                self.scroll_offset * zoom_factor + (anchor - viewport.min) * (zoom_factor - 1.0);
            // Clamp to the real scroll range. A dimension where the page *fits* has
            // an empty range, so its offset pins to 0 (centered). Without this egui
            // would position the content at the raw offset and only clamp afterward
            // — that per-frame shift-then-snap is the jitter.
            let max_off = (display - viewport.size()).max(Vec2::ZERO);
            target.max(Vec2::ZERO).min(max_off)
        });

        let mut area = egui::ScrollArea::both().id_salt("page_scroll").auto_shrink([false; 2]);
        if let Some(off) = forced_offset {
            area = area.scroll_offset(off);
        }
        let out = area.show(ui, |ui| {
            // Lay the content out at least viewport-sized so a page smaller than the
            // view can be centered in it (egui otherwise top-left-justifies); when
            // the page is larger, the extra is the scrollable region. Use the
            // pre-scrollbar viewport size so a fitting page doesn't flicker a bar.
            let content_size = display.max(viewport.size());
            let (content_rect, _) = ui.allocate_exact_size(content_size, Sense::hover());
            let pad = ((content_size - display) * 0.5).max(Vec2::ZERO);
            let rect = ERect::from_min_size(content_rect.min + pad, display);
            ui.painter().image(
                tex_id,
                rect,
                ERect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            let resp = ui.interact(rect, ui.id().with("page_canvas"), Sense::click_and_drag());

            let scale = if page_size.x > 0.0 {
                rect.width() / page_size.x
            } else {
                1.0
            };
            let to_screen = |p: Pos2| rect.min + p.to_vec2() * scale;
            let to_page = |s: Pos2| ((s - rect.min) / scale).to_pos2();

            // --- interaction ---------------------------------------------------
            // Middle-button drag pans the view (grab-scroll), and is handled
            // before region logic so it never starts drawing a region.
            let middle_down = ui.input(|i| i.pointer.middle_down());
            if middle_down && (self.panning || resp.contains_pointer()) {
                self.panning = true;
                // Immediate (un-animated) so the page tracks the cursor 1:1.
                ui.scroll_with_delta_animation(
                    ui.input(|i| i.pointer.delta()),
                    egui::style::ScrollAnimation::none(),
                );
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            } else {
                self.panning = false;
            }

            // Region draw/move/resize is the *primary* button only — the `_by`
            // variants keep middle/secondary buttons out of it.
            // Start from the *press origin*, not interact_pointer_pos: the latter
            // is only sampled once the drag threshold is crossed, which offsets
            // the start by a few pixels from where the user actually clicked.
            if resp.drag_started_by(egui::PointerButton::Primary) {
                if let Some(press) = ui.input(|i| i.pointer.press_origin()) {
                    self.drag = decide_drag(doc, *current_page, *selected, to_page(press), scale);
                    match self.drag {
                        Drag::Moving { id, .. } | Drag::Resizing { id, .. } => {
                            *selected = Some(id);
                        }
                        _ => {}
                    }
                }
            }
            if resp.dragged_by(egui::PointerButton::Primary) {
                if let Some(ptr) = resp.interact_pointer_pos() {
                    let pp = to_page(ptr);
                    match &mut self.drag {
                        Drag::Drawing { current, .. } => *current = pp,
                        Drag::Moving { id, grab } => {
                            if let Some(r) = doc.region_mut(*id) {
                                r.rect.x = pp.x - grab.x;
                                r.rect.y = pp.y - grab.y;
                                edited = true;
                            }
                        }
                        Drag::Resizing { id, fixed } => {
                            if let Some(r) = doc.region_mut(*id) {
                                r.rect = norm_rect(*fixed, pp);
                                edited = true;
                            }
                        }
                        Drag::Idle => {}
                    }
                }
            }
            if resp.drag_stopped_by(egui::PointerButton::Primary) {
                if let Drag::Drawing { start, current } = self.drag {
                    let r = norm_rect(start, current);
                    if r.w > 2.0 && r.h > 2.0 {
                        *selected = Some(doc.add_region(*current_page, r));
                        edited = true;
                    }
                }
                self.drag = Drag::Idle;
            }
            if resp.clicked() {
                if let Some(ptr) = resp.interact_pointer_pos() {
                    *selected = topmost_at(doc, *current_page, to_page(ptr));
                }
            }

            // What the idle pointer is over — for the hover highlight + cursor, so
            // the user can tell when a press will grab a region/handle instead of
            // drawing. Skipped mid-drag/pan (the action is already decided).
            let hover = if matches!(self.drag, Drag::Idle) && !self.panning {
                resp.hover_pos()
                    .map(|p| hover_target(doc, *current_page, *selected, to_page(p), scale))
                    .unwrap_or(Hover::None)
            } else {
                Hover::None
            };
            if !self.panning {
                match hover {
                    Hover::Handle(_, i) => ui.ctx().set_cursor_icon(if i == 0 || i == 3 {
                        egui::CursorIcon::ResizeNwSe
                    } else {
                        egui::CursorIcon::ResizeNeSw
                    }),
                    Hover::Body(_) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
                    Hover::None if resp.hovered() => {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair)
                    }
                    Hover::None => {}
                }
            }
            if let Some(id) = *selected {
                // Don't treat Delete/Backspace as "delete region" while a text
                // widget (e.g. the translation editor in the info panel) holds
                // keyboard focus — there those keys edit the text.
                let editing = ui.memory(|m| m.focused().is_some());
                let del = !editing
                    && ui.input(|i| {
                        i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)
                    });
                if del {
                    doc.remove_region(id);
                    *selected = None;
                    edited = true;
                }
            }

            // --- paint ---------------------------------------------------------
            let painter = ui.painter_at(rect.expand(HANDLE));
            let page_bitmap = doc.pages.get(*current_page).map(|p| &p.bitmap);
            for region in doc.regions.iter().filter(|r| r.page == *current_page) {
                let scr = ERect::from_min_size(
                    to_screen(egui::pos2(region.rect.x, region.rect.y)),
                    egui::vec2(region.rect.w * scale, region.rect.h * scale),
                );
                let is_sel = *selected == Some(region.id);

                // Live translation overlay: render the region's styled patch with
                // the *same* renderer the export uses, then draw it over the page —
                // so this is a true preview of the output.
                let translation = self
                    .show_translations
                    .then(|| results.get(&region.id))
                    .flatten();
                let mut drew_overlay = false;
                if let (Some(t), Some(bmp)) = (translation, page_bitmap) {
                    let w_px = (region.rect.w * bitmap_scale).round() as u32;
                    let h_px = (region.rect.h * bitmap_scale).round() as u32;
                    if w_px > 0 && h_px > 0 {
                        let bg = match t.style.background {
                            Some([r, g, b]) => Rgba([r, g, b, 255]),
                            None => sample_background(
                                bmp,
                                PixelRect {
                                    x: (region.rect.x * bitmap_scale).round() as i64,
                                    y: (region.rect.y * bitmap_scale).round() as i64,
                                    w: w_px as i64,
                                    h: h_px as i64,
                                },
                            ),
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
                        if self.patches.get(&region.id).map(|c| &c.key) != Some(&key) {
                            let patch = render_region_patch(
                                w_px,
                                h_px,
                                bg,
                                t,
                                &self.font,
                                &self.font_bold,
                                bitmap_scale,
                            );
                            let img = egui::ColorImage::from_rgba_unmultiplied(
                                [patch.width() as usize, patch.height() as usize],
                                patch.as_raw(),
                            );
                            let tex = ui.ctx().load_texture(
                                format!("tr-{}", region.id.0),
                                img,
                                egui::TextureOptions::LINEAR,
                            );
                            self.patches.insert(region.id, CachedPatch { key, tex });
                        }
                        if let Some(c) = self.patches.get(&region.id) {
                            painter.image(
                                c.tex.id(),
                                scr,
                                ERect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                            drew_overlay = true;
                        }
                    }
                }

                // A region the idle pointer is over (a press would grab it) gets a
                // brighter, heavier outline so accidental grabs are obvious.
                let is_hover_body = matches!(hover, Hover::Body(hid) if hid == region.id);
                let (stroke_color, fill, stroke_w) = if is_sel {
                    (ACCENT, ACCENT_FILL, 2.0)
                } else if is_hover_body {
                    (HOVER, REGION_FILL, 2.0)
                } else {
                    (REGION_STROKE, REGION_FILL, 1.5)
                };
                if !drew_overlay {
                    painter.rect_filled(scr, 2.0, fill);
                }
                painter.rect_stroke(
                    scr,
                    2.0,
                    Stroke::new(stroke_w, stroke_color),
                    egui::StrokeKind::Inside,
                );
                if is_sel {
                    let corners =
                        [scr.left_top(), scr.right_top(), scr.left_bottom(), scr.right_bottom()];
                    for (i, corner) in corners.iter().enumerate() {
                        // Enlarge + recolor the handle under the pointer.
                        let on = matches!(hover, Hover::Handle(hid, hi) if hid == region.id && hi == i);
                        let h = ERect::from_center_size(*corner, Vec2::splat(if on { HANDLE * 1.6 } else { HANDLE }));
                        painter.rect_filled(h, 1.0, if on { HOVER } else { ACCENT });
                    }
                }
            }
            if let Drag::Drawing { start, current } = self.drag {
                let scr = ERect::from_two_pos(to_screen(start), to_screen(current));
                painter.rect_stroke(scr, 0.0, Stroke::new(1.5, ACCENT), egui::StrokeKind::Inside);
            }

            // Drop cached patches whose regions are gone (deleted/cleared), so the
            // texture memory is released.
            self.patches.retain(|id, _| results.contains_key(id));
        });
        // Mirror the (possibly clamped) offset so next frame's zoom math starts
        // from where the view actually is — including manual scrolls and pans.
        self.scroll_offset = out.state.offset;

        edited
    }
}

/// Current render scale (pixels per point) of a page's bitmap.
fn page_scale(doc: &Document, page: usize) -> f32 {
    doc.pages.get(page).map_or(1.0, |p| {
        if p.size.0 > 0.0 {
            p.bitmap.width() as f32 / p.size.0
        } else {
            1.0
        }
    })
}

/// Target render scale (px/pt) for the given on-screen zoom and device
/// pixels-per-point, clamped and quantized to 0.5 steps so zoom scrolling doesn't
/// re-render every frame.
fn render_scale_for(zoom: f32, pixels_per_point: f32) -> f32 {
    let raw = (zoom * pixels_per_point).clamp(MIN_RENDER_SCALE, MAX_RENDER_SCALE);
    (raw * 2.0).round() / 2.0
}

/// Normalize two page-space corners into a non-negative-size [`Rect`].
fn norm_rect(a: Pos2, b: Pos2) -> Rect {
    Rect {
        x: a.x.min(b.x),
        y: a.y.min(b.y),
        w: (a.x - b.x).abs(),
        h: (a.y - b.y).abs(),
    }
}

fn point_in(p: Pos2, r: &Rect) -> bool {
    p.x >= r.x && p.x <= r.x + r.w && p.y >= r.y && p.y <= r.y + r.h
}

/// Topmost region (last drawn) containing `p`, if any.
fn topmost_at(doc: &Document, page: usize, p: Pos2) -> Option<RegionId> {
    doc.regions
        .iter()
        .rev()
        .find(|r| r.page == page && point_in(p, &r.rect))
        .map(|r| r.id)
}

/// The four corners of a region, in `Hover::Handle` index order (TL, TR, BL, BR).
fn region_corners(r: &Rect) -> [Pos2; 4] {
    [
        egui::pos2(r.x, r.y),
        egui::pos2(r.x + r.w, r.y),
        egui::pos2(r.x, r.y + r.h),
        egui::pos2(r.x + r.w, r.y + r.h),
    ]
}

/// What page-point `pp` is over: a corner handle of the selected region, a region
/// body, or nothing. This is the single hit-test behind both the hover highlight
/// and [`decide_drag`], so the highlight always matches what a press would do.
fn hover_target(
    doc: &Document,
    page: usize,
    selected: Option<RegionId>,
    pp: Pos2,
    scale: f32,
) -> Hover {
    let grab = HANDLE / scale.max(f32::EPSILON); // grab tolerance in page points

    // Resize handle: a corner of the currently selected region takes priority.
    if let Some(id) = selected
        && let Some(r) = doc.regions.iter().find(|r| r.id == id && r.page == page)
    {
        for (i, c) in region_corners(&r.rect).iter().enumerate() {
            if (pp.x - c.x).abs() <= grab && (pp.y - c.y).abs() <= grab {
                return Hover::Handle(id, i);
            }
        }
    }

    // Body: the topmost region under the pointer.
    if let Some(r) = doc
        .regions
        .iter()
        .rev()
        .find(|r| r.page == page && point_in(pp, &r.rect))
    {
        return Hover::Body(r.id);
    }

    Hover::None
}

/// Decide what a press at page-point `pp` begins: resizing a corner of the
/// selected region, moving a region under the pointer, or drawing a new one.
/// Mirrors [`hover_target`]'s hit-test exactly.
fn decide_drag(
    doc: &Document,
    page: usize,
    selected: Option<RegionId>,
    pp: Pos2,
    scale: f32,
) -> Drag {
    match hover_target(doc, page, selected, pp, scale) {
        Hover::Handle(id, i) => {
            // The drag's anchor is the corner opposite the grabbed one.
            const OPPOSITE: [usize; 4] = [3, 2, 1, 0];
            let r = doc
                .regions
                .iter()
                .find(|r| r.id == id)
                .expect("hover_target returned a live region");
            Drag::Resizing { id, fixed: region_corners(&r.rect)[OPPOSITE[i]] }
        }
        Hover::Body(id) => {
            let r = doc.regions.iter().find(|r| r.id == id).expect("live region");
            Drag::Moving { id, grab: egui::vec2(pp.x - r.rect.x, pp.y - r.rect.y) }
        }
        Hover::None => Drag::Drawing { start: pp, current: pp },
    }
}
