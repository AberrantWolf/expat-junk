//! The egui application shell: window layout and interaction state.
//!
//! This is the only module that wires the UI together. It owns transient state
//! (current document, page, selection, cached translations) and delegates the
//! page canvas to [`crate::viewer::Viewer`]. Three areas:
//!   - top bar: open file, target language, translate, export, page nav
//!   - central canvas: the rendered page with draggable/resizable regions
//!   - right panel: info for the selected region (source, target, alternatives)

use std::collections::HashMap;

use crate::config::Settings;
use crate::document::{Document, RegionId};
use crate::translate::TranslationResult;
use crate::viewer::Viewer;

pub struct App {
    settings: Settings,
    document: Option<Document>,
    /// Index of the page currently shown on the canvas.
    current_page: usize,
    selected: Option<RegionId>,
    /// Translations keyed by region, shown in the info panel and used on export.
    results: HashMap<RegionId, TranslationResult>,
    /// Most recent error, shown in the top bar.
    error: Option<String>,
    viewer: Viewer,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            settings: Settings::load().unwrap_or_default(),
            document: None,
            current_page: 0,
            selected: None,
            results: HashMap::new(),
            error: None,
            viewer: Viewer::default(),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Disjoint field borrows so the panels and the viewer can run together.
        let App {
            settings,
            document,
            current_page,
            selected,
            results,
            error,
            viewer,
        } = self;

        egui::Panel::top("toolbar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Documents", &["pdf", "png", "jpg", "jpeg", "webp", "tiff"])
                        .pick_file()
                {
                    match Document::open(&path) {
                        Ok(doc) => {
                            *document = Some(doc);
                            *current_page = 0;
                            *selected = None;
                            results.clear();
                            *error = None;
                            viewer.reset();
                        }
                        Err(e) => *error = Some(format!("{e:#}")),
                    }
                }
                ui.separator();
                ui.label(format!("Target: {}", settings.target_lang));
                let _ = ui.button("Translate selection"); // TODO: backend call
                let _ = ui.button("Export…"); // TODO: export::export
                if let Some(err) = error {
                    ui.separator();
                    ui.colored_label(egui::Color32::RED, err.as_str());
                }
            });

            // Page navigation for multi-page documents.
            if let Some(doc) = document.as_ref()
                && doc.pages.len() > 1
            {
                let total = doc.pages.len();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(*current_page > 0, egui::Button::new("◀ Prev"))
                        .clicked()
                    {
                        *current_page -= 1;
                    }
                    ui.label(format!("Page {} / {}", *current_page + 1, total));
                    if ui
                        .add_enabled(*current_page + 1 < total, egui::Button::new("Next ▶"))
                        .clicked()
                    {
                        *current_page += 1;
                    }
                });
            }
        });

        egui::Panel::right("info").show_inside(ui, |ui| {
            ui.heading("Region");
            match *selected {
                Some(id) => {
                    if let Some(r) = results.get(&id) {
                        ui.label("Source");
                        ui.monospace(&r.source_text);
                        ui.separator();
                        ui.label("Translation");
                        ui.monospace(&r.translation);
                        if !r.alternatives.is_empty() {
                            ui.separator();
                            ui.label("Alternatives");
                            for alt in &r.alternatives {
                                ui.monospace(alt);
                            }
                        }
                        if let Some(expl) = &r.explanation {
                            ui.separator();
                            ui.label("Notes");
                            ui.label(expl);
                        }
                    } else if let Some(reg) =
                        document.as_ref().and_then(|d| d.regions.iter().find(|r| r.id == id))
                    {
                        ui.label("Not translated yet.");
                        ui.add_space(4.0);
                        ui.monospace(format!("page {}", reg.page + 1));
                        ui.monospace(format!("x {:.0}  y {:.0} pt", reg.rect.x, reg.rect.y));
                        ui.monospace(format!("w {:.0}  h {:.0} pt", reg.rect.w, reg.rect.h));
                        ui.add_space(4.0);
                        ui.label("Drag a corner to resize, the body to move, Delete to remove.");
                    }
                }
                None => {
                    ui.label("Drag on the page to draw a region, or click one to select it.");
                }
            }
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            if let Some(doc) = document.as_mut() {
                viewer.show(ui, doc, *current_page, selected);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open a PDF or image to begin.");
                });
            }
        });
    }
}
