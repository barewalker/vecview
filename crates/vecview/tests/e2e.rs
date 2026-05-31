//! E2E: 実際の Typst 出力 SVG を読み込み、レンダラーで描画して PNG に保存する。
//! 視覚確認用。`VECVIEW_E2E_SVG` で入力 SVG を、`VECVIEW_E2E_PNG` で出力先を指定できる。

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
        eprintln!("入力 SVG がないためスキップ: {svg}");
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

    // ページ幅を 4 倍に拡大して描画（ベクター品質＝高解像度ラスタライズの確認）。
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

    // ズーム確認：ページ左上の小領域（タイトル付近）だけを高解像度で拡大描画する。
    let zoom_png = "/tmp/vecview-sample/render-zoom.png";
    let (zw, zh) = (800u32, 600u32);
    // ビューポート: x=8..88, y=14..74 あたり（266x186 ページ座標）。
    let vp = [8.0, 14.0, 80.0, 60.0];
    let zoomed = renderer.render(&page, zw, zh, vp).expect("zoom render");
    image::save_buffer(zoom_png, &zoomed, zw, zh, image::ColorType::Rgba8).expect("save zoom png");
    eprintln!("saved: {zoom_png} ({zw}x{zh}) viewport={vp:?}");

    // letterbox 確認：横長ページを縦長出力に「フィット」表示し、上下に暗帯が出るか。
    let lb_png = "/tmp/vecview-sample/render-letterbox.png";
    let (lw, lh) = (400u32, 800u32);
    // フィット: 横律速。vw=page.width、vh=lh/(lw/page.width)、上下センタリング。
    let s = lw as f32 / page.width;
    let vh = lh as f32 / s;
    let lb_vp = [0.0, (page.height - vh) / 2.0, page.width, vh];
    let lb = renderer.render(&page, lw, lh, lb_vp).expect("letterbox render");
    image::save_buffer(lb_png, &lb, lw, lh, image::ColorType::Rgba8).expect("save lb png");
    eprintln!("saved: {lb_png} ({lw}x{lh}) viewport={lb_vp:?}");
}
