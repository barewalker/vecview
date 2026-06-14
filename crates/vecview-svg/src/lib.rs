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
        // usvg は `<use transform="T">` の T を二重適用するバグがある（matplotlib のテキストや
        // pdftocairo の凡例で多発）。意味的に等価な `<g transform="T"><use/></g>` へ書き換えると
        // 正しく解決される。SVG はテキストなので、パース前に書き換える（ネスト SVG 画像内も）。
        let tree = match std::str::from_utf8(data) {
            Ok(text) => usvg::Tree::from_data(rewrite_use_transforms(text).as_bytes(), &opt)?,
            Err(_) => usvg::Tree::from_data(data, &opt)?, // 非UTF-8（gzip 等）はそのまま。
        };
        Ok(Self { tree })
    }
}

/// `<use … transform="T" …/>` を `<g transform="T"><use …/></g>` に書き換える（usvg の
/// `<use>` transform 二重適用バグの回避）。ネスト SVG 画像（data:image/svg+xml;base64,…）の
/// 中身も再帰的に処理する。意味的に等価な変換なので副作用はない。
fn rewrite_use_transforms(svg: &str) -> std::borrow::Cow<'_, str> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use std::sync::OnceLock;

    static USE_RE: OnceLock<regex::Regex> = OnceLock::new();
    static NESTED_RE: OnceLock<regex::Regex> = OnceLock::new();
    let use_re = USE_RE.get_or_init(|| {
        regex::Regex::new(r#"(?s)<use\b([^>]*?)\s+transform="([^"]*)"([^>]*?)\s*/>"#).unwrap()
    });
    let nested_re = NESTED_RE
        .get_or_init(|| regex::Regex::new(r"data:image/svg\+xml;base64,([A-Za-z0-9+/=]+)").unwrap());

    // 1. ネスト SVG 画像の base64 を復号 → 再帰書き換え → 再エンコード。
    let with_nested = nested_re.replace_all(svg, |c: &regex::Captures| {
        let decoded = STANDARD
            .decode(&c[1])
            .ok()
            .and_then(|b| String::from_utf8(b).ok());
        match decoded {
            Some(inner) => format!(
                "data:image/svg+xml;base64,{}",
                STANDARD.encode(rewrite_use_transforms(&inner).as_bytes())
            ),
            None => c[0].to_string(),
        }
    });

    // 2. このレベルの <use transform> を g ラップへ。
    let rewritten =
        use_re.replace_all(&with_nested, |c: &regex::Captures| {
            format!("<g transform=\"{}\"><use{}{}/></g>", &c[2], &c[1], &c[3])
        });

    // どちらの段でも変更が無ければ借用のまま返す。
    if matches!(with_nested, std::borrow::Cow::Borrowed(_)) && matches!(rewritten, std::borrow::Cow::Borrowed(_))
    {
        std::borrow::Cow::Borrowed(svg)
    } else {
        std::borrow::Cow::Owned(rewritten.into_owned())
    }
}

impl Document for SvgDocument {
    fn page_count(&self) -> usize {
        1
    }

    fn render_page(&self, _index: usize) -> Result<Page> {
        let size = self.tree.size();
        let mut commands = Vec::new();
        walk(self.tree.root(), usvg::Transform::identity(), &mut commands);
        Ok(Page {
            width: size.width(),
            height: size.height(),
            commands,
        })
    }
}

/// グループを再帰的に走査し、可視パス/画像を `commands` に収集する。`prefix` は親から積んだ
/// 追加 transform（ネスト SVG 画像の配置に使う）。トップレベルでは恒等。
fn walk(group: &usvg::Group, prefix: usvg::Transform, out: &mut Vec<DrawCommand>) {
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => walk(g, prefix, out),
            usvg::Node::Path(p) => push_path(p, prefix, out),
            usvg::Node::Image(img) => convert_image(img, prefix, out),
            // テキストは初回スコープ外（Typst/PDF の SVG では文字は Path 化されて来る）。
            usvg::Node::Text(_) => {}
        }
    }
}

/// `<image>` ノードを取り込む。ラスター（PNG/JPEG/GIF/WebP）は [`ImageData`] として、
/// ネスト SVG（typst が `image("…​.svg")` で出す data:image/svg+xml）はそのツリーを辿って
/// ベクターのまま取り込む（ベクター品質を保つ）。親グループのソフトマスク/クリップは未対応。
fn convert_image(img: &usvg::Image, prefix: usvg::Transform, out: &mut Vec<DrawCommand>) {
    if !img.is_visible() {
        return;
    }
    // 画像ローカル → 親キャンバスへの変換（親の prefix × 画像の絶対 transform）。
    let img_ts = prefix.pre_concat(img.abs_transform());
    match img.kind() {
        usvg::ImageKind::SVG(tree) => {
            // ネスト SVG を画像ボックス(size)へ非一様スケールで合わせ（typst は
            // preserveAspectRatio="none"）、その上に配置 transform を掛けて辿る。
            let (bw, bh) = (img.size().width(), img.size().height());
            let (tw, th) = (tree.size().width(), tree.size().height());
            if tw <= 0.0 || th <= 0.0 {
                return;
            }
            let fit = usvg::Transform::from_scale(bw / tw, bh / th);
            walk(tree.root(), img_ts.pre_concat(fit), out);
        }
        kind => {
            if let Some(image) = decode_raster(kind, img, prefix) {
                out.push(DrawCommand::Image(image));
            }
        }
    }
}

/// ラスター画像をデコードして [`ImageData`] にする。配置矩形は絶対外接矩形（`abs_bounding_box`、
/// 軸平行）に `prefix` を適用したもの。トップレベルでは prefix=恒等でそのまま。
fn decode_raster(kind: &usvg::ImageKind, img: &usvg::Image, prefix: usvg::Transform) -> Option<ImageData> {
    let bytes: &[u8] = match kind {
        usvg::ImageKind::JPEG(d)
        | usvg::ImageKind::PNG(d)
        | usvg::ImageKind::GIF(d)
        | usvg::ImageKind::WEBP(d) => d,
        usvg::ImageKind::SVG(_) => return None,
    };
    let decoded = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (px_width, px_height) = decoded.dimensions();
    let bbox = img.abs_bounding_box();
    let (x0, y0) = map_pt(&prefix, bbox.x(), bbox.y());
    let (x1, y1) = map_pt(&prefix, bbox.x() + bbox.width(), bbox.y() + bbox.height());
    Some(ImageData {
        rgba: decoded.into_raw(),
        px_width,
        px_height,
        rect: [x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()],
    })
}

/// 点 (x, y) に transform を適用する（tiny-skia の行優先: x'=sx·x+kx·y+tx, y'=ky·x+sy·y+ty）。
fn map_pt(ts: &usvg::Transform, x: f32, y: f32) -> (f32, f32) {
    (ts.sx * x + ts.kx * y + ts.tx, ts.ky * x + ts.sy * y + ts.ty)
}

fn push_path(path: &usvg::Path, prefix: usvg::Transform, out: &mut Vec<DrawCommand>) {
    if !path.is_visible() {
        return;
    }
    // usvg の `data()` はローカル座標。要素の絶対 transform に親の prefix を積んで
    // キャンバス座標へ変換する（`<use>` 配置のグリフ、viewBox→px、ネスト SVG が反映される）。
    let ts = prefix.pre_concat(path.abs_transform());
    let data = if ts.is_identity() {
        path.data().clone()
    } else {
        path.data()
            .clone()
            .transform(ts)
            .unwrap_or_else(|| path.data().clone())
    };
    let segments = convert_segments(&data);
    if segments.is_empty() {
        return;
    }
    // ストローク幅・破線間隔もジオメトリと同じ縮尺で変換する（ネスト SVG を縮小配置した図の
    // 軸線が太くならないように）。非一様スケールでも近似となるよう面積比の平方根を使う。
    let scale = (ts.sx * ts.sy - ts.kx * ts.ky).abs().sqrt();
    let fill = path.fill().and_then(convert_fill);
    let stroke = path.stroke().and_then(|s| convert_stroke(s, scale));
    if fill.is_none() && stroke.is_none() {
        return;
    }

    // 破線指定があれば、塗りは元パスで、ストロークは破線オン区間に分割したパスで描く。
    let dash = path
        .stroke()
        .and_then(|s| s.dasharray().map(|d| (d.to_vec(), s.dashoffset())));
    match (stroke, dash) {
        (Some(stroke), Some((pattern, offset))) => {
            if let Some(fill) = fill {
                out.push(DrawCommand::Path(PathData {
                    segments: segments.clone(),
                    fill: Some(fill),
                    stroke: None,
                }));
            }
            let dashed = dash_segments(&segments, &pattern, offset, scale);
            if !dashed.is_empty() {
                out.push(DrawCommand::Path(PathData {
                    segments: dashed,
                    fill: None,
                    stroke: Some(stroke),
                }));
            }
        }
        (stroke, _) => out.push(DrawCommand::Path(PathData {
            segments,
            fill,
            stroke,
        })),
    }
}

/// 破線パターンに従ってパスを「オン区間」の線分群に分割する。`pattern`/`offset` はローカル単位
/// なので `scale` でキャンバス単位へ。曲線は折れ線に平坦化してから等長で刻む。
fn dash_segments(segments: &[PathSegment], pattern: &[f32], offset: f32, scale: f32) -> Vec<PathSegment> {
    let mut pat: Vec<f32> = pattern.iter().map(|v| (v * scale).max(0.0)).collect();
    if pat.len() % 2 == 1 {
        let dup = pat.clone();
        pat.extend(dup); // SVG: 奇数長は2周ぶんで偶数化。
    }
    let period: f32 = pat.iter().sum();
    if period <= 1e-6 {
        return segments.to_vec(); // 全0は実線扱い。
    }

    let mut out = Vec::new();
    for poly in flatten(segments) {
        dash_polyline(&poly, &pat, period, offset * scale, &mut out);
    }
    out
}

/// セグメント列を部分パスごとの折れ線（点列）に平坦化する。
fn flatten(segments: &[PathSegment]) -> Vec<Vec<[f32; 2]>> {
    let mut polys = Vec::new();
    let mut cur: Vec<[f32; 2]> = Vec::new();
    let mut start = [0.0, 0.0];
    let mut p = [0.0, 0.0];
    let flush = |cur: &mut Vec<[f32; 2]>, polys: &mut Vec<Vec<[f32; 2]>>| {
        if cur.len() > 1 {
            polys.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for seg in segments {
        match *seg {
            PathSegment::MoveTo(a) => {
                flush(&mut cur, &mut polys);
                cur.push(a);
                start = a;
                p = a;
            }
            PathSegment::LineTo(a) => {
                cur.push(a);
                p = a;
            }
            PathSegment::QuadTo(c, a) => {
                flatten_bezier(p, [c, c], a, true, &mut cur);
                p = a;
            }
            PathSegment::CubicTo(c1, c2, a) => {
                flatten_bezier(p, [c1, c2], a, false, &mut cur);
                p = a;
            }
            PathSegment::Close => {
                if !cur.is_empty() {
                    cur.push(start);
                }
                p = start;
            }
        }
    }
    flush(&mut cur, &mut polys);
    polys
}

/// 2次/3次ベジェを固定分割で折れ線化して `out` に追加する（始点は既出なので終点側のみ）。
fn flatten_bezier(p0: [f32; 2], ctrl: [[f32; 2]; 2], p3: [f32; 2], quad: bool, out: &mut Vec<[f32; 2]>) {
    const N: usize = 16;
    for i in 1..=N {
        let t = i as f32 / N as f32;
        let pt = if quad {
            let u = 1.0 - t;
            [
                u * u * p0[0] + 2.0 * u * t * ctrl[0][0] + t * t * p3[0],
                u * u * p0[1] + 2.0 * u * t * ctrl[0][1] + t * t * p3[1],
            ]
        } else {
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            [
                a * p0[0] + b * ctrl[0][0] + c * ctrl[1][0] + d * p3[0],
                a * p0[1] + b * ctrl[0][1] + c * ctrl[1][1] + d * p3[1],
            ]
        };
        out.push(pt);
    }
}

/// 折れ線1本を破線パターンで刻み、オン区間を MoveTo/LineTo として `out` に追加する。
fn dash_polyline(poly: &[[f32; 2]], pat: &[f32], period: f32, offset: f32, out: &mut Vec<PathSegment>) {
    // 開始位置をパターン内へ進める。
    let mut t = offset.rem_euclid(period);
    let mut idx = 0;
    let mut guard = 0;
    while pat[idx] <= t && guard < pat.len() * 2 {
        t -= pat[idx];
        idx = (idx + 1) % pat.len();
        guard += 1;
    }
    let mut remain = (pat[idx] - t).max(0.0);
    let mut on = idx % 2 == 0;
    let mut pen_down = false;

    for w in poly.windows(2) {
        let (a, b) = (w[0], w[1]);
        let seg_len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        if seg_len <= 1e-9 {
            continue;
        }
        let dir = [(b[0] - a[0]) / seg_len, (b[1] - a[1]) / seg_len];
        let mut pos = 0.0;
        let mut iter = 0;
        while pos < seg_len - 1e-6 && iter < 100_000 {
            iter += 1;
            let step = remain.min(seg_len - pos);
            if on {
                let p0 = [a[0] + dir[0] * pos, a[1] + dir[1] * pos];
                let p1 = [a[0] + dir[0] * (pos + step), a[1] + dir[1] * (pos + step)];
                if !pen_down {
                    out.push(PathSegment::MoveTo(p0));
                    pen_down = true;
                }
                out.push(PathSegment::LineTo(p1));
            }
            pos += step;
            remain -= step;
            if remain <= 1e-6 {
                idx = (idx + 1) % pat.len();
                remain = pat[idx];
                on = !on;
                pen_down = false;
            }
        }
    }
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

fn convert_stroke(stroke: &usvg::Stroke, scale: f32) -> Option<Stroke> {
    let color = paint_color(stroke.paint(), stroke.opacity().get())?;
    Some(Stroke {
        color,
        width: stroke.width().get() * scale,
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
