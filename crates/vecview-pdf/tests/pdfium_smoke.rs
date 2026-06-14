//! pdfium バインド＋描画のスモークテスト。libpdfium と VECVIEW_TEST_PDF があるときだけ走る。
use vecview_pdf::Pdf;

#[test]
fn render_viewport_smoke() {
    let Ok(pdf_path) = std::env::var("VECVIEW_TEST_PDF") else {
        eprintln!("VECVIEW_TEST_PDF 未指定のためスキップ");
        return;
    };
    let pdf = Pdf::open(std::path::Path::new(&pdf_path)).expect("open pdf");
    assert!(pdf.page_count() >= 1);
    let (pw, ph) = pdf.page_size(0).expect("page size");
    assert!(pw > 0.0 && ph > 0.0);

    // フィット相当のビューポートで全面描画し、RGBA 長が一致することを確認。
    let (ow, oh) = (800u32, (800.0 * ph / pw) as u32);
    let rgba = pdf.render(0, [0.0, 0.0, pw, ph], ow, oh).expect("render");
    assert_eq!(rgba.len(), (ow * oh * 4) as usize);
}
