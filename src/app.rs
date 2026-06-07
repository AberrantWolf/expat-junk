//! The egui application shell: window layout, menus, and session state.
//!
//! This module wires the UI together and owns the session: the open document and
//! its source path, the current project file + dirty flag, the selection, cached
//! translations, and the in-flight request. The page canvas is delegated to
//! the `junk-libs-egui-docview` widget ([`DocView`]), with translations painted
//! over it by [`crate::viewer::TranslationOverlay`]. Menu/toolbar clicks are
//! collected as an [`Action`] and executed after the panels render, which keeps
//! borrows simple.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::Result;

use crate::cache::{self, TranslationCache};
use crate::config::{Backend, Settings};
use crate::document::{Document, RegionId};
use crate::export;
use crate::ocr;
use crate::project;
use crate::secrets::Secrets;
use crate::translate::{
    GeminiVision, TesseractText, TextApi, TranslationRequest, TranslationResult, Translator,
};
use crate::viewer::TranslationOverlay;
use junk_libs_egui_docview::DocView;

/// How a region's source text was obtained — shown as a badge so the user knows
/// what is being sent to translate.
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SourceMethod {
    /// Pulled from the PDF text layer (`char_boxes`).
    TextLayer,
    /// Recognized locally with Tesseract (no text layer).
    Ocr,
    /// No text available; only the image was sent (the vision model reads it).
    ImageOnly,
}

impl SourceMethod {
    fn label(self) -> &'static str {
        match self {
            SourceMethod::TextLayer => "PDF text layer",
            SourceMethod::Ocr => "OCR",
            SourceMethod::ImageOnly => "image (vision)",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            SourceMethod::TextLayer => egui::Color32::from_rgb(0x4c, 0xaf, 0x50),
            SourceMethod::Ocr => egui::Color32::from_rgb(0x4e, 0x9a, 0xff),
            SourceMethod::ImageOnly => egui::Color32::GRAY,
        }
    }
}

/// Horizontal alignment for the overlaid translation text.
#[derive(Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Justify {
    #[default]
    Left,
    Center,
    Right,
}

/// 90° rotation for the overlaid text. English documents use both conventions:
/// `Cw` (reads top-to-bottom, head tilted right) is the margin/spine/label norm
/// and the default when rotating; `Ccw` (reads bottom-to-top, head tilted left)
/// is the chart axis-label norm.
#[derive(Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Rotate {
    #[default]
    None,
    /// Clockwise — text reads top-to-bottom.
    Cw,
    /// Counter-clockwise — text reads bottom-to-top.
    Ccw,
}

impl Rotate {
    /// Whether the text is turned on its side (so the render buffer's width and
    /// height are swapped before it's rotated back to fill the region).
    pub fn is_sideways(self) -> bool {
        !matches!(self, Rotate::None)
    }
}

/// Per-region presentation overrides the user can tweak after translating. These
/// persist with the project and drive both the live preview and the export, so
/// what you set here is what gets baked into the output.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RegionStyle {
    /// User-edited replacement text. `None` means "use the machine translation"
    /// (and keeps tracking it if the region is re-translated).
    pub text: Option<String>,
    /// Render the text rotated 90° (for vertical/sideways layouts).
    pub rotate: Rotate,
    /// Background fill override (sRGB). `None` auto-samples the page around the
    /// region, matching the export's default background fill.
    pub background: Option<[u8; 3]>,
    pub justify: Justify,
    /// Render the text in bold (700 weight) instead of regular (400).
    #[serde(default)]
    pub bold: bool,
    /// Explicit font size in points. `None` auto-fits the largest size (up to the
    /// region height) that fits the box; `Some` sets the starting size, still
    /// shrunk to fit if the text would overflow.
    #[serde(default)]
    pub size_pt: Option<f32>,
}

/// A completed translation plus how its source text was obtained and any
/// user presentation overrides.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Translated {
    pub result: TranslationResult,
    pub method: SourceMethod,
    /// User tweaks to how the translation is rendered. `#[serde(default)]` keeps
    /// projects saved before styling existed loadable.
    #[serde(default)]
    pub style: RegionStyle,
}

impl Translated {
    /// The text to render: the user's edit if present, else the machine
    /// translation.
    pub fn display_text(&self) -> &str {
        self.style
            .text
            .as_deref()
            .unwrap_or(&self.result.translation)
    }
}

/// An in-flight translation running on a background thread.
struct Pending {
    region: RegionId,
    rx: mpsc::Receiver<Result<Translated>>,
}

/// A menu/toolbar/shortcut action, executed after the panels render.
enum Action {
    OpenDoc,
    OpenProject,
    SaveProject,
    SaveProjectAs,
    Export,
    Quit,
    Translate,
    TranslateAll,
    DeleteSelected,
    ClearTranslations,
    ToggleSettings,
}

pub struct App {
    settings: Settings,
    document: Option<Document>,
    /// Path of the open source document (for project save / export defaults).
    source_path: Option<PathBuf>,
    /// Path of the current project file, if saved or loaded.
    project_path: Option<PathBuf>,
    /// Unsaved changes since the last save/open.
    dirty: bool,
    current_page: usize,
    selected: Option<RegionId>,
    /// Completed translations keyed by region, for the info panel and export.
    results: HashMap<RegionId, Translated>,
    /// Persistent cache so repeat translations are free. Shared with the worker.
    cache: Arc<Mutex<TranslationCache>>,
    pending: Option<Pending>,
    /// Regions queued for batch translation ("Translate all"), processed one at
    /// a time through `pending`.
    queue: VecDeque<RegionId>,
    error: Option<String>,
    /// The shared interactive page-view widget (zoom/pan/region editing).
    doc_view: DocView,
    /// Translation-overlay state (fonts + per-region patch cache) painted onto the
    /// page by the widget.
    overlay: TranslationOverlay,
    /// Whether the translation overlay is shown over the page (toggled in the
    /// viewer's control bar). Persists across document opens.
    show_translations: bool,
    secrets: Secrets,
    show_settings: bool,
    api_key_input: String,
    /// Last window title set, to avoid resending it every frame.
    title: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::fonts::install(&cc.egui_ctx);
        Self {
            settings: Settings::load(),
            document: None,
            source_path: None,
            project_path: None,
            dirty: false,
            current_page: 0,
            selected: None,
            results: HashMap::new(),
            cache: Arc::new(Mutex::new(TranslationCache::load())),
            pending: None,
            queue: VecDeque::new(),
            error: None,
            doc_view: DocView::default(),
            overlay: TranslationOverlay::default(),
            show_translations: true,
            secrets: Secrets::load(),
            show_settings: false,
            api_key_input: String::new(),
            title: String::new(),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        let App {
            settings,
            document,
            source_path,
            project_path,
            dirty,
            current_page,
            selected,
            results,
            cache,
            pending,
            queue,
            error,
            doc_view,
            overlay,
            show_translations,
            secrets,
            show_settings,
            api_key_input,
            title,
        } = self;

        // --- collect a finished translation, if any --------------------------
        let mut clear_pending = false;
        if let Some(p) = pending.as_mut() {
            match p.rx.try_recv() {
                Ok(Ok(mut translated)) => {
                    // Re-translating a region keeps the user's presentation
                    // overrides (rotation, background, alignment, edited text).
                    if let Some(prev) = results.get(&p.region) {
                        translated.style = prev.style.clone();
                    }
                    results.insert(p.region, translated);
                    *dirty = true;
                    clear_pending = true;
                }
                Ok(Err(e)) => {
                    *error = Some(format!("{e:#}"));
                    queue.clear(); // abort any batch rather than repeat the failure
                    clear_pending = true;
                }
                Err(mpsc::TryRecvError::Empty) => ctx.request_repaint(),
                Err(mpsc::TryRecvError::Disconnected) => {
                    *error = Some("translation worker stopped unexpectedly".to_string());
                    queue.clear();
                    clear_pending = true;
                }
            }
        }
        if clear_pending {
            *pending = None;
        }

        let mut action: Option<Action> = None;

        // Count regions that still need translating (for the batch menu item).
        let untranslated = document
            .as_ref()
            .map(|d| {
                d.regions
                    .iter()
                    .filter(|r| !results.contains_key(&r.id))
                    .count()
            })
            .unwrap_or(0);

        // --- top panel: menu bar + toolbar -----------------------------------
        egui::Panel::top("topbar").show_inside(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open document…").clicked() {
                        action = Some(Action::OpenDoc);
                        ui.close();
                    }
                    if ui.button("Open project…").clicked() {
                        action = Some(Action::OpenProject);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Save project").clicked() {
                        action = Some(Action::SaveProject);
                        ui.close();
                    }
                    if ui.button("Save project as…").clicked() {
                        action = Some(Action::SaveProjectAs);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(document.is_some(), egui::Button::new("Export translated…"))
                        .clicked()
                    {
                        action = Some(Action::Export);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        action = Some(Action::Quit);
                        ui.close();
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if ui
                        .add_enabled(
                            untranslated > 0 && pending.is_none(),
                            egui::Button::new(format!("Translate all ({untranslated})")),
                        )
                        .clicked()
                    {
                        action = Some(Action::TranslateAll);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            selected.is_some(),
                            egui::Button::new("Delete selected region"),
                        )
                        .clicked()
                    {
                        action = Some(Action::DeleteSelected);
                        ui.close();
                    }
                    if ui
                        .add_enabled(!results.is_empty(), egui::Button::new("Clear translations"))
                        .clicked()
                    {
                        action = Some(Action::ClearTranslations);
                        ui.close();
                    }
                });
                if ui.button("Settings").clicked() {
                    action = Some(Action::ToggleSettings);
                }
            });
        });

        // --- right panel: languages + selected region info -------------------
        // Translation controls (language pickers, the Translate button) live here
        // so the bar above the document holds only page/zoom (PDF) controls.
        action = action.or(info_panel(
            ui, settings, document, results, pending, *selected, dirty, error,
        ));

        // --- central canvas --------------------------------------------------
        egui::CentralPanel::default().show_inside(ui, |ui| {
            if let Some(doc) = document.as_mut() {
                let mut frame = overlay.frame(results, *show_translations);
                let edited = doc_view.show(ui, doc, current_page, selected, &mut frame, |ui| {
                    // Host control: the translation-overlay toggle, in the viewer bar.
                    ui.separator();
                    ui.add_enabled_ui(!results.is_empty(), |ui| {
                        if ui
                            .selectable_label(*show_translations, "Translations")
                            .on_hover_text(
                                "Overlay translated text on the page (preview of the export)",
                            )
                            .clicked()
                        {
                            *show_translations = !*show_translations;
                        }
                    });
                });
                // `frame`'s borrow of `overlay` ends here (last used above), so we
                // can prune the patch cache for regions whose translations are gone.
                overlay.prune(results);
                if edited {
                    *dirty = true;
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open a PDF or image to begin.");
                });
            }
        });

        settings_window(&ctx, show_settings, settings, secrets, api_key_input);

        // Keyboard shortcuts (only if no menu/toolbar action already fired).
        if action.is_none() {
            action = ctx.input(|i| {
                if i.modifiers.command && i.key_pressed(egui::Key::S) {
                    Some(Action::SaveProject)
                } else if i.modifiers.command && i.key_pressed(egui::Key::O) {
                    Some(Action::OpenDoc)
                } else if i.modifiers.command && i.key_pressed(egui::Key::E) {
                    Some(Action::Export)
                } else {
                    None
                }
            });
        }

        // --- execute the action ----------------------------------------------
        match action {
            None => {}
            Some(Action::OpenDoc) => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Documents", &["pdf", "png", "jpg", "jpeg", "webp", "tiff"])
                    .pick_file()
                {
                    match Document::open(&path) {
                        Ok(doc) => {
                            *document = Some(doc);
                            *source_path = Some(path);
                            *project_path = None;
                            *current_page = 0;
                            *selected = None;
                            results.clear();
                            *error = None;
                            *pending = None;
                            *dirty = false;
                            doc_view.reset();
                            overlay.reset();
                        }
                        Err(e) => *error = Some(format!("{e:#}")),
                    }
                }
            }
            Some(Action::OpenProject) => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("expat-junk project", &[project::EXTENSION])
                    .pick_file()
                {
                    let loaded = project::Project::load(&path)
                        .and_then(|p| project::open_document(&p).map(|d| (p, d)));
                    match loaded {
                        Ok((proj, doc)) => {
                            let last = doc.pages.len().saturating_sub(1);
                            *current_page = proj.current_page.min(last);
                            *document = Some(doc);
                            *source_path = Some(proj.source_path);
                            *project_path = Some(path);
                            *selected = None;
                            *results = proj.results;
                            settings.source_lang = proj.source_lang;
                            settings.target_lang = proj.target_lang;
                            settings.backend = proj.backend;
                            settings.save();
                            *error = None;
                            *pending = None;
                            *dirty = false;
                            doc_view.reset();
                            overlay.reset();
                        }
                        Err(e) => *error = Some(format!("{e:#}")),
                    }
                }
            }
            Some(Action::SaveProject) => save_project(
                false,
                settings,
                source_path,
                document,
                *current_page,
                results,
                project_path,
                dirty,
                error,
            ),
            Some(Action::SaveProjectAs) => save_project(
                true,
                settings,
                source_path,
                document,
                *current_page,
                results,
                project_path,
                dirty,
                error,
            ),
            Some(Action::Export) => {
                if let (Some(doc), Some(src)) = (document.as_ref(), source_path.as_ref()) {
                    let default = src
                        .file_stem()
                        .map(|s| format!("{}-translated.pdf", s.to_string_lossy()))
                        .unwrap_or_else(|| "translated.pdf".to_string());
                    if let Some(mut path) = rfd::FileDialog::new()
                        .add_filter("PDF", &["pdf"])
                        .set_file_name(default)
                        .save_file()
                    {
                        if path.extension().is_none() {
                            path.set_extension("pdf");
                        }
                        match export::export(doc, src, results, &path) {
                            Ok(()) => *error = None,
                            Err(e) => *error = Some(format!("{e:#}")),
                        }
                    }
                }
            }
            Some(Action::Quit) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Some(Action::Translate) => {
                if let Some(id) = *selected {
                    start_translation(id, settings, document, secrets, cache, pending, error);
                }
            }
            Some(Action::TranslateAll) => {
                queue.clear();
                if let Some(doc) = document.as_ref() {
                    for r in &doc.regions {
                        if !results.contains_key(&r.id) {
                            queue.push_back(r.id);
                        }
                    }
                }
            }
            Some(Action::DeleteSelected) => {
                if let (Some(id), Some(doc)) = (*selected, document.as_mut()) {
                    doc.remove_region(id);
                    results.remove(&id);
                    *selected = None;
                    *dirty = true;
                }
            }
            Some(Action::ClearTranslations) => {
                *dirty = *dirty || !results.is_empty();
                results.clear();
            }
            Some(Action::ToggleSettings) => *show_settings = !*show_settings,
        }

        // --- drive the batch queue: start the next region when idle ----------
        if pending.is_none() {
            while let Some(next) = queue.pop_front() {
                let still_valid = document
                    .as_ref()
                    .is_some_and(|d| d.regions.iter().any(|r| r.id == next))
                    && !results.contains_key(&next);
                if still_valid {
                    start_translation(next, settings, document, secrets, cache, pending, error);
                    // If it couldn't even start (e.g. no API key), abort the batch.
                    if pending.is_none() {
                        queue.clear();
                    }
                    break;
                }
            }
        }
        if pending.is_some() || !queue.is_empty() {
            ctx.request_repaint();
        }

        // --- window title reflects the project + dirty state -----------------
        let name = project_path
            .as_ref()
            .or(source_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned());
        let new_title = match name {
            Some(n) => format!("expat-junk — {n}{}", if *dirty { " *" } else { "" }),
            None => "expat-junk".to_string(),
        };
        if *title != new_title {
            *title = new_title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(new_title));
        }
    }
}

/// Right-hand info panel for the selected region.
#[allow(clippy::too_many_arguments)]
fn info_panel(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    document: &Option<Document>,
    results: &mut HashMap<RegionId, Translated>,
    pending: &Option<Pending>,
    selected: Option<RegionId>,
    dirty: &mut bool,
    error: &Option<String>,
) -> Option<Action> {
    let mut action = None;
    egui::Panel::right("info").show_inside(ui, |ui| {
        // Global translation status + language pickers (apply to every region).
        if pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Translating…");
            });
        }
        if let Some(err) = error {
            ui.colored_label(egui::Color32::RED, err.as_str());
        }
        ui.heading("Languages");
        // Display names are computed up front so the immutable borrow they need
        // doesn't overlap the `&mut` the combo takes on the same field.
        let src_name = crate::config::Language::name_for_ocr(&settings.source_lang).to_owned();
        if language_combo(ui, "Source (OCR)", "src_lang", &mut settings.source_lang, &src_name, |l| l.ocr) {
            settings.save();
        }
        let tgt_name = crate::config::Language::name_for_iso(&settings.target_lang).to_owned();
        if language_combo(ui, "Target", "tgt_lang", &mut settings.target_lang, &tgt_name, |l| l.iso) {
            settings.save();
        }
        ui.separator();

        ui.heading("Region");
        let can_translate = document.is_some() && pending.is_none();
        let translating = pending.as_ref().map(|p| p.region) == selected && pending.is_some();
        match selected {
            Some(id) if results.contains_key(&id) => {
                let t = results.get_mut(&id).expect("checked by guard");
                badge(ui, t.method);
                ui.separator();

                // Reference text: the source and the raw machine translation.
                // Read-only but selectable, so the user can copy without editing.
                reference_field(ui, "Source", &t.result.source_text);
                reference_field(ui, "Machine translation", &t.result.translation);
                if ui
                    .add_enabled(can_translate, egui::Button::new("Re-translate"))
                    .on_hover_text("Translate this region again (keeps your style edits)")
                    .clicked()
                {
                    action = Some(Action::Translate);
                }
                ui.separator();

                // 1. Editable text — defaults to the machine translation.
                ui.label("Text (rendered on the page)");
                let machine = t.result.translation.clone();
                let mut text = t.style.text.clone().unwrap_or_else(|| machine.clone());
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                if resp.changed() {
                    // Collapse back to "track the machine translation" if the user
                    // typed it verbatim, so re-translation can update it.
                    t.style.text = (text != machine).then_some(text);
                    *dirty = true;
                }
                if t.style.text.is_some() && ui.small_button("Reset to machine translation").clicked()
                {
                    t.style.text = None;
                    *dirty = true;
                }
                ui.separator();

                // 2. Rotate 90° (clockwise = top-to-bottom; ccw = bottom-to-top).
                ui.horizontal(|ui| {
                    ui.label("Rotate");
                    for (r, label, hover) in [
                        (Rotate::None, "None", "Horizontal"),
                        (Rotate::Cw, "↻ CW", "Clockwise — reads top-to-bottom (margin/label)"),
                        (Rotate::Ccw, "↺ CCW", "Counter-clockwise — reads bottom-to-top (chart axis)"),
                    ] {
                        if ui
                            .selectable_label(t.style.rotate == r, label)
                            .on_hover_text(hover)
                            .clicked()
                        {
                            t.style.rotate = r;
                            *dirty = true;
                        }
                    }
                });

                // 4. Left / Center / Right justify.
                ui.horizontal(|ui| {
                    ui.label("Align");
                    for (j, label) in [
                        (Justify::Left, "Left"),
                        (Justify::Center, "Center"),
                        (Justify::Right, "Right"),
                    ] {
                        if ui.selectable_label(t.style.justify == j, label).clicked() {
                            t.style.justify = j;
                            *dirty = true;
                        }
                    }
                });

                // Weight + font size. Size is optional: unset auto-fits the box.
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut t.style.bold, "Bold").changed() {
                        *dirty = true;
                    }
                    ui.separator();
                    let mut on = t.style.size_pt.is_some();
                    if ui.checkbox(&mut on, "Font size").changed() {
                        // Seed the override from the region height (a sane visible
                        // default) so the field doesn't start at a jarring value.
                        t.style.size_pt = on.then(|| {
                            document
                                .as_ref()
                                .and_then(|d| d.regions.iter().find(|r| r.id == id))
                                .map(|r| r.rect.h.min(48.0))
                                .unwrap_or(16.0)
                        });
                        *dirty = true;
                    }
                    if let Some(size) = t.style.size_pt.as_mut() {
                        if ui
                            .add(
                                egui::DragValue::new(size)
                                    .speed(0.5)
                                    .range(4.0..=400.0)
                                    .suffix(" pt"),
                            )
                            .changed()
                        {
                            *dirty = true;
                        }
                    } else {
                        ui.weak("(auto-fit)");
                    }
                });

                // 3. Background color override.
                ui.horizontal(|ui| {
                    let mut on = t.style.background.is_some();
                    if ui.checkbox(&mut on, "Override background").changed() {
                        t.style.background = on.then_some([255, 255, 255]);
                        *dirty = true;
                    }
                    if let Some(color) = t.style.background.as_mut() {
                        if ui.color_edit_button_srgb(color).changed() {
                            *dirty = true;
                        }
                    } else {
                        ui.weak("(auto-matched to page)");
                    }
                });

                if !t.result.alternatives.is_empty() {
                    ui.separator();
                    ui.label("Alternatives");
                    for alt in &t.result.alternatives {
                        ui.monospace(alt);
                    }
                }
                if let Some(expl) = &t.result.explanation {
                    ui.separator();
                    ui.label("Notes");
                    ui.label(expl);
                }
            }
            Some(_) if translating => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Translating…");
                });
            }
            Some(id) => {
                if let Some(doc) = document.as_ref()
                    && let Some(reg) = doc.regions.iter().find(|r| r.id == id)
                {
                    let text = doc.region_text(id);
                    if !text.trim().is_empty() {
                        badge(ui, SourceMethod::TextLayer);
                        ui.separator();
                        ui.label("Detected text (sent as the source)");
                        ui.monospace(&text);
                    } else if ocr::AVAILABLE {
                        badge(ui, SourceMethod::Ocr);
                        ui.separator();
                        ui.label("No text layer — the region image will be OCR'd locally when you translate.");
                    } else {
                        badge(ui, SourceMethod::ImageOnly);
                        ui.separator();
                        ui.label("No text layer — the region image is sent to the vision model. (Build with --features ocr to OCR locally.)");
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_enabled(can_translate, egui::Button::new("Translate"))
                        .clicked()
                    {
                        action = Some(Action::Translate);
                    }
                    ui.separator();
                    ui.monospace(format!("page {}", reg.page + 1));
                    ui.monospace(format!("x {:.0}  y {:.0} pt", reg.rect.x, reg.rect.y));
                    ui.monospace(format!("w {:.0}  h {:.0} pt", reg.rect.w, reg.rect.h));
                }
            }
            None => {
                ui.label("Drag on the page to draw a region, or click one to select it.");
            }
        }
    });
    action
}

/// A language dropdown bound to a persisted code. `selected_name` is the display
/// label for the current value; `code_of` maps a [`Language`] to the code this
/// field stores (Tesseract for source, ISO for target). Returns true on change.
fn language_combo(
    ui: &mut egui::Ui,
    label: &str,
    salt: &str,
    current: &mut String,
    selected_name: &str,
    code_of: impl Fn(&crate::config::Language) -> &'static str,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(salt)
            .selected_text(selected_name)
            .show_ui(ui, |ui| {
                for lang in crate::config::LANGUAGES {
                    let code = code_of(lang);
                    if ui
                        .selectable_label(current.as_str() == code, lang.name)
                        .clicked()
                    {
                        *current = code.to_owned();
                        changed = true;
                    }
                }
            });
    });
    changed
}

/// A labelled, read-only-but-copyable text field (selectable label + a copy
/// button). Used for the source text and the reference machine translation.
fn reference_field(ui: &mut egui::Ui, label: &str, text: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        if ui.small_button("Copy").clicked() {
            ui.ctx().copy_text(text.to_owned());
        }
    });
    ui.add(egui::Label::new(egui::RichText::new(text).monospace()).selectable(true));
}

/// A small colored badge naming how the source text was/will be obtained.
fn badge(ui: &mut egui::Ui, method: SourceMethod) {
    ui.label(
        egui::RichText::new(format!("source: {}", method.label()))
            .small()
            .color(method.color()),
    );
}

/// The Settings window: pick a provider/model and store its API key locally.
fn settings_window(
    ctx: &egui::Context,
    open: &mut bool,
    settings: &mut Settings,
    secrets: &mut Secrets,
    api_key_input: &mut String,
) {
    egui::Window::new("Settings")
        .open(open)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label("Translation provider");
            egui::ComboBox::from_id_salt("provider")
                .selected_text(settings.backend.provider_label())
                .show_ui(ui, |ui| {
                    let is_gemini = matches!(settings.backend, Backend::GeminiVision { .. });
                    if ui.selectable_label(is_gemini, "Gemini (vision)").clicked() && !is_gemini {
                        settings.backend = Backend::default();
                        settings.save();
                    }
                    for (label, api, ready) in [
                        ("Google Translate", TextApi::Google, true),
                        ("DeepL", TextApi::DeepL, false),
                        ("Kagi", TextApi::Kagi, false),
                    ] {
                        let sel = matches!(&settings.backend, Backend::TesseractText { api: a } if *a == api);
                        let text = if ready {
                            label.to_string()
                        } else {
                            format!("{label} (coming soon)")
                        };
                        let resp = ui.add_enabled(ready, egui::Button::selectable(sel, text));
                        if resp.clicked() && !sel {
                            settings.backend = Backend::TesseractText { api };
                            settings.save();
                        }
                    }
                });

            let mut model_changed = false;
            if let Backend::GeminiVision { model } = &mut settings.backend {
                ui.horizontal(|ui| {
                    ui.label("Model");
                    model_changed = ui
                        .add(
                            egui::TextEdit::singleline(model).hint_text("gemini-flash-lite-latest"),
                        )
                        .changed();
                });
            }
            if model_changed {
                settings.save();
            }

            ui.separator();
            let provider = settings.backend.secret_id();
            ui.label(format!("API key — {}", settings.backend.provider_label()));
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(api_key_input)
                        .password(true)
                        .desired_width(240.0)
                        .hint_text("paste API key"),
                );
                if ui.button("Save").clicked() && !api_key_input.is_empty() {
                    secrets.set(provider, std::mem::take(api_key_input));
                }
            });
            if secrets.get(provider).is_some() {
                ui.colored_label(egui::Color32::from_rgb(0x4c, 0xaf, 0x50), "✓ a key is saved");
            } else {
                ui.colored_label(egui::Color32::GRAY, "no key saved yet");
            }
            if std::env::var("EXPAT_JUNK_API_KEY").is_ok_and(|v| !v.is_empty()) {
                ui.colored_label(
                    egui::Color32::from_rgb(0xff, 0xa5, 0x00),
                    "EXPAT_JUNK_API_KEY is set and overrides saved keys",
                );
            }
            ui.separator();
            ui.small("Keys are stored in secrets.json in your config dir (owner-only), not in git.");
        });
}

/// Save the session as a project file, prompting for a path when needed.
#[allow(clippy::too_many_arguments)]
fn save_project(
    force_dialog: bool,
    settings: &Settings,
    source_path: &Option<PathBuf>,
    document: &Option<Document>,
    current_page: usize,
    results: &HashMap<RegionId, Translated>,
    project_path: &mut Option<PathBuf>,
    dirty: &mut bool,
    error: &mut Option<String>,
) {
    let (Some(src), Some(doc)) = (source_path.as_ref(), document.as_ref()) else {
        *error = Some("open a document before saving a project".to_string());
        return;
    };

    let target = if !force_dialog && project_path.is_some() {
        project_path.clone()
    } else {
        let default = src
            .file_stem()
            .map(|s| format!("{}.{}", s.to_string_lossy(), project::EXTENSION))
            .unwrap_or_else(|| format!("untitled.{}", project::EXTENSION));
        rfd::FileDialog::new()
            .add_filter("expat-junk project", &[project::EXTENSION])
            .set_file_name(default)
            .save_file()
    };
    let Some(mut path) = target else {
        return;
    };
    if path.extension().is_none() {
        path.set_extension(project::EXTENSION);
    }

    let proj = project::build(
        src.clone(),
        current_page,
        settings.source_lang.clone(),
        settings.target_lang.clone(),
        settings.backend.clone(),
        doc,
        results.clone(),
    );
    match proj.save(&path) {
        Ok(()) => {
            *project_path = Some(path);
            *dirty = false;
            *error = None;
        }
        Err(e) => *error = Some(format!("{e:#}")),
    }
}

/// Crop the selected region and kick off a background translation. The worker
/// resolves the source text (text layer → OCR), checks/populates the shared
/// cache, and calls the translator. All errors land in `error`.
fn start_translation(
    id: RegionId,
    settings: &Settings,
    document: &Option<Document>,
    secrets: &Secrets,
    cache: &Arc<Mutex<TranslationCache>>,
    pending: &mut Option<Pending>,
    error: &mut Option<String>,
) {
    let Some(doc) = document.as_ref() else {
        return;
    };
    let crop = match doc.crop(id) {
        Ok(c) => c,
        Err(e) => {
            *error = Some(format!("{e:#}"));
            return;
        }
    };
    let text_layer = {
        let t = doc.region_text(id);
        (!t.trim().is_empty()).then_some(t)
    };
    let translator = match build_translator(settings, secrets) {
        Ok(t) => t,
        Err(e) => {
            *error = Some(format!("{e:#}"));
            return;
        }
    };

    let (provider, model) = backend_ids(&settings.backend);
    let target_lang = settings.target_lang.clone();
    let source_lang = settings.source_lang.clone();
    let cache = Arc::clone(cache);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = run_translation(
            translator,
            crop,
            text_layer,
            provider,
            model,
            target_lang,
            source_lang,
            cache,
        );
        let _ = tx.send(outcome);
    });
    *pending = Some(Pending { region: id, rx });
    *error = None;
}

/// Worker body (runs off the UI thread): resolve source text, hit the cache, or
/// translate and cache the result.
#[allow(clippy::too_many_arguments)]
fn run_translation(
    translator: Box<dyn Translator + Send>,
    crop: image::RgbaImage,
    text_layer: Option<String>,
    provider: &'static str,
    model: String,
    target_lang: String,
    source_lang: String,
    cache: Arc<Mutex<TranslationCache>>,
) -> Result<Translated> {
    let (source_text, method) = match text_layer {
        Some(t) => (Some(t), SourceMethod::TextLayer),
        None => match ocr::recognize(&crop, &source_lang) {
            Ok(t) if !t.trim().is_empty() => (Some(t), SourceMethod::Ocr),
            _ => (None, SourceMethod::ImageOnly),
        },
    };

    let key = match &source_text {
        Some(t) => cache::key_text(provider, &model, &target_lang, t),
        None => cache::key_pixels(provider, &model, &target_lang, &crop),
    };
    if let Some(result) = cache.lock().unwrap().get(&key) {
        return Ok(Translated {
            result,
            method,
            style: RegionStyle::default(),
        });
    }

    let req = TranslationRequest {
        image: &crop,
        source_text: source_text.as_deref(),
        target_lang: &target_lang,
    };
    let result = translator.translate(&req)?;
    cache.lock().unwrap().insert(key, result.clone());
    Ok(Translated {
        result,
        method,
        style: RegionStyle::default(),
    })
}

/// Provider id and model string used for cache keys.
fn backend_ids(backend: &Backend) -> (&'static str, String) {
    match backend {
        Backend::GeminiVision { model } => ("gemini", model.clone()),
        Backend::TesseractText { api } => ("tesseract", format!("{api:?}")),
    }
}

/// Build the configured translator, pulling the API key from secrets/env.
fn build_translator(settings: &Settings, secrets: &Secrets) -> Result<Box<dyn Translator + Send>> {
    let api_key = crate::config::load_api_key(secrets, &settings.backend)?;
    Ok(match &settings.backend {
        Backend::GeminiVision { model } => Box::new(GeminiVision {
            api_key,
            model: model.clone(),
        }),
        Backend::TesseractText { api } => Box::new(TesseractText { api: *api, api_key }),
    })
}
