//! Host adapter for the shared document model. Zoom renders run on one worker
//! at a time; rapid zooms replace the queued request instead of spawning threads.

use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use junk_libs_egui_docview::PageModel;
pub use junk_libs_egui_docview::{Rect, RegionId};
pub use junk_libs_egui_pdfdoc::Region;

pub struct Document {
    inner: junk_libs_egui_pdfdoc::LazyDocument,
    source_fingerprint: String,
}
impl Deref for Document {
    type Target = junk_libs_egui_pdfdoc::LazyDocument;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl DerefMut for Document {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
impl Document {
    pub fn open(path: &Path) -> Result<Self> {
        let bytes = Arc::new(std::fs::read(path)?);
        let source_fingerprint = fingerprint(&bytes);
        let inner = if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        {
            junk_libs_egui_pdfdoc::LazyDocument::from_pdf_bytes(bytes, Default::default())?
        } else {
            junk_libs_egui_pdfdoc::LazyDocument::open(path)?
        };
        anyhow::ensure!(
            fingerprint(&std::fs::read(path)?) == source_fingerprint,
            "source changed while opening; try again"
        );
        Ok(Self {
            inner,
            source_fingerprint,
        })
    }
    pub fn snapshot(&self) -> Self {
        Self {
            inner: self.inner.snapshot(),
            source_fingerprint: self.source_fingerprint.clone(),
        }
    }
    pub fn source_fingerprint(&self) -> &str {
        &self.source_fingerprint
    }
    pub fn verify_source(&self, path: &Path) -> Result<()> {
        anyhow::ensure!(
            fingerprint(&std::fs::read(path)?) == self.source_fingerprint,
            "source document changed since opening; restore the original source before exporting"
        );
        Ok(())
    }
    pub fn poll_render(&mut self) -> Result<()> {
        self.inner.poll()
    }
    pub fn clamp_regions(&mut self) {
        for region in &mut self.inner.regions {
            if let Some(page) = self.inner.pages.get(region.page) {
                region.rect.w = region.rect.w.clamp(0.1, page.size.0);
                region.rect.h = region.rect.h.clamp(0.1, page.size.1);
                region.rect.x = region.rect.x.clamp(0.0, page.size.0 - region.rect.w);
                region.rect.y = region.rect.y.clamp(0.0, page.size.1 - region.rect.h);
            }
        }
    }
}
impl PageModel for Document {
    fn page_count(&self) -> usize {
        self.inner.page_count()
    }
    fn page_label(&self, page: usize) -> Option<&str> {
        self.inner.page_label(page)
    }
    fn page_size(&self, page: usize) -> Option<(f32, f32)> {
        self.inner.page_size(page)
    }
    fn page_bitmap(&self, page: usize) -> Option<&image::RgbaImage> {
        self.inner.page_bitmap(page)
    }
    fn rerender_page(&mut self, page: usize, scale: f32) {
        self.inner.rerender_page(page, scale);
    }
    fn regions_on(&self, page: usize) -> Vec<(RegionId, Rect)> {
        self.inner.regions_on(page)
    }
    fn add_region(&mut self, page: usize, rect: Rect) -> RegionId {
        self.inner.add_region(page, rect)
    }
    fn region_rect_mut(&mut self, id: RegionId) -> Option<&mut Rect> {
        self.inner.region_rect_mut(id)
    }
    fn remove_region(&mut self, id: RegionId) {
        self.inner.remove_region(id);
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn async_zoom_keeps_only_latest_requested_render() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.pdf");
        let bytes = printpdf::PdfDocument::new("test")
            .with_pages(vec![printpdf::PdfPage::new(
                printpdf::Mm(30.0),
                printpdf::Mm(40.0),
                Vec::new(),
            )])
            .save(&printpdf::PdfSaveOptions::default(), &mut Vec::new());
        std::fs::write(&path, bytes).unwrap();
        let mut doc = Document::open(&path).unwrap();
        PageModel::rerender_page(&mut doc, 0, 1.0);
        PageModel::rerender_page(&mut doc, 0, 3.0);
        PageModel::rerender_page(&mut doc, 0, 4.0);
        assert_eq!(doc.resident_pages(), 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while doc.is_rendering() {
            doc.poll_render().unwrap();
            assert!(std::time::Instant::now() < deadline, "render timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let scale = doc.bitmap(0).unwrap().width() as f32 / doc.pages[0].size.0;
        assert!((scale - 4.0).abs() < 0.02);
    }
}
