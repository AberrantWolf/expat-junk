// expat-junk: a desktop tool for translating PDFs and images region-by-region.
//
// Pipeline (see module docs for detail):
//   document  -> render page to a bitmap + extract per-character boxes
//   ui        -> user draws/resizes regions over the page on an egui canvas
//   translate -> each region's crop goes to a Translator (Gemini vision / OCR+text API)
//   export    -> bake translations back onto the original as an overlay PDF
//
// The GUI (egui) only owns the interactive editing surface. Rendering, OCR,
// API calls, and PDF generation are all backend modules with no egui dependency.

mod app;
mod cache;
mod config;
mod document;
mod export;
mod ocr;
mod translate;
mod viewer;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "expat-junk",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
