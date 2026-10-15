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
//! Both use the shared Unicode shaping/raster engine with explicit Noto fallback
//! fonts, bidi layout, wrapping and auto-fit. Background matching is a
//! flat fill, so photo/textured backgrounds show a seam ("no loss if we can't").

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use lopdf::{Dictionary, Object, Stream};
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Pt, RawImage, XObjectTransform};

use junk_libs_raster::{PixelRect, sample_background};

use crate::document::{Document, Rect, RegionId};
use crate::translation::{Justify, Rotate, Translated};

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
    anyhow::ensure!(
        !same_file(source_path, out_path),
        "export destination must differ from the source document"
    );
    doc.verify_source(source_path)?;
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
/// translated text) placed in the displayed CropBox, accounting for inherited
/// page geometry, offsets and rotation. Resources are detached before editing.
pub fn export_vector(
    doc: &Document,
    source_path: &Path,
    results: &HashMap<RegionId, Translated>,
    out_path: &Path,
) -> Result<()> {
    doc.verify_source(source_path)?;

    let mut pdf = lopdf::Document::load(source_path)
        .with_context(|| format!("loading source PDF {}", source_path.display()))?;
    anyhow::ensure!(
        !same_file(source_path, out_path),
        "export destination must differ from the source document"
    );
    let page_ids: Vec<lopdf::ObjectId> = pdf.get_pages().into_values().collect();
    anyhow::ensure!(
        page_ids.len() == doc.pages.len(),
        "source PDF page count changed; reopen the document before exporting"
    );

    let mut overlay_n = 0u32;
    for (page_index, _info) in doc.pages.iter().enumerate() {
        let Some(&page_id) = page_ids.get(page_index) else {
            continue;
        };
        if !doc
            .regions
            .iter()
            .any(|r| r.page == page_index && results.contains_key(&r.id))
        {
            continue;
        }
        let page = doc.render_page(page_index, 2.0)?;
        let geometry = PageGeometry::read(&pdf, page_id)?;
        let displayed = geometry.displayed_size();
        anyhow::ensure!(
            (displayed.0 - page.size.0).abs() < 0.1 && (displayed.1 - page.size.1).abs() < 0.1,
            "source PDF geometry changed or is unsupported; reopen the document before exporting"
        );
        prepare_page(&mut pdf, page_id)?;
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
            let (pw, ph, rgb) = render_patch(region.rect.w, region.rect.h, bg, t);

            let img_id = add_image_xobject(&mut pdf, pw, ph, rgb);
            overlay_n += 1;
            let mut name = format!("XjTr{overlay_n}");
            let resources = pdf.get_dictionary(page_id)?.get(b"Resources")?.as_dict()?;
            let xobjects = resources.get(b"XObject")?.as_dict()?;
            while xobjects.has(name.as_bytes()) {
                overlay_n += 1;
                name = format!("XjTr{overlay_n}");
            }
            pdf.add_xobject(page_id, name.clone().into_bytes(), img_id)
                .with_context(|| format!("adding overlay to page {page_index}"))?;

            let [a, b, c, d, e, f] = geometry.patch_matrix(region.rect);
            let ops = format!("q {a:.5} {b:.5} {c:.5} {d:.5} {e:.5} {f:.5} cm /{name} Do Q\n");
            pdf.add_page_contents(page_id, ops.into_bytes())
                .with_context(|| format!("drawing overlay on page {page_index}"))?;
        }
    }

    let mut bytes = Vec::new();
    pdf.save_to(&mut bytes)
        .context("serializing translated PDF")?;
    crate::persistence::write(out_path, &bytes)?;
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Resolve the nearest page-tree property, including indirect values.
fn inherited(pdf: &lopdf::Document, mut id: lopdf::ObjectId, key: &[u8]) -> Result<Option<Object>> {
    let mut seen = std::collections::HashSet::new();
    loop {
        anyhow::ensure!(
            seen.insert(id) && seen.len() <= 128,
            "invalid PDF page tree"
        );
        let dict = pdf.get_dictionary(id)?;
        if let Ok(value) = dict.get(key) {
            return Ok(Some(pdf.dereference(value)?.1.clone()));
        }
        match dict.get(b"Parent") {
            Ok(parent) => id = parent.as_reference()?,
            Err(_) => return Ok(None),
        }
    }
}

struct PageGeometry {
    bounds: [f32; 4],
    rotation: i64,
}

impl PageGeometry {
    fn read(pdf: &lopdf::Document, id: lopdf::ObjectId) -> Result<Self> {
        let read_box = |key: &[u8]| -> Result<Option<[f32; 4]>> {
            inherited(pdf, id, key)?
                .map(|o| {
                    let array = o.as_array()?;
                    anyhow::ensure!(array.len() == 4, "invalid PDF page box");
                    Ok([
                        array[0].as_float()?,
                        array[1].as_float()?,
                        array[2].as_float()?,
                        array[3].as_float()?,
                    ])
                })
                .transpose()
        };
        let media = read_box(b"MediaBox")?.context("PDF page has no MediaBox")?;
        let crop = read_box(b"CropBox")?.unwrap_or(media);
        let bounds = [
            media[0].max(crop[0]),
            media[1].max(crop[1]),
            media[2].min(crop[2]),
            media[3].min(crop[3]),
        ];
        anyhow::ensure!(
            bounds.iter().all(|n| n.is_finite()) && bounds[2] > bounds[0] && bounds[3] > bounds[1],
            "invalid PDF page bounds"
        );
        let rotation = inherited(pdf, id, b"Rotate")?
            .map(|o| o.as_i64())
            .transpose()?
            .unwrap_or(0)
            .rem_euclid(360);
        anyhow::ensure!(rotation % 90 == 0, "unsupported PDF page rotation");
        // The current render engine does not model UserUnit in its page sizes.
        // Refuse rather than silently mis-size overlays in other PDF viewers.
        let unit = inherited(pdf, id, b"UserUnit")?
            .map(|o| o.as_float())
            .transpose()?
            .unwrap_or(1.0);
        anyhow::ensure!(
            (unit - 1.0).abs() < f32::EPSILON,
            "PDF UserUnit other than 1 is not supported for vector export"
        );
        Ok(Self { bounds, rotation })
    }

    fn displayed_size(&self) -> (f32, f32) {
        let [x0, y0, x1, y1] = self.bounds;
        if self.rotation == 90 || self.rotation == 270 {
            (y1 - y0, x1 - x0)
        } else {
            (x1 - x0, y1 - y0)
        }
    }

    fn patch_matrix(&self, rect: Rect) -> [f32; 6] {
        let [x0, y0, x1, y1] = self.bounds;
        let (width, height) = (x1 - x0, y1 - y0);
        let (x, y) = (rect.x, self.displayed_size().1 - rect.y - rect.h);
        let (w, h) = (rect.w, rect.h);
        match self.rotation {
            90 => [0.0, w, -h, 0.0, x0 + width - y, y0 + x],
            180 => [-w, 0.0, 0.0, -h, x0 + width - x, y0 + height - y],
            270 => [0.0, -w, h, 0.0, x0 + y, y0 + height - x],
            _ => [w, 0.0, 0.0, h, x0 + x, y0 + y],
        }
    }
}

fn prepare_page(pdf: &mut lopdf::Document, id: lopdf::ObjectId) -> Result<()> {
    let mut resources = inherited(pdf, id, b"Resources")?
        .map(|o| o.as_dict().cloned())
        .transpose()?
        .unwrap_or_default();
    let xobjects = resources
        .get(b"XObject")
        .ok()
        .map(|o| -> Result<Dictionary> { Ok(pdf.dereference(o)?.1.as_dict()?.clone()) })
        .transpose()?
        .unwrap_or_default();
    resources.set("XObject", xobjects);
    let existing = pdf.get_page_contents(id);
    let prefix = pdf.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
    let suffix = pdf.add_object(Stream::new(Dictionary::new(), b"\nQ\n".to_vec()));
    let mut contents = vec![Object::Reference(prefix)];
    contents.extend(existing.into_iter().map(Object::Reference));
    contents.push(Object::Reference(suffix));
    let page = pdf.get_object_mut(id)?.as_dict_mut()?;
    page.set("Resources", resources);
    page.set("Contents", contents);
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
fn render_patch(w_pts: f32, h_pts: f32, bg: Rgba<u8>, t: &Translated) -> (u32, u32, Vec<u8>) {
    const DPI: f32 = 200.0;
    let px_per_pt = (DPI / POINTS_PER_INCH)
        .min(junk_libs_platen::MAX_RENDER_DIMENSION as f32 / w_pts.max(h_pts).max(1.0));
    let pw = ((w_pts * px_per_pt).round() as u32).max(1);
    let ph = ((h_pts * px_per_pt).round() as u32).max(1);
    let patch = render_region_patch(pw, ph, bg, t, px_per_pt);

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
    px_per_pt: f32,
) -> RgbaImage {
    // When rotating, lay the text out in a buffer with width/height swapped, then
    // rotate it back so it fills the region.
    let (bw, bh) = if t.style.rotate.is_sideways() {
        (h_px.max(1), w_px.max(1))
    } else {
        (w_px.max(1), h_px.max(1))
    };

    let lum = 0.299 * bg[0] as f32 + 0.587 * bg[1] as f32 + 0.114 * bg[2] as f32;
    let color = if lum > 140.0 {
        Rgba([0, 0, 0, 255])
    } else {
        Rgba([255, 255, 255, 255])
    };
    let patch = crate::fonts::with_renderer(|renderer| {
        renderer
            .render(
                t.display_text().trim(),
                bw,
                bh,
                bg.0,
                color.0,
                junk_libs_text::TextStyle {
                    alignment: match t.style.justify {
                        Justify::Left => junk_libs_text::Alignment::Left,
                        Justify::Center => junk_libs_text::Alignment::Center,
                        Justify::Right => junk_libs_text::Alignment::Right,
                    },
                    bold: t.style.bold,
                    size_px: t.style.size_pt.map(|s| s * px_per_pt),
                    padding_px: px_per_pt,
                },
            )
            .image
    });

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
    let mut pdf = PdfDocument::new("expat-junk translation");
    let mut pages = Vec::with_capacity(doc.pages.len());

    for page_index in 0..doc.pages.len() {
        let page = doc.render_page(page_index, 2.0)?;
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
            bake_region(&mut img, rect, t, scale_x);
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
    crate::persistence::write(out_path, &bytes)?;
    Ok(())
}

fn pt_to_mm(pt: f32) -> f32 {
    pt / POINTS_PER_INCH * 25.4
}

/// Render the region's styled translation patch and composite it (opaque) over
/// the source at `rect`.
fn bake_region(img: &mut RgbaImage, rect: PixelRect, t: &Translated, px_per_pt: f32) {
    if rect.w <= 0 || rect.h <= 0 {
        return;
    }
    let bg = effective_bg(t, || sample_background(img, rect));
    let patch = render_region_patch(rect.w as u32, rect.h as u32, bg, t, px_per_pt);
    for (dx, dy, px) in patch.enumerate_pixels() {
        let x = rect.x + dx as i64;
        let y = rect.y + dy as i64;
        if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
            img.put_pixel(x as u32, y as u32, *px);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, Rect};
    use crate::translate::TranslationResult;
    use crate::translation::{Justify, RegionStyle, Rotate, SourceMethod, Translated};
    use lopdf::dictionary;

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
            settings: None,
        }
    }

    #[test]
    fn shapes_all_offered_scripts_and_wraps_cjk() {
        crate::fonts::with_renderer(|renderer| {
            for text in [
                "これは改行のない長い日本語の文章です",
                "안녕하세요 세계",
                "مرحبا بالعالم",
                "שלום עולם",
                "नमस्ते दुनिया",
                "สวัสดีชาวโลก",
            ] {
                let output =
                    renderer.render(text, 120, 80, [255; 4], [0, 0, 0, 255], Default::default());
                assert_eq!(output.layout.missing_glyphs, 0, "{text}");
                assert!(!output.layout.overflow, "{text}");
            }
        });
    }

    #[test]
    fn vector_export_handles_inherited_geometry_resources_and_graphics_state() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.pdf");
        let out = dir.path().join("translated.pdf");
        for rotation in [0_i64, 90, 180, 270] {
            let mut pdf = lopdf::Document::with_version("1.5");
            let pages_id = pdf.new_object_id();
            // An inherited font must survive creating local overlay resources.
            let font_id = pdf.add_object(lopdf::dictionary! {"Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica"});
            let resources = lopdf::dictionary! {"Font" => lopdf::dictionary! {"F1" => font_id}};
            // Leave a nonidentity CTM at the end; overlays must start from the
            // initial graphics state. Original text remains selectable.
            let content = pdf.add_object(Stream::new(
                Dictionary::new(),
                b"BT /F1 10 Tf 30 50 Td (Original) Tj ET 2 0 0 2 10 20 cm\n".to_vec(),
            ));
            let page_id = pdf.add_object(
                lopdf::dictionary! {"Type" => "Page", "Parent" => pages_id, "Contents" => content},
            );
            let second_id = pdf.add_object(
                lopdf::dictionary! {"Type" => "Page", "Parent" => pages_id, "Contents" => content},
            );
            pdf.objects.insert(pages_id, lopdf::dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into(), second_id.into()], "Count" => 2,
                "MediaBox" => vec![10.into(), 20.into(), 230.into(), 320.into()],
                "CropBox" => vec![20.into(), 40.into(), 220.into(), 280.into()],
                "Rotate" => rotation, "Resources" => resources,
            }.into());
            let catalog =
                pdf.add_object(lopdf::dictionary! {"Type" => "Catalog", "Pages" => pages_id});
            pdf.trailer.set("Root", catalog);
            pdf.save(&src).unwrap();
            let original_bytes = std::fs::read(&src).unwrap();
            let mut doc = Document::open(&src).unwrap();
            let id = doc.add_region(
                0,
                Rect {
                    x: 15.0,
                    y: 25.0,
                    w: 65.0,
                    h: 35.0,
                },
            );
            let t = styled(
                "Hello",
                RegionStyle {
                    background: Some([250, 0, 0]),
                    ..Default::default()
                },
            );
            let results = HashMap::from([(id, t)]);
            assert!(export_vector(&doc, &src, &results, &src).is_err());
            assert_eq!(std::fs::read(&src).unwrap(), original_bytes);
            export_vector(&doc, &src, &results, &out).unwrap();
            let loaded = lopdf::Document::load(&out).unwrap();
            assert_eq!(loaded.get_pages().len(), 2);
            assert!(loaded.extract_text(&[1]).unwrap().contains("Original"));
            let rendered = junk_libs_platen::render_pdf(&out, 2.0).unwrap();
            let image = &rendered[0].image;
            // Interior of the region, away from text: displayed geometry stays
            // identical for all four page rotations and nonzero CropBox origins.
            let inside = image.get_pixel(150, 110);
            assert!(
                inside[0] > 200 && inside[1] < 40,
                "rotation {rotation}: {inside:?}"
            );
            assert!(image.get_pixel(10, 10)[1] > 200);
            assert!(rendered[1].image.get_pixel(150, 110)[1] > 200);
        }
    }

    #[test]
    fn patch_honors_style_overrides() {
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
        let patch = render_region_patch(120, 40, Rgba([10, 20, 30, 255]), &t, 2.0);
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
            let patch = render_region_patch(120, 40, Rgba([255, 255, 255, 255]), &rotated, 2.0);
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
                settings: None,
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
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("marker.png");
        let mut image = RgbaImage::from_pixel(595, 842, Rgba([255; 4]));
        for y in 0..84 {
            for x in 0..59 {
                image.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
        image.save(&src).unwrap();
        let doc = Document::open(&src).unwrap();

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
    use crate::document::{Document, Rect};
    use crate::translate::TranslationResult;
    use crate::translation::{SourceMethod, Translated};

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
                settings: None,
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

        let doc = Document::open(&src).unwrap();
        let base_w = doc.render_page(0, 2.0).unwrap().bitmap.width();

        // Re-render at 4 px/pt; an A4 (595pt wide) should reach ~2380px.
        let hi_w = doc.render_page(0, 4.0).unwrap().bitmap.width();
        assert!(hi_w > base_w, "hi-res not larger: {hi_w} vs {base_w}");
        let expected = (doc.pages[0].size.0 * 4.0) as u32;
        assert!(
            hi_w + 4 >= expected.min(4096),
            "expected ~{expected}px wide, got {hi_w}"
        );

        let _ = std::fs::remove_file(&src);
    }
}
