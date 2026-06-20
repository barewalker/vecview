//! E2E: load a real Typst-produced SVG, render it, and save the result as a PNG.
//! For visual inspection. `VECVIEW_E2E_SVG` sets the input SVG and `VECVIEW_E2E_PNG` the output path.

use vecview_core::Document;
use vecview_renderer::Renderer;
use vecview_svg::SvgDocument;

#[test]
fn render_svg_to_png() {
    let svg = std::env::var("VECVIEW_E2E_SVG")
        .unwrap_or_else(|_| "/tmp/vecview-sample/sample-1.svg".to_string());
    let png = std::env::var("VECVIEW_E2E_PNG")
        .unwrap_or_else(|_| "/tmp/vecview-sample/render.png".to_string());

    if !std::path::Path::new(&svg).exists() {
        eprintln!("skipping: no input SVG: {svg}");
        return;
    }

    let doc = SvgDocument::open(&svg).expect("SVG open");
    let page = doc.render_page(0).expect("render_page");
    eprintln!(
        "page = {}x{}, commands = {}",
        page.width,
        page.height,
        page.commands.len()
    );

    // Render at 4x the page width (to check vector quality, i.e. high-resolution rasterization).
    let scale = 4.0;
    let w = (page.width * scale).round() as u32;
    let h = (page.height * scale).round() as u32;

    let renderer = Renderer::new().expect("renderer");
    eprintln!("GPU: {}", renderer.adapter_info);
    let rgba = renderer
        .render(&page, w, h, [0.0, 0.0, page.width, page.height])
        .expect("render");

    image::save_buffer(&png, &rgba, w, h, image::ColorType::Rgba8).expect("save png");
    eprintln!("saved: {png} ({w}x{h})");

    // Zoom check: render just a small region at the top-left of the page (around the title) at high resolution.
    let zoom_png = "/tmp/vecview-sample/render-zoom.png";
    let (zw, zh) = (800u32, 600u32);
    // Viewport: roughly x=8..88, y=14..74 (266x186 in page coordinates).
    let vp = [8.0, 14.0, 80.0, 60.0];
    let zoomed = renderer.render(&page, zw, zh, vp).expect("zoom render");
    image::save_buffer(zoom_png, &zoomed, zw, zh, image::ColorType::Rgba8).expect("save zoom png");
    eprintln!("saved: {zoom_png} ({zw}x{zh}) viewport={vp:?}");

    // Letterbox check: "fit" a wide page into a tall output and verify dark bands appear top and bottom.
    let lb_png = "/tmp/vecview-sample/render-letterbox.png";
    let (lw, lh) = (400u32, 800u32);
    // Fit: width-limited. vw=page.width, vh=lh/(lw/page.width), centered vertically.
    let s = lw as f32 / page.width;
    let vh = lh as f32 / s;
    let lb_vp = [0.0, (page.height - vh) / 2.0, page.width, vh];
    let lb = renderer.render(&page, lw, lh, lb_vp).expect("letterbox render");
    image::save_buffer(lb_png, &lb, lw, lh, image::ColorType::Rgba8).expect("save lb png");
    eprintln!("saved: {lb_png} ({lw}x{lh}) viewport={lb_vp:?}");
}
