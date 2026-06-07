//! Bundled fonts.
//!
//! egui's default fonts cover Latin only, so Japanese (and other CJK) source and
//! translated text would render as tofu boxes. We append Noto Sans JP (SIL Open
//! Font License — see `assets/fonts/OFL.txt`) as the last fallback for both font
//! families: Latin keeps the default look, and any codepoint the defaults can't
//! draw falls through to Noto.

use std::sync::Arc;

/// Bundled Noto Sans JP (OFL). Also used by the export raster text renderer.
///
/// This is the **variable** font (weight axis 100–900); its default instance is
/// Thin (100), which renders spindly. Always pin the weight via [`weighted`]
/// before rasterizing translation text so it isn't unintentionally thin.
pub const NOTO_SANS_JP: &[u8] = include_bytes!("../assets/fonts/NotoSansJP.ttf");

/// A `FontRef` over the bundled variable font with its weight axis pinned —
/// `bold` selects 700 (Bold) vs 400 (Regular). Used by the export renderer and
/// the viewer's live preview so the rasterized translation has a usable weight
/// (the font's default instance is Thin) and an honest bold option.
pub fn weighted(bold: bool) -> ab_glyph::FontRef<'static> {
    use ab_glyph::VariableFont;
    let mut font = ab_glyph::FontRef::try_from_slice(NOTO_SANS_JP).expect("bundled font is valid");
    font.set_variation(b"wght", if bold { 700.0 } else { 400.0 });
    font
}

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
