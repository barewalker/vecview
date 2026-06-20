//! Smoke test for pdfium binding + rendering. Runs only when libpdfium and VECVIEW_TEST_PDF are present.
use vecview_pdf::Pdf;

#[test]
fn render_viewport_smoke() {
    let Ok(pdf_path) = std::env::var("VECVIEW_TEST_PDF") else {
        eprintln!("skipping because VECVIEW_TEST_PDF is not set");
        return;
    };
    let pdf = Pdf::open(std::path::Path::new(&pdf_path)).expect("open pdf");
    assert!(pdf.page_count() >= 1);
    let (pw, ph) = pdf.page_size(0).expect("page size");
    assert!(pw > 0.0 && ph > 0.0);

    // Render the full surface with a fit-equivalent viewport and confirm the RGBA length matches.
    let (ow, oh) = (800u32, (800.0 * ph / pw) as u32);
    let rgba = pdf.render(0, [0.0, 0.0, pw, ph], ow, oh).expect("render");
    assert_eq!(rgba.len(), (ow * oh * 4) as usize);
}
