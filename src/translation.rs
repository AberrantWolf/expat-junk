//! GUI-independent translation results and presentation settings.

use crate::translate::TranslationResult;

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
    /// Provenance for detecting translations made with different settings.
    #[serde(default)]
    pub settings: Option<crate::config::Settings>,
}

impl Translated {
    /// Whether this result matches the current translation settings.
    pub fn is_current(&self, settings: &crate::config::Settings) -> bool {
        // Preserve the behavior of legacy projects with unknown provenance.
        self.settings
            .as_ref()
            .is_none_or(|previous| previous == settings)
    }

    /// The user's edit if present, else the machine translation.
    pub fn display_text(&self) -> &str {
        self.style
            .text
            .as_deref()
            .unwrap_or(&self.result.translation)
    }
}
