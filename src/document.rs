//! Document model + rendering.
//!
//! The model and its PDF/image loading now live in the shared
//! `junk-libs-egui-pdfdoc` crate (a [`Document`] of [`Page`]s with [`Region`]s,
//! implementing the docview widget's `PageModel`); the geometry types live in
//! `junk-libs-egui-docview`. We re-export them here so the rest of the app keeps
//! referring to `crate::document::{Document, Page, Region, Rect, RegionId}`.
//!
//! Per-region application data (translations, etc.) is not stored on the document
//! — it's keyed by [`RegionId`] in the app and project (see `app::Translated`).

// `Rect` is part of this module's canonical surface (tests refer to
// `crate::document::Rect`) but isn't named by non-test code, hence the allow.
#[allow(unused_imports)]
pub use junk_libs_egui_docview::{Rect, RegionId};
pub use junk_libs_egui_pdfdoc::{Document, Region};
