//! The egui application shell: window layout and interaction state.
//!
//! This is the only module that wires the UI together. It owns transient state
//! (current document, page, selection, cached translations, in-flight request)
//! and delegates the page canvas to [`crate::viewer::Viewer`]. Three areas:
//!   - top bar: open file, target language, translate, export, page nav
//!   - central canvas: the rendered page with draggable/resizable regions
//!   - right panel: detected source text + translation for the selected region

use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};

use anyhow::Result;

use crate::cache::{self, TranslationCache};
use crate::config::{Backend, Settings};
use crate::document::{Document, RegionId};
use crate::ocr;
use crate::secrets::Secrets;
use crate::translate::{
    GeminiVision, TesseractText, TextApi, TranslationRequest, TranslationResult, Translator,
};
use crate::viewer::Viewer;

/// How a region's source text was obtained — shown as a badge so the user knows
/// what is being sent to translate.
#[derive(Clone, Copy, PartialEq)]
enum SourceMethod {
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
struct Translated {
    result: TranslationResult,
    method: SourceMethod,
}

/// An in-flight translation running on a background thread.
struct Pending {
    region: RegionId,
    rx: mpsc::Receiver<Result<Translated>>,
}

pub struct App {
    settings: Settings,
    document: Option<Document>,
    /// Index of the page currently shown on the canvas.
    current_page: usize,
    selected: Option<RegionId>,
    /// Completed translations keyed by region, for the info panel and export.
    results: HashMap<RegionId, Translated>,
    /// Persistent cache so repeat translations are free. Shared with the worker
    /// thread, which checks and populates it after resolving the source text.
    cache: Arc<Mutex<TranslationCache>>,
    /// The one translation currently in flight, if any.
    pending: Option<Pending>,
    /// Most recent error, shown in the top bar.
    error: Option<String>,
    viewer: Viewer,
    /// Local API-key store, edited via the Settings window.
    secrets: Secrets,
    show_settings: bool,
    /// Scratch buffer for the API-key field (never pre-filled from `secrets`).
    api_key_input: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::fonts::install(&cc.egui_ctx);
        Self {
            settings: Settings::load(),
            document: None,
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
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Disjoint field borrows so the panels and the viewer can run together.
        let App {
            settings,
            document,
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
        } = self;

        // --- collect a finished translation, if any --------------------------
        let mut clear_pending = false;
        if let Some(p) = pending.as_mut() {
            match p.rx.try_recv() {
                Ok(Ok(translated)) => {
                    results.insert(p.region, translated);
                    clear_pending = true;
                }
                Ok(Err(e)) => {
                    *error = Some(format!("{e:#}"));
                    clear_pending = true;
                }
                Err(mpsc::TryRecvError::Empty) => ctx.request_repaint(), // keep polling
                Err(mpsc::TryRecvError::Disconnected) => {
                    *error = Some("translation worker stopped unexpectedly".to_string());
                    clear_pending = true;
                }
            }
        }
        if clear_pending {
            *pending = None;
        }

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
                            *pending = None;
                            viewer.reset();
                        }
                        Err(e) => *error = Some(format!("{e:#}")),
                    }
                }
                ui.separator();
                // Editable source/target languages. Source is a Tesseract code
                // (eng, jpn, …) used only for OCR; target is the translation
                // target. Persisted on change.
                ui.label("OCR src");
                let src = ui.add(
                    egui::TextEdit::singleline(&mut settings.source_lang)
                        .desired_width(40.0)
                        .hint_text("eng"),
                );
                ui.label("→");
                let tgt = ui.add(
                    egui::TextEdit::singleline(&mut settings.target_lang)
                        .desired_width(40.0)
                        .hint_text("en"),
                );
                if src.changed() || tgt.changed() {
                    settings.save();
                }

                let can_translate =
                    document.is_some() && selected.is_some() && pending.is_none();
                if ui
                    .add_enabled(can_translate, egui::Button::new("Translate selection"))
                    .clicked()
                {
                    start_translation(settings, document, *selected, secrets, cache, pending, error);
                }
                let _ = ui.button("Export…"); // TODO: export::export
                if ui.button("Settings").clicked() {
                    *show_settings = !*show_settings;
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
            let translating = pending.as_ref().map(|p| p.region) == *selected && pending.is_some();
            match *selected {
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
                    // Pre-translation preview: what we'll send, and how we got it.
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
                        ui.label("Press “Translate selection” above.");
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

        settings_window(&ctx, show_settings, settings, secrets, api_key_input);
    }
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
                        let resp = ui.add_enabled(ready, egui::SelectableLabel::new(sel, text));
                        if resp.clicked() && !sel {
                            settings.backend = Backend::TesseractText { api };
                            settings.save();
                        }
                    }
                });

            // Model is Gemini-specific. Persist edits once the &mut borrow ends.
            let mut model_changed = false;
            if let Backend::GeminiVision { model } = &mut settings.backend {
                ui.horizontal(|ui| {
                    ui.label("Model");
                    model_changed = ui
                        .add(
                            egui::TextEdit::singleline(model)
                                .hint_text("gemini-flash-lite-latest"),
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

/// A small colored badge naming how the source text was/will be obtained.
fn badge(ui: &mut egui::Ui, method: SourceMethod) {
    ui.label(
        egui::RichText::new(format!("source: {}", method.label()))
            .small()
            .color(method.color()),
    );
}

/// Crop the selected region and kick off a background translation. The worker
/// resolves the source text (text layer → OCR), checks/populates the shared
/// cache, and calls the translator. All errors land in `error`.
#[allow(clippy::too_many_arguments)]
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
    // Cheap text-layer extraction happens on the UI thread; OCR (if needed) runs
    // on the worker.
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
            _ => (None, SourceMethod::ImageOnly), // OCR off or found nothing
        },
    };

    let key = match &source_text {
        Some(t) => cache::key_text(provider, &model, &target_lang, t),
        None => cache::key_pixels(provider, &model, &target_lang, &crop),
    };
    if let Some(result) = cache.lock().unwrap().get(&key) {
        return Ok(Translated { result, method }); // cache hit, no API call
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
