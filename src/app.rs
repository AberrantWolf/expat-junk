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
use crate::translate::{GeminiVision, TesseractText, TextApi, TranslationRequest, Translator};
use crate::translation::{Justify, RegionStyle, Rotate, SourceMethod, Translated};
use crate::viewer::TranslationOverlay;
use junk_libs_egui_docview::{DocView, ViewState};

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

/// An in-flight translation running on a background thread.
struct Pending {
    region: RegionId,
    rect: [f32; 4],
    settings: Settings,
    rx: mpsc::Receiver<Result<Translated>>,
}

enum Loaded {
    Document(Document),
    Project(Box<project::Project>, Document),
}

struct Loading {
    path: PathBuf,
    rx: mpsc::Receiver<Result<Loaded>>,
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
    loading: Option<Loading>,
    exporting: Option<mpsc::Receiver<Result<()>>>,
    /// Regions queued for batch translation ("Translate all"), processed one at
    /// a time through `pending`.
    queue: VecDeque<RegionId>,
    error: Option<String>,
    /// The shared interactive page-view widget (zoom/pan/region editing).
    doc_view: DocView,
    view_state: ViewState,
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
            loading: None,
            exporting: None,
            queue: VecDeque::new(),
            error: None,
            doc_view: DocView::default(),
            view_state: ViewState::default(),
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
            loading,
            exporting,
            queue,
            error,
            doc_view,
            view_state,
            overlay,
            show_translations,
            secrets,
            show_settings,
            api_key_input,
            title,
        } = self;

        if let Some(rx) = exporting.as_ref() {
            match rx.try_recv() {
                Ok(result) => {
                    *error = result.err().map(|e| format!("Exporting PDF: {e:#}"));
                    *exporting = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(50))
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    *error = Some("PDF export worker stopped".into());
                    *exporting = None;
                }
            }
        }

        let load_result = loading.as_ref().and_then(|load| match load.rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("document loader stopped")))
            }
        });
        if let Some(result) = load_result {
            let path = loading.take().expect("load result has a job").path;
            match result {
                Ok(loaded) => {
                    match loaded {
                        Loaded::Document(doc) => {
                            *document = Some(doc);
                            *source_path = Some(path);
                            *project_path = None;
                            *current_page = 0;
                            settings.context.clear();
                            results.clear();
                        }
                        Loaded::Project(proj, doc) => {
                            *current_page =
                                proj.current_page.min(doc.pages.len().saturating_sub(1));
                            *document = Some(doc);
                            *source_path = Some(proj.source_path);
                            *project_path = Some(path);
                            *results = proj.results;
                            settings.source_lang = proj.source_lang;
                            settings.reading_direction = proj.reading_direction;
                            settings.target_lang = proj.target_lang;
                            settings.backend = proj.backend;
                            settings.context = proj.context;
                            settings.save();
                        }
                    }
                    *selected = None;
                    *error = None;
                    *pending = None;
                    queue.clear();
                    *dirty = false;
                    doc_view.invalidate_document();
                    *view_state = ViewState::default();
                    overlay.reset();
                }
                Err(e) => *error = Some(format!("Opening document: {e:#}")),
            }
        }
        if loading.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Opening document…");
            });
            ui.disable();
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        if let Some(doc) = document.as_mut() {
            if let Err(e) = doc.poll_render() {
                *error = Some(format!("Rendering page: {e:#}"));
            }
            if doc.is_rendering() {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        // --- collect a finished translation, if any --------------------------
        let mut clear_pending = false;
        if let Some(p) = pending.as_mut() {
            match p.rx.try_recv() {
                Ok(Ok(mut translated)) => {
                    if !pending_is_current(p, document.as_ref(), settings) {
                        *error = Some(
                            "Region or translation settings changed; discarded the old result."
                                .into(),
                        );
                        queue.clear();
                        clear_pending = true;
                    } else {
                        // Re-translating a region keeps the user's presentation
                        // overrides (rotation, background, alignment, edited text).
                        if let Some(prev) = results.get(&p.region) {
                            translated.style = prev.style.clone();
                        }
                        results.insert(p.region, translated);
                        *dirty = true;
                        clear_pending = true;
                    }
                }
                Ok(Err(e)) => {
                    *error = Some(format!("{e:#}"));
                    queue.clear(); // abort any batch rather than repeat the failure
                    clear_pending = true;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(50))
                }
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

        let close_requested = ctx.input(|i| i.viewport().close_requested());
        let has_unsaved_work =
            *dirty || pending.is_some() || exporting.is_some() || !queue.is_empty();
        if close_requested && has_unsaved_work {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        let mut action: Option<Action> = if close_requested && has_unsaved_work {
            Some(Action::Quit)
        } else {
            None
        };

        // Count regions that still need translating (for the batch menu item).
        let untranslated = document
            .as_ref()
            .map(|d| {
                d.regions
                    .iter()
                    .filter(|r| !results.get(&r.id).is_some_and(|t| t.is_current(settings)))
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
                        .add_enabled(
                            document.is_some() && exporting.is_none(),
                            egui::Button::new("Export translated…"),
                        )
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
            ui,
            settings,
            document,
            results,
            pending,
            exporting.is_some(),
            *selected,
            dirty,
            error,
        ));

        // --- central canvas --------------------------------------------------
        egui::CentralPanel::default().show_inside(ui, |ui| {
            if let Some(doc) = document.as_mut() {
                let mut frame = overlay.frame(results, *show_translations);
                view_state.page_index = *current_page;
                let output = doc_view.show(ui, doc, view_state, selected, &mut frame, |ui| {
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
                if output.edited {
                    doc.clamp_regions();
                }
                if doc.is_rendering() {
                    ctx.request_repaint_after(std::time::Duration::from_millis(50));
                }
                results.retain(|id, _| doc.regions.iter().any(|r| r.id == *id));
                let page_regions = doc
                    .regions
                    .iter()
                    .filter(|r| r.page == view_state.page_index)
                    .map(|r| r.id)
                    .collect();
                overlay.prune(results, &page_regions);
                *current_page = view_state.page_index;
                if output.edited || output.page_changed {
                    *dirty = true;
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open a PDF or image to begin.");
                });
            }
        });

        if loading.is_none()
            && settings_window(&ctx, show_settings, settings, secrets, api_key_input, error)
        {
            *dirty |= document.is_some();
        }

        // Keyboard shortcuts (only if no menu/toolbar action already fired).
        if action.is_none() && loading.is_none() {
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

        if (*dirty || pending.is_some() || exporting.is_some() || !queue.is_empty())
            && matches!(
                action,
                Some(Action::OpenDoc | Action::OpenProject | Action::Quit)
            )
        {
            let discard = rfd::MessageDialog::new()
                .set_title("Unsaved project")
                .set_description(
                    "Discard unsaved changes and pending work? Cancel and save the project first to keep your work.",
                )
                .set_level(rfd::MessageLevel::Warning)
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if discard != rfd::MessageDialogResult::Ok {
                action = None;
            }
        }

        // --- execute the action ----------------------------------------------
        match action {
            None => {}
            Some(Action::OpenDoc) => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Documents", &["pdf", "png", "jpg", "jpeg", "webp", "tiff"])
                    .pick_file()
                {
                    *pending = None;
                    queue.clear();
                    *loading = Some(start_loading(path, false, &ctx));
                }
            }
            Some(Action::OpenProject) => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("expat-junk project", &[project::EXTENSION])
                    .pick_file()
                {
                    *pending = None;
                    queue.clear();
                    *loading = Some(start_loading(path, true, &ctx));
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
                        if exporting.is_none() {
                            let snapshot = doc.snapshot();
                            let results = results.clone();
                            let src = src.clone();
                            let (tx, rx) = mpsc::channel();
                            let repaint = ctx.clone();
                            std::thread::spawn(move || {
                                let _ = tx.send(export::export(&snapshot, &src, &results, &path));
                                repaint.request_repaint();
                            });
                            *exporting = Some(rx);
                        }
                    }
                }
            }
            Some(Action::Quit) => {
                *pending = None;
                queue.clear();
                *dirty = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some(Action::Translate) => {
                if let Some(id) = *selected {
                    start_translation(
                        id,
                        settings,
                        document,
                        secrets,
                        cache,
                        pending,
                        error,
                        results.contains_key(&id),
                        &ctx,
                    );
                }
            }
            Some(Action::TranslateAll) => {
                queue.clear();
                if let Some(doc) = document.as_ref() {
                    for r in &doc.regions {
                        if !results.get(&r.id).is_some_and(|t| t.is_current(settings)) {
                            queue.push_back(r.id);
                        }
                    }
                }
            }
            Some(Action::DeleteSelected) => {
                if let (Some(id), Some(doc)) = (*selected, document.as_mut()) {
                    doc.remove_region(id);
                    results.remove(&id);
                    queue.retain(|queued| *queued != id);
                    if pending.as_ref().is_some_and(|p| p.region == id) {
                        *pending = None;
                    }
                    *selected = None;
                    *dirty = true;
                }
            }
            Some(Action::ClearTranslations) => {
                *dirty = *dirty || !results.is_empty();
                results.clear();
                queue.clear();
                *pending = None;
            }
            Some(Action::ToggleSettings) => *show_settings = !*show_settings,
        }

        // --- drive the batch queue: start the next region when idle ----------
        if pending.is_none() {
            while let Some(next) = queue.pop_front() {
                let still_valid = document
                    .as_ref()
                    .is_some_and(|d| d.regions.iter().any(|r| r.id == next))
                    && !results.get(&next).is_some_and(|t| t.is_current(settings));
                if still_valid {
                    start_translation(
                        next, settings, document, secrets, cache, pending, error, false, &ctx,
                    );
                    // If it couldn't even start (e.g. no API key), abort the batch.
                    if pending.is_none() {
                        queue.clear();
                    }
                    break;
                }
            }
        }
        if pending.is_some() || !queue.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
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
    exporting: bool,
    selected: Option<RegionId>,
    dirty: &mut bool,
    error: &Option<String>,
) -> Option<Action> {
    let mut action = None;
    egui::Panel::right("info").show_inside(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
        // Global translation status + language pickers (apply to every region).
        if pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Translating…");
            });
        }
        if exporting {ui.horizontal(|ui|{ui.spinner();ui.label("Exporting PDF…");});}
        if let Some(err) = error {
            ui.colored_label(egui::Color32::RED, err.as_str());
        }
        ui.heading("Languages");
        // Display names are computed up front so the immutable borrow they need
        // doesn't overlap the `&mut` the combo takes on the same field.
        let src_name = crate::config::Language::name_for_ocr(&settings.source_lang).to_owned();
        if language_combo(ui, "Source (OCR)", "src_lang", &mut settings.source_lang, &src_name, |l| l.ocr) {
            settings.save();
            *dirty |= document.is_some();
        }
        ui.horizontal(|ui| {
            use junk_libs_platen::reading_order::Direction;
            ui.label("PDF reading order");
            let previous = settings.reading_direction;
            egui::ComboBox::from_id_salt("reading_direction").selected_text(match previous { Direction::Auto=>"Automatic",Direction::LeftToRight=>"Left to right",Direction::RightToLeft=>"Right to left",Direction::VerticalRightToLeft=>"Vertical Japanese" }).show_ui(ui,|ui| {
                for (direction,label) in [(Direction::Auto,"Automatic"),(Direction::LeftToRight,"Left to right"),(Direction::RightToLeft,"Right to left"),(Direction::VerticalRightToLeft,"Vertical Japanese")] {
                    ui.selectable_value(&mut settings.reading_direction,direction,label);
                }
            });
            if previous!=settings.reading_direction {settings.save();*dirty|=document.is_some();}
        });
        let tgt_name = crate::config::Language::name_for_iso(&settings.target_lang).to_owned();
        if language_combo(ui, "Target", "tgt_lang", &mut settings.target_lang, &tgt_name, |l| l.iso) {
            settings.save();
            *dirty |= document.is_some();
        }
        ui.separator();
        ui.collapsing("Document context / terminology", |ui| {
            ui.small("Used by Gemini and DeepL. Include the document type, intended tone, or terms to preserve.");
            if ui.add(egui::TextEdit::multiline(&mut settings.context).desired_rows(3)).changed() {
                settings.save();
                *dirty |= document.is_some();
            }
        });
        ui.separator();

        ui.heading("Region");
        let can_translate = document.is_some() && pending.is_none();
        let translating = pending.as_ref().map(|p| p.region) == selected && pending.is_some();
        match selected {
            Some(id) if results.contains_key(&id) => {
                let t = results.get_mut(&id).expect("checked by guard");
                badge(ui, t.method);
                if !t.is_current(settings) {
                    ui.colored_label(egui::Color32::YELLOW, "Translation uses previous language, provider, or context. Re-translate to update it.");
                }
                if let Some(previous) = &t.settings {
                    ui.small(format!("{} → {}", previous.backend.provider_label(), previous.target_lang));
                }
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
                    let text = doc.region_text_with_direction(id,settings.reading_direction);
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
    error: &mut Option<String>,
) -> bool {
    let before = settings.clone();
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
                        ("DeepL", TextApi::DeepL, true),
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
                            egui::TextEdit::singleline(model).hint_text("gemini-3.5-flash-lite"),
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
                    match secrets.set(provider, api_key_input.clone()) {
                        Ok(()) => { api_key_input.clear(); *error = None; }
                        Err(e) => *error = Some(format!("Saving API key: {e:#}")),
                    }
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
    let changed = *settings != before;
    if changed {
        api_key_input.clear();
    }
    changed
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
    if std::fs::canonicalize(&path)
        .ok()
        .is_some_and(|p| std::fs::canonicalize(src).ok().as_ref() == Some(&p))
    {
        *error = Some("Project destination must differ from the source document.".into());
        return;
    }

    let proj = project::build(
        src.clone(),
        current_page,
        settings.source_lang.clone(),
        settings.reading_direction,
        settings.target_lang.clone(),
        settings.context.clone(),
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
#[allow(clippy::too_many_arguments)]
fn start_translation(
    id: RegionId,
    settings: &Settings,
    document: &Option<Document>,
    secrets: &Secrets,
    cache: &Arc<Mutex<TranslationCache>>,
    pending: &mut Option<Pending>,
    error: &mut Option<String>,
    bypass_cache: bool,
    ctx: &egui::Context,
) {
    if pending.is_some() {
        return;
    }
    let Some(doc) = document.as_ref() else {
        return;
    };
    let Some(region) = doc.regions.iter().find(|r| r.id == id) else {
        return;
    };
    let rect = rect_signature(region.rect);
    let input = match doc.region_input(id) {
        Ok(input) => input,
        Err(e) => {
            *error = Some(format!("{e:#}"));
            return;
        }
    };
    // A cached result remains available offline, without a configured key.
    let translator = build_translator(settings, secrets);

    let worker_settings = settings.clone();
    let cache = Arc::clone(cache);
    let repaint = ctx.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = input
            .resolve_with_direction(2.0, worker_settings.reading_direction)
            .and_then(|(crop, text)| {
                let text_layer = (!text.trim().is_empty()).then_some(text);
                run_translation(
                    translator,
                    crop,
                    text_layer,
                    worker_settings,
                    bypass_cache,
                    cache,
                )
            });
        let _ = tx.send(outcome);
        repaint.request_repaint();
    });
    *pending = Some(Pending {
        region: id,
        rect,
        settings: settings.clone(),
        rx,
    });
    *error = None;
}

/// Worker body (runs off the UI thread): resolve source text, hit the cache, or
/// translate and cache the result.
fn run_translation(
    translator: Result<Box<dyn Translator + Send>>,
    crop: image::RgbaImage,
    text_layer: Option<String>,
    settings: Settings,
    bypass_cache: bool,
    cache: Arc<Mutex<TranslationCache>>,
) -> Result<Translated> {
    let (provider, model) = backend_ids(&settings.backend);
    let (target_lang, source_lang, context) = (
        &settings.target_lang,
        &settings.source_lang,
        &settings.context,
    );
    let (source_text, method) = match text_layer {
        Some(t) => (Some(t), SourceMethod::TextLayer),
        None => match ocr::recognize(&crop, source_lang) {
            Ok(t) if !t.trim().is_empty() => (Some(t), SourceMethod::Ocr),
            Err(e) if provider != "gemini" => {
                return Err(e.context(
                    "Reading source text: use Gemini for vision, or enable/configure local OCR",
                ));
            }
            _ => (None, SourceMethod::ImageOnly),
        },
    };

    let key = cache::key_request(
        provider,
        &model,
        target_lang,
        source_lang,
        if provider == "google" { "" } else { context },
        source_text.as_deref(),
        (provider == "gemini").then_some(&crop),
    );
    if !bypass_cache
        && let Some(result) = cache
            .lock()
            .map_err(|_| anyhow::anyhow!("translation cache lock poisoned"))?
            .get(&key)
    {
        return Ok(Translated {
            result,
            method,
            style: RegionStyle::default(),
            settings: Some(settings),
        });
    }

    let req = TranslationRequest {
        image: &crop,
        source_text: source_text.as_deref(),
        target_lang,
        context,
    };
    let result = translator?.translate(&req)?;
    cache
        .lock()
        .map_err(|_| anyhow::anyhow!("translation cache lock poisoned"))?
        .insert(key, result.clone());
    Ok(Translated {
        result,
        method,
        style: RegionStyle::default(),
        settings: Some(settings),
    })
}

/// Provider id and model string used for cache keys.
fn backend_ids(backend: &Backend) -> (&'static str, String) {
    match backend {
        Backend::GeminiVision { model } => ("gemini", model.clone()),
        Backend::TesseractText { api } => (backend.secret_id(), format!("{api:?}")),
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

fn rect_signature(rect: crate::document::Rect) -> [f32; 4] {
    [rect.x, rect.y, rect.w, rect.h]
}

fn pending_is_current(p: &Pending, doc: Option<&Document>, settings: &Settings) -> bool {
    p.settings == *settings
        && doc.is_some_and(|doc| {
            doc.regions
                .iter()
                .any(|r| r.id == p.region && rect_signature(r.rect) == p.rect)
        })
}

fn start_loading(path: PathBuf, is_project: bool, ctx: &egui::Context) -> Loading {
    let (tx, rx) = mpsc::channel();
    let source = path.clone();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        let result = if is_project {
            project::Project::load(&source)
                .and_then(|p| project::open_document(&p).map(|d| Loaded::Project(Box::new(p), d)))
        } else {
            Document::open(&source).map(Loaded::Document)
        };
        let _ = tx.send(result);
        repaint.request_repaint();
    });
    Loading { path, rx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Rect;
    use crate::translate::TranslationResult;

    struct Stub;
    impl Translator for Stub {
        fn translate(&self, _: &TranslationRequest) -> Result<TranslationResult> {
            Ok(result("fresh"))
        }
    }
    fn result(text: &str) -> TranslationResult {
        TranslationResult {
            source_text: "source".into(),
            translation: text.into(),
            alternatives: vec![],
            explanation: None,
        }
    }

    #[test]
    fn retranslate_bypasses_cached_result() {
        let dir = tempfile::tempdir().unwrap();
        let crop = image::RgbaImage::new(8, 8);
        // This test must never load or write the user's real cache.
        let cache = Arc::new(Mutex::new(TranslationCache::with_path(Some(
            dir.path().join("cache.json"),
        ))));
        let key = cache::key_request("google", "Google", "en", "eng", "", Some("source"), None);
        cache.lock().unwrap().insert(key, result("cached"));
        let run = |bypass| {
            run_translation(
                Ok(Box::new(Stub)),
                crop.clone(),
                Some("source".into()),
                Settings {
                    backend: Backend::TesseractText {
                        api: TextApi::Google,
                    },
                    ..Settings::default()
                },
                bypass,
                Arc::clone(&cache),
            )
            .unwrap()
        };
        let cached_without_key = run_translation(
            Err(anyhow::anyhow!("missing API key")),
            crop.clone(),
            Some("source".into()),
            Settings {
                backend: Backend::TesseractText {
                    api: TextApi::Google,
                },
                ..Settings::default()
            },
            false,
            Arc::clone(&cache),
        )
        .unwrap();
        assert_eq!(cached_without_key.result.translation, "cached");
        assert_eq!(run(false).result.translation, "cached");
        assert_eq!(run(true).result.translation, "fresh");
        assert_eq!(run(false).result.translation, "fresh");
    }

    #[test]
    fn pending_result_rejects_deleted_moved_and_reconfigured_regions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.png");
        image::RgbaImage::new(100, 100).save(&path).unwrap();
        let mut doc = Document::open(&path).unwrap();
        let rect = Rect {
            x: 1.0,
            y: 2.0,
            w: 20.0,
            h: 20.0,
        };
        let id = doc.add_region(0, rect);
        let settings = Settings::default();
        let (_, rx) = mpsc::channel();
        let pending = Pending {
            region: id,
            rect: rect_signature(rect),
            settings: settings.clone(),
            rx,
        };
        assert!(pending_is_current(&pending, Some(&doc), &settings));
        let mut changed = settings.clone();
        changed.target_lang = "ja".into();
        assert!(!pending_is_current(&pending, Some(&doc), &changed));
        doc.region_mut(id).unwrap().rect.x += 1.0;
        assert!(!pending_is_current(&pending, Some(&doc), &settings));
        doc.remove_region(id);
        assert!(!pending_is_current(&pending, Some(&doc), &settings));
        assert!(!pending_is_current(&pending, None, &settings));
    }
}
