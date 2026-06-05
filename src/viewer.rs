//! Interactive page canvas: shows the current page and lets the user draw,
//! select, move, resize, and delete translation regions over it.
//!
//! This is the seed of the eventual shared `junk-libs-egui-pdfview` widget, so
//! it stays self-contained: it takes the `Document` and the current selection,
//! and owns only its texture + in-progress drag. All geometry is kept in page
//! points; the page↔screen transform is derived from the displayed image rect
//! each frame, so it stays correct when zoom is added later.

use egui::{Color32, Pos2, Rect as ERect, Sense, Stroke, Vec2};

use crate::document::{Document, Rect, RegionId};

/// Screen-space size of a corner resize handle, and the grab tolerance.
const HANDLE: f32 = 7.0;
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

#[derive(Default)]
pub struct Viewer {
    texture: Option<egui::TextureHandle>,
    /// Which page `texture` holds, so we rebuild it on page changes.
    texture_page: usize,
    drag: Drag,
}

impl Viewer {
    /// Invalidate the cached texture (e.g. when a new document is opened).
    pub fn reset(&mut self) {
        self.texture = None;
        self.drag = Drag::Idle;
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        doc: &mut Document,
        page_index: usize,
        selected: &mut Option<RegionId>,
    ) {
        // (Re)build the page texture when the page changed or it's missing.
        if self.texture.is_none() || self.texture_page != page_index {
            if let Some(page) = doc.pages.get(page_index) {
                let size = [page.bitmap.width() as usize, page.bitmap.height() as usize];
                let img = egui::ColorImage::from_rgba_unmultiplied(size, page.bitmap.as_raw());
                self.texture = Some(ui.ctx().load_texture("page", img, egui::TextureOptions::LINEAR));
                self.texture_page = page_index;
            } else {
                self.texture = None;
            }
        }

        let (Some(texture), Some(page)) = (self.texture.as_ref(), doc.pages.get(page_index)) else {
            ui.centered_and_justified(|ui| {
                ui.label("Open a PDF or image to begin.");
            });
            return;
        };
        let tex_id = texture.id();
        let page_size = egui::vec2(page.size.0, page.size.1);

        egui::ScrollArea::both().show(ui, |ui| {
            // Display the page at its native point size (≈1 page point per UI
            // point); the rendered bitmap is higher-res, so it stays crisp.
            let resp = ui.add(
                egui::Image::new(egui::load::SizedTexture::new(tex_id, page_size))
                    .fit_to_exact_size(page_size)
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
            if resp.drag_started() {
                if let Some(ptr) = resp.interact_pointer_pos() {
                    self.drag = decide_drag(doc, page_index, *selected, to_page(ptr), scale);
                    match self.drag {
                        Drag::Moving { id, .. } | Drag::Resizing { id, .. } => *selected = Some(id),
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
                            }
                        }
                        Drag::Resizing { id, fixed } => {
                            if let Some(r) = doc.region_mut(*id) {
                                r.rect = norm_rect(*fixed, pp);
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
                        *selected = Some(doc.add_region(page_index, r));
                    }
                }
                self.drag = Drag::Idle;
            }
            if resp.clicked() {
                if let Some(ptr) = resp.interact_pointer_pos() {
                    *selected = topmost_at(doc, page_index, to_page(ptr));
                }
            }
            if let Some(id) = *selected {
                let del = ui.input(|i| {
                    i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)
                });
                if del {
                    doc.remove_region(id);
                    *selected = None;
                }
            }

            // --- paint ---------------------------------------------------------
            let painter = ui.painter_at(rect.expand(HANDLE));
            for region in doc.regions.iter().filter(|r| r.page == page_index) {
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
                    for corner in [scr.left_top(), scr.right_top(), scr.left_bottom(), scr.right_bottom()]
                    {
                        let h = ERect::from_center_size(corner, Vec2::splat(HANDLE));
                        painter.rect_filled(h, 1.0, ACCENT);
                    }
                }
            }
            // Rubber-band preview for a region being drawn.
            if let Drag::Drawing { start, current } = self.drag {
                let scr = ERect::from_two_pos(to_screen(start), to_screen(current));
                painter.rect_stroke(scr, 0.0, Stroke::new(1.5, ACCENT), egui::StrokeKind::Inside);
            }
        });
    }
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
        // Index of the diagonally opposite corner for each corner above.
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
