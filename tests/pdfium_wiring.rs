//! Guards that the vendored PDFium resolves at runtime when `junk-libs-pdfium`
//! is consumed as a dependency: its build.rs sets `PDFIUM_LIB_DIR` while
//! compiling the crate inside *our* build tree, and `instance()` must find the
//! library there. The crate's own tests cover rendering; this only checks that
//! the binding wiring survives the lift across repos.

#[test]
fn pdfium_binds_from_consumer() {
    assert!(
        junk_libs_pdfium::instance().is_ok(),
        "PDFium failed to bind from the consuming crate"
    );
}
