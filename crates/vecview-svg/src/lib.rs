//! SVG パーサー。`usvg` で SVG をパースし、フォーマット非依存な [`Page`] に変換する。
//!
//! `usvg` の `Path::data()` は絶対座標（親 transform 適用済み）でセグメントを返すため、
//! 変換側で transform 行列を合成する必要はない。ラスタライズ（resvg）は使わず、
//! ベクター情報（ベジェ曲線）をそのまま [`PathData`] に写し取る。
//!
//! Typst が出力する SVG は文字もパス化されているため、本パーサーは `Node::Path` のみを
//! 扱えば本文・罫線を描画できる。グラデーションは先頭ストップ色で近似し、
//! クリップ/マスク/画像/テキストノードは初回スコープでは無視する。

use anyhow::Result;
use vecview_core::{
    Color, Document, DrawCommand, Fill, FillRule, ImageData, Page, PathData, PathSegment, Stroke,
};

pub struct SvgDocument {
    tree: usvg::Tree,
}

impl SvgDocument {
    /// ファイルパスから読み込む。
    pub fn open(path: &str) -> Result<Self> {
        let data = std::fs::read(path)?;
        Self::from_bytes(&data)
    }

    /// バイト列からパースする。
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let opt = usvg::Options::default();
        let tree = usvg::Tree::from_data(data, &opt)?;
        Ok(Self { tree })
    }
}

impl Document for SvgDocument {
    fn page_count(&self) -> usize {
        1
    }

    fn render_page(&self, _index: usize) -> Result<Page> {
        let size = self.tree.size();
        let mut commands = Vec::new();
        walk(self.tree.root(), &mut commands);
        Ok(Page {
            width: size.width(),
            height: size.height(),
            commands,
        })
    }
}

/// グループを再帰的に走査し、可視パスを `commands` に収集する。
fn walk(group: &usvg::Group, out: &mut Vec<DrawCommand>) {
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => walk(g, out),
            usvg::Node::Path(p) => {
                if let Some(path) = convert_path(p) {
                    out.push(DrawCommand::Path(path));
                }
            }
            // 埋め込みラスター画像（PDF 内のビットマップ図など）をデコードして取り込む。
            // 親グループのソフトマスク/クリップは未対応（当面無視）。
            usvg::Node::Image(img) => {
                if let Some(image) = convert_image(img) {
                    out.push(DrawCommand::Image(image));
                }
            }
            // テキストは初回スコープ外（Typst/PDF の SVG では文字は Path 化されて来る）。
            usvg::Node::Text(_) => {}
        }
    }
}

/// `<image>` ノードを [`ImageData`] に変換する。配置矩形は絶対座標の外接矩形
/// （`abs_bounding_box`）を使う。pdftocairo の画像は軸平行なのでこれで一致する。
/// 埋め込みデータ（PNG/JPEG/GIF/WebP）は `image` クレートで RGBA8 にデコードする。
fn convert_image(img: &usvg::Image) -> Option<ImageData> {
    if !img.is_visible() {
        return None;
    }
    let bytes: &[u8] = match img.kind() {
        usvg::ImageKind::JPEG(d)
        | usvg::ImageKind::PNG(d)
        | usvg::ImageKind::GIF(d)
        | usvg::ImageKind::WEBP(d) => d,
        // ネストされた SVG 画像は別途ベクター描画が必要なため当面未対応。
        usvg::ImageKind::SVG(_) => return None,
    };
    let decoded = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (px_width, px_height) = decoded.dimensions();
    let bbox = img.abs_bounding_box();
    Some(ImageData {
        rgba: decoded.into_raw(),
        px_width,
        px_height,
        rect: [bbox.x(), bbox.y(), bbox.width(), bbox.height()],
    })
}

fn convert_path(path: &usvg::Path) -> Option<PathData> {
    if !path.is_visible() {
        return None;
    }
    // usvg の `data()` はローカル座標のため、要素の絶対 transform を適用して
    // キャンバス座標（tree.size() と同じ px 空間）に変換する。これにより `<use>` で
    // 配置されたグリフや、viewBox→px スケールが正しく反映される。
    let abs = path.abs_transform();
    let data = if abs.is_identity() {
        path.data().clone()
    } else {
        path.data()
            .clone()
            .transform(abs)
            .unwrap_or_else(|| path.data().clone())
    };
    let segments = convert_segments(&data);
    if segments.is_empty() {
        return None;
    }
    let fill = path.fill().and_then(convert_fill);
    let stroke = path.stroke().and_then(convert_stroke);
    if fill.is_none() && stroke.is_none() {
        return None;
    }
    Some(PathData {
        segments,
        fill,
        stroke,
    })
}

fn convert_segments(data: &usvg::tiny_skia_path::Path) -> Vec<PathSegment> {
    use usvg::tiny_skia_path::PathSegment as S;
    data.segments()
        .map(|seg| match seg {
            S::MoveTo(p) => PathSegment::MoveTo([p.x, p.y]),
            S::LineTo(p) => PathSegment::LineTo([p.x, p.y]),
            S::QuadTo(c, p) => PathSegment::QuadTo([c.x, c.y], [p.x, p.y]),
            S::CubicTo(c1, c2, p) => {
                PathSegment::CubicTo([c1.x, c1.y], [c2.x, c2.y], [p.x, p.y])
            }
            S::Close => PathSegment::Close,
        })
        .collect()
}

fn convert_fill(fill: &usvg::Fill) -> Option<Fill> {
    let color = paint_color(fill.paint(), fill.opacity().get())?;
    let rule = match fill.rule() {
        usvg::FillRule::NonZero => FillRule::NonZero,
        usvg::FillRule::EvenOdd => FillRule::EvenOdd,
    };
    Some(Fill { color, rule })
}

fn convert_stroke(stroke: &usvg::Stroke) -> Option<Stroke> {
    let color = paint_color(stroke.paint(), stroke.opacity().get())?;
    Some(Stroke {
        color,
        width: stroke.width().get(),
    })
}

/// Paint を単色に落とす。グラデーションは先頭ストップ色で近似、パターンは未対応。
fn paint_color(paint: &usvg::Paint, opacity: f32) -> Option<Color> {
    let color = match paint {
        usvg::Paint::Color(c) => *c,
        usvg::Paint::LinearGradient(g) => g.stops().first()?.color(),
        usvg::Paint::RadialGradient(g) => g.stops().first()?.color(),
        usvg::Paint::Pattern(_) => return None,
    };
    let a = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(Color::rgba(color.red, color.green, color.blue, a))
}
