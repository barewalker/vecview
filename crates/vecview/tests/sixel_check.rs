//! Round-trip verification of sixel encode/decode. If a real PDF (VECVIEW_TEST_PDF) is available, check quality on an actual page.
use icy_sixel::{sixel_encode, EncodeOptions, SixelImage};

#[test]
fn sixel_roundtrip() {
    // Minimal verification even with a synthetic image (red/green/blue bands plus white).
    let (w, h) = (64usize, 48usize);
    let mut rgba = vec![255u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let c = match x / 16 { 0 => [220, 30, 30], 1 => [30, 200, 60], 2 => [40, 80, 220], _ => [255, 255, 255] };
            rgba[i] = c[0]; rgba[i+1] = c[1]; rgba[i+2] = c[2]; rgba[i+3] = 255;
        }
    }
    let s = sixel_encode(&rgba, w, h, &EncodeOptions::default()).expect("encode");
    assert!(s.starts_with('\u{1b}') && s.as_bytes()[1] == b'P', "starts with DCS");
    assert!(s.ends_with("\u{1b}\\"), "ends with ST");
    let img = SixelImage::decode(s.as_bytes()).expect("decode");
    assert_eq!((img.width, img.height), (w, h));
    eprintln!("synthetic ok: {}x{} pixels_len={}", img.width, img.height, img.pixels.len());

    // If a real page is available, encode then decode and save as PNG (an approximation of how it looks in WT).
    if let Ok(pdf_path) = std::env::var("VECVIEW_TEST_PDF") {
        let pdf = vecview_pdf::Pdf::open(std::path::Path::new(&pdf_path)).unwrap();
        let idx: usize = std::env::var("VECVIEW_TEST_PAGE").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
        let (pw, ph) = pdf.page_size(idx).unwrap();
        let (ow, oh) = (1200u32, (1200.0 * ph / pw) as u32);
        let page_rgba = pdf.render(idx, [0.0, 0.0, pw, ph], ow, oh).unwrap();
        let s = sixel_encode(&page_rgba, ow as usize, oh as usize, &EncodeOptions::default()).unwrap();
        eprintln!("page sixel bytes = {}", s.len());
        let img = SixelImage::decode(s.as_bytes()).unwrap();
        let ch = img.pixels.len() / (img.width * img.height);
        eprintln!("decoded {}x{} channels={}", img.width, img.height, ch);
        let color = if ch == 4 { image::ColorType::Rgba8 } else { image::ColorType::Rgb8 };
        image::save_buffer("/tmp/sixel_roundtrip.png", &img.pixels, img.width as u32, img.height as u32, color).unwrap();
        eprintln!("saved /tmp/sixel_roundtrip.png");
    }
}
