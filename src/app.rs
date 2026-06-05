//! The egui application shell: window layout, menus, and session state.
//!
//! This module wires the UI together and owns the session: the open document and
//! its source path, the current project file + dirty flag, the selection, cached
//! translations, and the in-flight request. The page canvas is delegated to
//! [`crate::viewer::Viewer`]. Menu/toolbar clicks are collected as an [`Action`]
//! and executed after the panels render, which keeps borrows simple.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

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
use crate::viewer::Viewer;

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

/// A completed translation plus how its source text was obtained.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Translated {
    pub result: TranslationResult,
    pub method: SourceMethod,
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
    error: Option<String>,
    viewer: Viewer,
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
            error: None,
            viewer: Viewer::default(),
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
            error,
            viewer,
            secrets,
            show_settings,
            api_key_input,
            title,
        } = self;

        // --- collect a finished translation, if any --------------------------
        let mut clear_pending = false;
        if let Some(p) = pending.as_mut() {
            match p.rx.try_recv() {
                Ok(Ok(translated)) => {
                    results.insert(p.region, translated);
                    *dirty = true;
                    clear_pending = true;
                }
                Ok(Err(e)) => {
                    *error = Some(format!("{e:#}"));
                    clear_pending = true;
                }
                Err(mpsc::TryRecvError::Empty) => ctx.request_repaint(),
                Err(mpsc::TryRecvError::Disconnected) => {
                    *error = Some("translation worker stopped unexpectedly".to_string());
                    clear_pending = true;
                }
            }
        }
        if clear_pending {
            *pending = None;
        }

        let mut action: Option<Action> = None;

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
                        .add_enabled(selected.is_some(), egui::Button::new("Delete selected region"))
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

            ui.horizontal(|ui| {
                ui.label("OCR src");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut settings.source_lang)
                            .desired_width(40.0)
                            .hint_text("eng"),
                    )
                    .changed()
                {
                    settings.save();
                }
                ui.label("→");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut settings.target_lang)
                            .desired_width(40.0)
                            .hint_text("en"),
                    )
                    .changed()
                {
                    settings.save();
                }
                ui.separator();
                let can_translate =
                    document.is_some() && selected.is_some() && pending.is_none();
                if ui
                    .add_enabled(can_translate, egui::Button::new("Translate selection"))
                    .clicked()
                {
                    action = Some(Action::Translate);
                }
                if pending.is_some() {
                    ui.separator();
                    ui.spinner();
                    ui.label("Translating…");
                }
                if let Some(err) = error {
                    ui.separator();
                    ui.colored_label(egui::Color32::RED, err.as_str());
                }
            });
        });

        // --- right panel: selected region info -------------------------------
        info_panel(ui, document, results, pending, *selected);

        // --- central canvas --------------------------------------------------
        egui::CentralPanel::default().show_inside(ui, |ui| {
            if let Some(doc) = document.as_mut() {
                if viewer.show(ui, doc, current_page, selected) {
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
                            viewer.reset();
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
                            viewer.reset();
                        }
                        Err(e) => *error = Some(format!("{e:#}")),
                    }
                }
            }
            Some(Action::SaveProject) => save_project(
                false, settings, source_path, document, *current_page, results, project_path,
                dirty, error,
            ),
            Some(Action::SaveProjectAs) => save_project(
                true, settings, source_path, document, *current_page, results, project_path,
                dirty, error,
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
                        match export::export_pdf(doc, results, &path) {
                            Ok(()) => *error = None,
                            Err(e) => *error = Some(format!("{e:#}")),
                        }
                    }
                }
            }
            Some(Action::Quit) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Some(Action::Translate) => {
                start_translation(settings, document, *selected, secrets, cache, pending, error);
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
                if !results.is_empty() {
                    results.clear();
                    *dirty = true;
                }
            }
            Some(Action::ToggleSettings) => *show_settings = !*show_settings,
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
fn info_panel(
    ui: &mut egui::Ui,
    document: &Option<Document>,
    results: &HashMap<RegionId, Translated>,
    pending: &Option<Pending>,
    selected: Option<RegionId>,
) {
    egui::Panel::right("info").show_inside(ui, |ui| {
        ui.heading("Region");
        let translating = pending.as_ref().map(|p| p.region) == selected && pending.is_some();
        match selected {
            Some(id) if results.contains_key(&id) => {
                let t = &results[&id];
                badge(ui, t.method);
                ui.separator();
                ui.label("Source");
                ui.monospace(&t.result.source_text);
                ui.separator();
                ui.label("Translation");
                ui.monospace(&t.result.translation);
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
                    ui.separator();
                    ui.monospace(format!("page {}", reg.page + 1));
                    ui.monospace(format!("x {:.0}  y {:.0} pt", reg.rect.x, reg.rect.y));
                    ui.monospace(format!("w {:.0}  h {:.0} pt", reg.rect.w, reg.rect.h));
                    ui.add_space(4.0);
                    ui.label("Press “Translate selection”.");
                }
            }
            None => {
                ui.label("Drag on the page to draw a region, or click one to select it.");
            }
        }
    });
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
    settings: &Settings,
    document: &Option<Document>,
    selected: Option<RegionId>,
    secrets: &Secrets,
    cache: &Arc<Mutex<TranslationCache>>,
    pending: &mut Option<Pending>,
    error: &mut Option<String>,
) {
    let (Some(id), Some(doc)) = (selected, document.as_ref()) else {
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
            translator, crop, text_layer, provider, model, target_lang, source_lang, cache,
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
        return Ok(Translated { result, method });
    }

    let req = TranslationRequest {
        image: &crop,
        source_text: source_text.as_deref(),
        target_lang: &target_lang,
    };
    let result = translator.translate(&req)?;
    cache.lock().unwrap().insert(key, result.clone());
    Ok(Translated { result, method })
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
