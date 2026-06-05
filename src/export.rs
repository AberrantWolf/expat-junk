//! Export: bake translated regions onto the rendered pages and write a PDF.
//!
//! Strategy (v1): **overlay on the rendered raster.** For each translated region
//! we sample the surrounding background color, paint a cover rectangle over the
//! source, and draw the translated text on top — wrapped and auto-sized, using
//! the bundled CJK-capable font. Each annotated page is embedded full-size into a
//! PDF page. Untranslated content is preserved as the original raster; vector
//! text is not retained (acceptable per "no loss if we can't"). Background
//! matching is a flat fill, so photo/textured backgrounds will show a seam.

use std::collections::HashMap;
use std::path::Path;

use ab_glyph::{point, Font, FontRef, Glyph, PxScale, ScaleFont};
use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Pt, RawImage, XObjectTransform};

use crate::app::Translated;
use crate::document::{Document, RegionId};

const POINTS_PER_INCH: f32 = 72.0;

/// Bake translations onto each page and write a multi-page PDF to `out_path`.
pub fn export_pdf(
    doc: &Document,
    results: &HashMap<RegionId, Translated>,
    out_path: &Path,
) -> Result<()> {
    let font = FontRef::try_from_slice(crate::fonts::NOTO_SANS_JP)
        .map_err(|e| anyhow::anyhow!("loading export font: {e}"))?;

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
            bake_region(&mut img, rect, t.result.translation.trim(), &font);
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

#[derive(Clone, Copy)]
struct PixelRect {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

/// Cover a region's source with the sampled background color, then draw the
/// translated text on top.
fn bake_region(img: &mut RgbaImage, rect: PixelRect, text: &str, font: &FontRef) {
    if rect.w <= 0 || rect.h <= 0 {
        return;
    }
    let bg = sample_background(img, rect);
    fill_rect(img, rect, bg);

    let lum = 0.299 * bg[0] as f32 + 0.587 * bg[1] as f32 + 0.114 * bg[2] as f32;
    let color = if lum > 140.0 {
        Rgba([0, 0, 0, 255])
    } else {
        Rgba([255, 255, 255, 255])
    };
    draw_text(img, rect, text, font, color);
}

/// Average color of a 1px ring just outside the region (clamped to the image).
fn sample_background(img: &RgbaImage, rect: PixelRect) -> Rgba<u8> {
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    let mut sample = |x: i64, y: i64| {
        if x >= 0 && y >= 0 && x < iw && y < ih {
            let p = img.get_pixel(x as u32, y as u32);
            r += p[0] as u64;
            g += p[1] as u64;
            b += p[2] as u64;
            n += 1;
        }
    };
    for x in rect.x..rect.x + rect.w {
        sample(x, rect.y - 1);
        sample(x, rect.y + rect.h);
    }
    for y in rect.y..rect.y + rect.h {
        sample(rect.x - 1, y);
        sample(rect.x + rect.w, y);
    }
    if n == 0 {
        Rgba([255, 255, 255, 255])
    } else {
        Rgba([(r / n) as u8, (g / n) as u8, (b / n) as u8, 255])
    }
}

fn fill_rect(img: &mut RgbaImage, rect: PixelRect, color: Rgba<u8>) {
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    for y in rect.y.max(0)..(rect.y + rect.h).min(ih) {
        for x in rect.x.max(0)..(rect.x + rect.w).min(iw) {
            img.put_pixel(x as u32, y as u32, color);
        }
    }
}

/// Wrap `text` to the rect width at the largest size (capped) that fits the
/// height, then rasterize it into the rect.
fn draw_text(img: &mut RgbaImage, rect: PixelRect, text: &str, font: &FontRef, color: Rgba<u8>) {
    if text.is_empty() {
        return;
    }
    let pad = 2.0_f32;
    let max_w = (rect.w as f32 - 2.0 * pad).max(1.0);
    let max_h = (rect.h as f32 - 2.0 * pad).max(1.0);

    let mut size = max_h.min(48.0);
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
    let mut baseline_y = rect.y as f32 + pad + scaled.ascent();
    for line in lines {
        let mut pen_x = rect.x as f32 + pad;
        for ch in line.chars() {
            let id = font.glyph_id(ch);
            let glyph: Glyph = id.with_scale_and_position(PxScale::from(size), point(pen_x, baseline_y));
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
    let measure = |s: &str| -> f32 {
        s.chars().map(|c| scaled.h_advance(font.glyph_id(c))).sum()
    };
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
    use crate::app::{SourceMethod, Translated};
    use crate::document::{Document, Rect};
    use crate::translate::TranslationResult;

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
                    translation: "Hello world, this is a longer line to exercise wrapping".to_string(),
                    alternatives: Vec::new(),
                    explanation: None,
                },
                method: SourceMethod::TextLayer,
            },
        );

        let out = std::env::temp_dir().join("expat-junk-export-out.pdf");
        export_pdf(&doc, &results, &out).expect("export");
        let written = std::fs::read(&out).expect("read output");
        assert!(written.starts_with(b"%PDF"), "output is not a PDF");
        assert!(written.len() > 1000, "output suspiciously small: {}", written.len());

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
        export_pdf(&doc, &HashMap::new(), &out).unwrap();

        // Re-render the exported PDF and confirm the marker is still at the top.
        let pdfium = junk_libs_pdfium::instance().unwrap();
        let rendered = junk_libs_pdfium::render_pdf(pdfium, &out, 1.0).unwrap();
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
