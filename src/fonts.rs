//! Explicit Noto fonts for the UI and shared multilingual translation renderer.

use std::sync::Arc;

/// Bundled variable CJK face; the shaping engine selects the requested weight.
pub const NOTO_SANS_JP: &[u8] = include_bytes!("../assets/fonts/NotoSansJP.ttf");

pub const NOTO_SANS_KR: &[u8] = include_bytes!("../assets/fonts/NotoSansKR.ttf");

thread_local! {
    static RENDERER: std::cell::RefCell<junk_libs_text::TextRenderer> = std::cell::RefCell::new(
        junk_libs_text::TextRenderer::with_fonts([NOTO_SANS_JP.to_vec(), NOTO_SANS_KR.to_vec()]));
}

pub fn with_renderer<R>(f: impl FnOnce(&mut junk_libs_text::TextRenderer) -> R) -> R {
    RENDERER.with_borrow_mut(f)
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (index, data) in junk_libs_text::BUNDLED_FONTS
        .iter()
        .copied()
        .chain([NOTO_SANS_JP, NOTO_SANS_KR])
        .enumerate()
    {
        let name = format!("noto-fallback-{index}");
        fonts
            .font_data
            .insert(name.clone(), Arc::new(egui::FontData::from_static(data)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.clone());
        }
    }
    ctx.set_fonts(fonts);
}
