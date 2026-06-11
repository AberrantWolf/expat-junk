//! Export: write a translated PDF.
//!
//! Two strategies, chosen by source type in [`export`]:
//! - **Vector-preserving** ([`export_vector`], PDF sources): load the original
//!   with `lopdf` and overlay each translated region as an opaque image patch
//!   (background-matched fill + the translated text). The original page's vector
//!   text/graphics stay untouched — only the covered source regions become
//!   raster. Default for PDFs.
//! - **Full raster** ([`export_raster`], image sources): bake the cover boxes +
//!   text onto the page raster and embed each page full-size via `printpdf`.
//!
//! Both render the translation with `ab_glyph` + the bundled CJK font (so
//! Japanese exports correctly), wrapped and auto-sized. Background matching is a
//! flat fill, so photo/textured backgrounds show a seam ("no loss if we can't").

use std::collections::HashMap;
use std::path::Path;

use ab_glyph::{Font, FontRef, Glyph, PxScale, ScaleFont, point};
use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use lopdf::{Dictionary, Object, Stream};
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Pt, RawImage, XObjectTransform};

use junk_libs_raster::{PixelRect, sample_background};

use crate::app::{Justify, Rotate, Translated};
use crate::document::{Document, RegionId};

const POINTS_PER_INCH: f32 = 72.0;

/// Export to `out_path`. For PDF sources this **preserves the original vector
/// content** and overlays only the translated regions (so untranslated text
/// stays sharp and selectable); image sources fall back to a full-raster PDF.
pub fn export(
    doc: &Document,
    source_path: &Path,
    results: &HashMap<RegionId, Translated>,
    out_path: &Path,
) -> Result<()> {
    let is_pdf = source_path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    if is_pdf {
        export_vector(doc, source_path, results, out_path)
    } else {
        export_raster(doc, results, out_path)
    }
}

/// Overlay translated regions onto the original PDF, keeping its vector content.
/// Each region becomes an opaque image patch (background-matched fill + the
/// translated text) placed over the source. Assumes a zero-origin `MediaBox` and
/// no `/Rotate` (the common case); rotated or offset pages would misplace
/// overlays.
pub fn export_vector(
    doc: &Document,
    source_path: &Path,
    results: &HashMap<RegionId, Translated>,
    out_path: &Path,
) -> Result<()> {
    let font = crate::fonts::weighted(false);
    let font_bold = crate::fonts::weighted(true);

    let mut pdf = lopdf::Document::load(source_path)
        .with_context(|| format!("loading source PDF {}", source_path.display()))?;
    let page_ids: Vec<lopdf::ObjectId> = pdf.get_pages().into_values().collect();

    let mut overlay_n = 0u32;
    for (page_index, page) in doc.pages.iter().enumerate() {
        let Some(&page_id) = page_ids.get(page_index) else {
            continue;
        };
        let page_h = page.size.1;
        let scale_x = page.bitmap.width() as f32 / page.size.0;
        let scale_y = page.bitmap.height() as f32 / page.size.1;

        for region in doc.regions.iter().filter(|r| r.page == page_index) {
            let Some(t) = results.get(&region.id) else {
                continue;
            };
            let px = PixelRect {
                x: (region.rect.x * scale_x).round() as i64,
                y: (region.rect.y * scale_y).round() as i64,
                w: (region.rect.w * scale_x).round() as i64,
                h: (region.rect.h * scale_y).round() as i64,
            };
            let bg = effective_bg(t, || sample_background(&page.bitmap, px));
            let (pw, ph, rgb) =
                render_patch(region.rect.w, region.rect.h, bg, t, &font, &font_bold);

            let img_id = add_image_xobject(&mut pdf, pw, ph, rgb);
            overlay_n += 1;
            let name = format!("XjTr{overlay_n}");
            pdf.add_xobject(page_id, name.clone().into_bytes(), img_id)
                .with_context(|| format!("adding overlay to page {page_index}"))?;

            // Place the patch at the region, converting top-left to PDF bottom-left.
            let x = region.rect.x;
            let y = page_h - region.rect.y - region.rect.h;
            let (w, h) = (region.rect.w, region.rect.h);
            let ops = format!("q {w:.3} 0 0 {h:.3} {x:.3} {y:.3} cm /{name} Do Q\n");
            pdf.add_page_contents(page_id, ops.into_bytes())
                .with_context(|| format!("drawing overlay on page {page_index}"))?;
        }
    }

    pdf.save(out_path)
        .with_context(|| format!("writing {}", out_path.display()))?;
    Ok(())
}

/// The background fill for a region: the user's override if set, else the
/// auto-sampled page color (computed lazily, since sampling isn't free).
fn effective_bg(t: &Translated, sample: impl FnOnce() -> Rgba<u8>) -> Rgba<u8> {
    match t.style.background {
        Some([r, g, b]) => Rgba([r, g, b, 255]),
        None => sample(),
    }
}

/// Render a region's translated content (opaque background + styled text) to raw
/// RGB at print resolution. Returns `(width, height, rgb_bytes)`.
fn render_patch(
    w_pts: f32,
    h_pts: f32,
    bg: Rgba<u8>,
    t: &Translated,
    regular: &FontRef,
    bold: &FontRef,
) -> (u32, u32, Vec<u8>) {
    const DPI: f32 = 200.0;
    let px_per_pt = DPI / POINTS_PER_INCH;
    let pw = ((w_pts / POINTS_PER_INCH * DPI).round() as u32).max(1);
    let ph = ((h_pts / POINTS_PER_INCH * DPI).round() as u32).max(1);
    let patch = render_region_patch(pw, ph, bg, t, regular, bold, px_per_pt);

    let mut rgb = Vec::with_capacity((patch.width() * patch.height() * 3) as usize);
    for p in patch.pixels() {
        rgb.extend_from_slice(&[p[0], p[1], p[2]]);
    }
    (patch.width(), patch.height(), rgb)
}

/// Render a `w_px`×`h_px` opaque patch: a flat background fill plus the region's
/// styled translation (edited text, alignment, optional 90° rotation). This is
/// the single source of truth shared by both export paths and the live preview,
/// so what's previewed matches what's exported.
pub(crate) fn render_region_patch(
    w_px: u32,
    h_px: u32,
    bg: Rgba<u8>,
    t: &Translated,
    regular: &FontRef,
    bold: &FontRef,
    px_per_pt: f32,
) -> RgbaImage {
    let font = if t.style.bold { bold } else { regular };
    // When rotating, lay the text out in a buffer with width/height swapped, then
    // rotate it back so it fills the region.
    let (bw, bh) = if t.style.rotate.is_sideways() {
        (h_px.max(1), w_px.max(1))
    } else {
        (w_px.max(1), h_px.max(1))
    };
    let mut patch = RgbaImage::from_pixel(bw, bh, bg);

    let lum = 0.299 * bg[0] as f32 + 0.587 * bg[1] as f32 + 0.114 * bg[2] as f32;
    let color = if lum > 140.0 {
        Rgba([0, 0, 0, 255])
    } else {
        Rgba([255, 255, 255, 255])
    };
    draw_text(
        &mut patch,
        PixelRect {
            x: 0,
            y: 0,
            w: bw as i64,
            h: bh as i64,
        },
        t.display_text().trim(),
        font,
        color,
        t.style.justify,
        t.style.size_pt,
        px_per_pt,
    );

    // `rotate90` is clockwise (text reads top-to-bottom), `rotate270` is
    // counter-clockwise (bottom-to-top). Both swap dims back to (w_px, h_px).
    match t.style.rotate {
        Rotate::None => patch,
        Rotate::Cw => image::imageops::rotate90(&patch),
        Rotate::Ccw => image::imageops::rotate270(&patch),
    }
}

/// Build a FlateDecode-compressed DeviceRGB image XObject and add it to `pdf`.
fn add_image_xobject(pdf: &mut lopdf::Document, w: u32, h: u32, rgb: Vec<u8>) -> lopdf::ObjectId {
    let mut dict = Dictionary::new();
    dict.set("Type", Object::Name(b"XObject".to_vec()));
    dict.set("Subtype", Object::Name(b"Image".to_vec()));
    dict.set("Width", Object::Integer(w as i64));
    dict.set("Height", Object::Integer(h as i64));
    dict.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
    dict.set("BitsPerComponent", Object::Integer(8));
    let mut stream = Stream::new(dict, rgb);
    let _ = stream.compress();
    pdf.add_object(stream)
}

/// Bake translations onto each page raster and write a full-raster multi-page PDF
/// — used for image sources, which have no vector content to preserve.
pub fn export_raster(
    doc: &Document,
    results: &HashMap<RegionId, Translated>,
    out_path: &Path,
) -> Result<()> {
    let font = crate::fonts::weighted(false);
    let font_bold = crate::fonts::weighted(true);

    let mut pdf = PdfDocument::new("expat-junk translation");
    let mut pages = Vec::with_capacity(doc.pages.len());

    for (page_index, page) in doc.pages.iter().enumerate() {
        let mut img = page.bitmap.clone();
        let scale_x = img.width() as f32 / page.size.0;
        let scale_y = img.height() as f32 / page.size.1;

        for region in doc.regions.iter().filter(|r| r.page == page_index) {
            let Some(t) = results.get(&region.id) else {
                continue;
            };
            let rect = PixelRect {
                x: (region.rect.x * scale_x).round() as i64,
                y: (region.rect.y * scale_y).round() as i64,
                w: (region.rect.w * scale_x).round() as i64,
                h: (region.rect.h * scale_y).round() as i64,
            };
            // Raster preserves the page aspect, so scale_x ≈ scale_y; use it as
            // the px-per-point scale for converting any explicit font size.
            bake_region(&mut img, rect, t, &font, &font_bold, scale_x);
        }

        let bitmap_w = img.width() as f32;
        let raw = RawImage::from_dynamic_image(image::DynamicImage::ImageRgba8(img))
            .map_err(|e| anyhow::anyhow!("embedding page {page_index}: {e}"))?;
        let image_id = pdf.add_image(&raw);

        // DPI that makes the bitmap exactly fill the page width; height follows
        // because the raster preserves the page's aspect ratio.
        let dpi = bitmap_w * POINTS_PER_INCH / page.size.0;
        let ops = vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform {
                translate_x: Some(Pt(0.0)),
                translate_y: Some(Pt(0.0)),
                scale_x: Some(1.0),
                scale_y: Some(1.0),
                dpi: Some(dpi),
                rotate: None,
            },
        }];
        pages.push(PdfPage::new(
            Mm(pt_to_mm(page.size.0)),
            Mm(pt_to_mm(page.size.1)),
            ops,
        ));
    }

    let bytes = pdf
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    std::fs::write(out_path, bytes).with_context(|| format!("writing {}", out_path.display()))?;
    Ok(())
}

fn pt_to_mm(pt: f32) -> f32 {
    pt / POINTS_PER_INCH * 25.4
}

/// Render the region's styled translation patch and composite it (opaque) over
/// the source at `rect`.
fn bake_region(
    img: &mut RgbaImage,
    rect: PixelRect,
    t: &Translated,
    regular: &FontRef,
    bold: &FontRef,
    px_per_pt: f32,
) {
    if rect.w <= 0 || rect.h <= 0 {
        return;
    }
    let bg = effective_bg(t, || sample_background(img, rect));
    let patch = render_region_patch(
        rect.w as u32,
        rect.h as u32,
        bg,
        t,
        regular,
        bold,
        px_per_pt,
    );
    for (dx, dy, px) in patch.enumerate_pixels() {
        let x = rect.x + dx as i64;
        let y = rect.y + dy as i64;
        if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
            img.put_pixel(x as u32, y as u32, *px);
        }
    }
}

/// Wrap `text` to the rect width at the largest size (capped) that fits the
/// height, then rasterize it into the rect with the given horizontal alignment.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    img: &mut RgbaImage,
    rect: PixelRect,
    text: &str,
    font: &FontRef,
    color: Rgba<u8>,
    justify: Justify,
    size_pt: Option<f32>,
    px_per_pt: f32,
) {
    if text.is_empty() {
        return;
    }
    let pad = 2.0_f32;
    let max_w = (rect.w as f32 - 2.0 * pad).max(1.0);
    let max_h = (rect.h as f32 - 2.0 * pad).max(1.0);

    // Starting size, in buffer pixels: the user's explicit point size if set,
    // else the region height (no fixed ceiling — big regions render big). Either
    // way the loop below shrinks it until the wrapped text fits the box.
    let mut size = match size_pt {
        Some(pt) => (pt * px_per_pt).max(1.0),
        None => max_h,
    };
    let lines = loop {
        let lines = wrap_lines(text, font, size, max_w);
        let scaled = font.as_scaled(PxScale::from(size));
        let line_h = scaled.height() + scaled.line_gap();
        if line_h * lines.len() as f32 <= max_h || size <= 6.0 {
            break lines;
        }
        size *= 0.9;
    };

    let scaled = font.as_scaled(PxScale::from(size));
    let line_h = scaled.height() + scaled.line_gap();
    let measure = |s: &str| -> f32 { s.chars().map(|c| scaled.h_advance(font.glyph_id(c))).sum() };
    let mut baseline_y = rect.y as f32 + pad + scaled.ascent();
    for line in lines {
        // Offset the line's start by its width per the alignment.
        let slack = (max_w - measure(&line)).max(0.0);
        let indent = match justify {
            Justify::Left => 0.0,
            Justify::Center => slack / 2.0,
            Justify::Right => slack,
        };
        let mut pen_x = rect.x as f32 + pad + indent;
        for ch in line.chars() {
            let id = font.glyph_id(ch);
            let glyph: Glyph =
                id.with_scale_and_position(PxScale::from(size), point(pen_x, baseline_y));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let x = bounds.min.x as i64 + gx as i64;
                    let y = bounds.min.y as i64 + gy as i64;
                    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
                        blend(img.get_pixel_mut(x as u32, y as u32), color, coverage);
                    }
                });
            }
            pen_x += scaled.h_advance(id);
        }
        baseline_y += line_h;
    }
}

/// Greedy word-wrap (respecting existing newlines) at the given size.
fn wrap_lines(text: &str, font: &FontRef, size: f32, max_w: f32) -> Vec<String> {
    let scaled = font.as_scaled(PxScale::from(size));
    let measure = |s: &str| -> f32 { s.chars().map(|c| scaled.h_advance(font.glyph_id(c))).sum() };
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if measure(&candidate) <= max_w || line.is_empty() {
                line = candidate;
            } else {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        lines.push(line);
    }
    lines
}

fn blend(px: &mut Rgba<u8>, color: Rgba<u8>, coverage: f32) {
    let a = coverage.clamp(0.0, 1.0);
    for i in 0..3 {
        px[i] = (color[i] as f32 * a + px[i] as f32 * (1.0 - a)).round() as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Justify, RegionStyle, Rotate, SourceMethod, Translated};
    use crate::document::{Document, Rect};
    use crate::translate::TranslationResult;

    fn styled(translation: &str, style: RegionStyle) -> Translated {
        Translated {
            result: TranslationResult {
                source_text: "src".into(),
                translation: translation.into(),
                alternatives: Vec::new(),
                explanation: None,
            },
            method: SourceMethod::TextLayer,
            style,
        }
    }

    #[test]
    fn patch_honors_style_overrides() {
        let font = crate::fonts::weighted(false);
        let bold = crate::fonts::weighted(true);

        // Edited text overrides the machine translation; background override is
        // used verbatim; corners stay the background color (no glyph there).
        let t = styled(
            "machine",
            RegionStyle {
                text: Some("EDITED".into()),
                background: Some([10, 20, 30]),
                justify: Justify::Right,
                rotate: Rotate::None,
                bold: true,
                size_pt: Some(12.0),
            },
        );
        assert_eq!(t.display_text(), "EDITED");
        let patch = render_region_patch(120, 40, Rgba([10, 20, 30, 255]), &t, &font, &bold, 2.0);
        assert_eq!(patch.dimensions(), (120, 40));
        assert_eq!(*patch.get_pixel(0, 0), Rgba([10, 20, 30, 255]));

        // Rotation (either direction) lays text out swapped then rotates back to
        // fill the region, so the returned patch still matches the region size.
        for rotate in [Rotate::Cw, Rotate::Ccw] {
            let rotated = styled(
                "hello",
                RegionStyle {
                    rotate,
                    ..Default::default()
                },
            );
            let patch = render_region_patch(
                120,
                40,
                Rgba([255, 255, 255, 255]),
                &rotated,
                &font,
                &bold,
                2.0,
            );
            assert_eq!(patch.dimensions(), (120, 40));
        }
    }

    #[test]
    fn exports_a_valid_pdf() {
        // A minimal one-page source PDF to open + render.
        let src = std::env::temp_dir().join("expat-junk-export-src.pdf");
        let bytes = printpdf::PdfDocument::new("src")
            .with_pages(vec![printpdf::PdfPage::new(
                printpdf::Mm(210.0),
                printpdf::Mm(297.0),
                Vec::new(),
            )])
            .save(&printpdf::PdfSaveOptions::default(), &mut Vec::new());
        std::fs::write(&src, bytes).unwrap();

        let mut doc = Document::open(&src).expect("open source");
        let id = doc.add_region(
            0,
            Rect {
                x: 50.0,
                y: 50.0,
                w: 240.0,
                h: 48.0,
            },
        );
        let mut results = HashMap::new();
        results.insert(
            id,
            Translated {
                result: TranslationResult {
                    source_text: "こんにちは世界".to_string(),
                    translation: "Hello world, this is a longer line to exercise wrapping"
                        .to_string(),
                    alternatives: Vec::new(),
                    explanation: None,
                },
                method: SourceMethod::TextLayer,
                style: Default::default(),
            },
        );

        let out = std::env::temp_dir().join("expat-junk-export-out.pdf");
        export_raster(&doc, &results, &out).expect("export");
        let written = std::fs::read(&out).expect("read output");
        assert!(written.starts_with(b"%PDF"), "output is not a PDF");
        assert!(
            written.len() > 1000,
            "output suspiciously small: {}",
            written.len()
        );

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&out);
    }
}

#[cfg(test)]
mod orientation_test {
    use super::*;
    use crate::document::Document;

    #[test]
    fn export_is_not_vertically_flipped() {
        let src = std::env::temp_dir().join("expat-junk-orient-src.pdf");
        let bytes = printpdf::PdfDocument::new("src")
            .with_pages(vec![printpdf::PdfPage::new(
                printpdf::Mm(210.0),
                printpdf::Mm(297.0),
                Vec::new(),
            )])
            .save(&printpdf::PdfSaveOptions::default(), &mut Vec::new());
        std::fs::write(&src, bytes).unwrap();

        let mut doc = Document::open(&src).unwrap();
        // Paint a red marker across the top-left tenth of the page bitmap.
        let (bw, bh) = (doc.pages[0].bitmap.width(), doc.pages[0].bitmap.height());
        for y in 0..bh / 10 {
            for x in 0..bw / 10 {
                doc.pages[0].bitmap.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }

        let out = std::env::temp_dir().join("expat-junk-orient-out.pdf");
        export_raster(&doc, &HashMap::new(), &out).unwrap();

        // Re-render the exported PDF and confirm the marker is still at the top.
        let rendered = junk_libs_platen::render_pdf(&out, 1.0).unwrap();
        let img = &rendered[0].image;
        let (rw, rh) = (img.width(), img.height());
        let top_left = img.get_pixel(rw / 20, rh / 20);
        let bottom_left = img.get_pixel(rw / 20, rh - rh / 20);

        assert!(
            top_left[0] > 150 && top_left[1] < 100,
            "top-left not red — export is vertically flipped: {top_left:?}"
        );
        assert!(
            !(bottom_left[0] > 150 && bottom_left[1] < 100),
            "bottom-left is red — export is vertically flipped: {bottom_left:?}"
        );

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&out);
    }
}

#[cfg(test)]
mod vector_test {
    use super::*;
    use crate::app::{SourceMethod, Translated};
    use crate::document::{Document, Rect};
    use crate::translate::TranslationResult;

    #[test]
    fn vector_export_overlays_and_stays_valid() {
        let src = std::env::temp_dir().join("expat-junk-vec-src.pdf");
        let bytes = printpdf::PdfDocument::new("src")
            .with_pages(vec![printpdf::PdfPage::new(
                printpdf::Mm(210.0),
                printpdf::Mm(297.0),
                Vec::new(),
            )])
            .save(&printpdf::PdfSaveOptions::default(), &mut Vec::new());
        std::fs::write(&src, bytes).unwrap();

        let mut doc = Document::open(&src).unwrap();
        // A region near the top of the page (y small = near top).
        let id = doc.add_region(
            0,
            Rect {
                x: 60.0,
                y: 60.0,
                w: 240.0,
                h: 40.0,
            },
        );
        let mut results = HashMap::new();
        results.insert(
            id,
            Translated {
                result: TranslationResult {
                    source_text: "こんにちは".to_string(),
                    translation: "Hello world overlay text".to_string(),
                    alternatives: Vec::new(),
                    explanation: None,
                },
                method: SourceMethod::TextLayer,
                style: Default::default(),
            },
        );

        let out = std::env::temp_dir().join("expat-junk-vec-out.pdf");
        export_vector(&doc, &src, &results, &out).expect("vector export");

        // Valid PDF, re-loadable, page count preserved.
        assert!(std::fs::read(&out).unwrap().starts_with(b"%PDF"));
        assert_eq!(lopdf::Document::load(&out).unwrap().get_pages().len(), 1);

        // Re-render and confirm the overlay (black text on white) landed near the
        // TOP, not flipped to the bottom.
        let rendered = junk_libs_platen::render_pdf(&out, 1.0).unwrap();
        let img = &rendered[0].image;
        let sx = img.width() as f32 / 595.0;
        let sy = img.height() as f32 / 842.0;
        let dark = |x0: f32, y0: f32, w: f32, h: f32| {
            let mut n = 0;
            for y in (y0 * sy) as u32..((y0 + h) * sy) as u32 {
                for x in (x0 * sx) as u32..((x0 + w) * sx) as u32 {
                    if x < img.width() && y < img.height() {
                        let p = img.get_pixel(x, y);
                        if p[0] < 100 && p[1] < 100 && p[2] < 100 {
                            n += 1;
                        }
                    }
                }
            }
            n
        };
        let top = dark(60.0, 60.0, 240.0, 40.0);
        let bottom = dark(60.0, 842.0 - 100.0, 240.0, 40.0);
        assert!(top > 0, "no overlay text rendered in the region");
        assert!(
            top > bottom,
            "overlay appears flipped (top={top}, bottom={bottom})"
        );

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&out);
    }
}

#[cfg(test)]
mod rerender_test {
    use crate::document::Document;

    #[test]
    fn rerender_page_increases_resolution() {
        let src = std::env::temp_dir().join("expat-junk-rerender-src.pdf");
        let bytes = printpdf::PdfDocument::new("src")
            .with_pages(vec![printpdf::PdfPage::new(
                printpdf::Mm(210.0),
                printpdf::Mm(297.0),
                Vec::new(),
            )])
            .save(&printpdf::PdfSaveOptions::default(), &mut Vec::new());
        std::fs::write(&src, bytes).unwrap();

        let mut doc = Document::open(&src).unwrap();
        let base_w = doc.pages[0].bitmap.width();

        // Re-render at 4 px/pt; an A4 (595pt wide) should reach ~2380px.
        doc.rerender_page(0, 4.0).unwrap();
        let hi_w = doc.pages[0].bitmap.width();
        assert!(hi_w > base_w, "hi-res not larger: {hi_w} vs {base_w}");
        let expected = (doc.pages[0].size.0 * 4.0) as u32;
        assert!(
            hi_w + 4 >= expected.min(4096),
            "expected ~{expected}px wide, got {hi_w}"
        );

        let _ = std::fs::remove_file(&src);
    }
}
