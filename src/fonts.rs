//! Bundled fonts.
//!
//! egui's default fonts cover Latin only, so Japanese (and other CJK) source and
//! translated text would render as tofu boxes. We append Noto Sans JP (SIL Open
//! Font License — see `assets/fonts/OFL.txt`) as the last fallback for both font
//! families: Latin keeps the default look, and any codepoint the defaults can't
//! draw falls through to Noto.

use std::sync::Arc;

const NOTO_SANS_JP: &[u8] = include_bytes!("../assets/fonts/NotoSansJP.ttf");

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto_sans_jp".to_owned(),
        Arc::new(egui::FontData::from_static(NOTO_SANS_JP)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("noto_sans_jp".to_owned());
    }
    ctx.set_fonts(fonts);
}
