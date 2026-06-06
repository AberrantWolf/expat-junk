//! Interactive page canvas: a zoomable, pageable view of the document over which
//! the user draws, selects, moves, resizes, and deletes translation regions.
//!
//! This is the seed of the eventual shared `junk-libs-egui-pdfview` widget, so it
//! stays self-contained: it takes the `Document`, the current page, and the
//! selection, and owns only its texture, zoom, and in-progress drag. All geometry
//! is kept in page points; the page↔screen transform is derived from the
//! displayed image rect each frame, so it's correct at any zoom.

use egui::{Color32, Pos2, Rect as ERect, Sense, Stroke, Vec2};

use crate::document::{Document, Rect, RegionId};

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
    drag: Drag,
}

impl Default for Viewer {
    fn default() -> Self {
        Self {
            texture: None,
            texture_page: 0,
            texture_scale: 0.0,
            zoom: 1.0,
            fit: true,
            drag: Drag::Idle,
        }
    }
}

impl Viewer {
    /// Invalidate cached state (e.g. when a new document is opened).
    pub fn reset(&mut self) {
        self.texture = None;
        self.texture_scale = 0.0;
        self.drag = Drag::Idle;
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
    ) -> bool {
        let total_pages = doc.pages.len();
        let mut edited = false;

        // --- control bar: zoom + page navigation ---------------------------
        ui.horizontal(|ui| {
            if ui.button("−").on_hover_text("Zoom out").clicked() {
                self.zoom = (self.zoom / ZOOM_STEP).max(MIN_ZOOM);
                self.fit = false;
            }
            ui.label(format!("{:.0}%", self.zoom * 100.0));
            if ui.button("+").on_hover_text("Zoom in").clicked() {
                self.zoom = (self.zoom * ZOOM_STEP).min(MAX_ZOOM);
                self.fit = false;
            }
            if ui.selectable_label(self.fit, "Fit width").clicked() {
                self.fit = true;
            }

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
        let cmd_scroll = ui.input(|i| {
            if i.modifiers.command {
                i.smooth_scroll_delta.y
            } else {
                0.0
            }
        });
        if cmd_scroll.abs() > 0.1 {
            self.zoom = (self.zoom * (1.0 + cmd_scroll * 0.0015)).clamp(MIN_ZOOM, MAX_ZOOM);
            self.fit = false;
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

        egui::ScrollArea::both().show(ui, |ui| {
            let resp = ui.add(
                egui::Image::new(egui::load::SizedTexture::new(tex_id, display))
                    .fit_to_exact_size(display)
                    .sense(Sense::click_and_drag()),
            );
            let rect = resp.rect;
            let scale = if page_size.x > 0.0 {
                rect.width() / page_size.x
            } else {
                1.0
            };
            let to_screen = |p: Pos2| rect.min + p.to_vec2() * scale;
            let to_page = |s: Pos2| ((s - rect.min) / scale).to_pos2();

            // --- interaction ---------------------------------------------------
            // Start from the *press origin*, not interact_pointer_pos: the latter
            // is only sampled once the drag threshold is crossed, which offsets
            // the start by a few pixels from where the user actually clicked.
            if resp.drag_started() {
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
            if resp.dragged() {
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
            if resp.drag_stopped() {
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
            if let Some(id) = *selected {
                let del = ui.input(|i| {
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
            for region in doc.regions.iter().filter(|r| r.page == *current_page) {
                let scr = ERect::from_min_size(
                    to_screen(egui::pos2(region.rect.x, region.rect.y)),
                    egui::vec2(region.rect.w * scale, region.rect.h * scale),
                );
                let is_sel = *selected == Some(region.id);
                let (stroke_color, fill) = if is_sel {
                    (ACCENT, ACCENT_FILL)
                } else {
                    (REGION_STROKE, REGION_FILL)
                };
                painter.rect_filled(scr, 2.0, fill);
                painter.rect_stroke(
                    scr,
                    2.0,
                    Stroke::new(if is_sel { 2.0 } else { 1.5 }, stroke_color),
                    egui::StrokeKind::Inside,
                );
                if is_sel {
                    for corner in
                        [scr.left_top(), scr.right_top(), scr.left_bottom(), scr.right_bottom()]
                    {
                        let h = ERect::from_center_size(corner, Vec2::splat(HANDLE));
                        painter.rect_filled(h, 1.0, ACCENT);
                    }
                }
            }
            if let Drag::Drawing { start, current } = self.drag {
                let scr = ERect::from_two_pos(to_screen(start), to_screen(current));
                painter.rect_stroke(scr, 0.0, Stroke::new(1.5, ACCENT), egui::StrokeKind::Inside);
            }
        });

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

/// Decide what a press at page-point `pp` begins: resizing a corner of the
/// selected region, moving a region under the pointer, or drawing a new one.
fn decide_drag(
    doc: &Document,
    page: usize,
    selected: Option<RegionId>,
    pp: Pos2,
    scale: f32,
) -> Drag {
    let grab = HANDLE / scale.max(f32::EPSILON); // grab tolerance in page points

    // Resize: a corner of the currently selected region.
    if let Some(id) = selected
        && let Some(r) = doc.regions.iter().find(|r| r.id == id && r.page == page)
    {
        let corners = [
            egui::pos2(r.rect.x, r.rect.y),
            egui::pos2(r.rect.x + r.rect.w, r.rect.y),
            egui::pos2(r.rect.x, r.rect.y + r.rect.h),
            egui::pos2(r.rect.x + r.rect.w, r.rect.y + r.rect.h),
        ];
        const OPPOSITE: [usize; 4] = [3, 2, 1, 0];
        for (i, c) in corners.iter().enumerate() {
            if (pp.x - c.x).abs() <= grab && (pp.y - c.y).abs() <= grab {
                return Drag::Resizing {
                    id,
                    fixed: corners[OPPOSITE[i]],
                };
            }
        }
    }

    // Move: the topmost region whose body is under the pointer.
    if let Some(r) = doc
        .regions
        .iter()
        .rev()
        .find(|r| r.page == page && point_in(pp, &r.rect))
    {
        return Drag::Moving {
            id: r.id,
            grab: egui::vec2(pp.x - r.rect.x, pp.y - r.rect.y),
        };
    }

    // Otherwise: start drawing a new region.
    Drag::Drawing {
        start: pp,
        current: pp,
    }
}
