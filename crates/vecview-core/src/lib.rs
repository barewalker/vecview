//! vecview のフォーマット非依存な抽象表現。
//!
//! パーサー（SVG/PDF）はドキュメントを [`Page`] に変換し、レンダラーは [`Page`] を
//! テッセレーションして RGBA に描画、出力バックエンドはその RGBA を端末/Framebuffer へ送る。
//! ベクター品質を保つため、パスは直線近似ではなく曲線セグメントのまま保持する。

use anyhow::Result;

/// ベクタードキュメント1ページの抽象表現。座標系は左上原点・Y下向き（SVG準拠）。
pub struct Page {
    /// ページ幅（ユーザー単位）。
    pub width: f32,
    /// ページ高さ（ユーザー単位）。
    pub height: f32,
    /// 描画順に並んだコマンド列。
    pub commands: Vec<DrawCommand>,
}

/// 描画コマンド。ベクターパスと、埋め込みラスター画像（PDF の図など）。
pub enum DrawCommand {
    Path(PathData),
    Image(ImageData),
}

/// 埋め込みラスター画像1枚。ページ座標の矩形 `rect` に RGBA8 画素を貼り付ける。
/// PDF 内のビットマップ図（pdftocairo が SVG の `<image>` として出力するもの）に対応する。
pub struct ImageData {
    /// RGBA8 ストレートアルファ。長さは `px_width * px_height * 4`。
    pub rgba: Vec<u8>,
    /// 画素幅。
    pub px_width: u32,
    /// 画素高さ。
    pub px_height: u32,
    /// ページ座標での配置矩形 [x, y, w, h]（左上原点）。画像はこの矩形へ拡縮して貼られる。
    pub rect: [f32; 4],
}

/// パスのセグメント。座標は絶対座標（親の transform 適用済み）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathSegment {
    MoveTo([f32; 2]),
    LineTo([f32; 2]),
    /// 制御点1つの2次ベジェ。
    QuadTo([f32; 2], [f32; 2]),
    /// 制御点2つの3次ベジェ。
    CubicTo([f32; 2], [f32; 2], [f32; 2]),
    Close,
}

/// 塗りつぶし規則。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

/// ベクターパス1つ分。塗りと/またはストロークを持つ。
pub struct PathData {
    pub segments: Vec<PathSegment>,
    pub fill: Option<Fill>,
    pub stroke: Option<Stroke>,
}

/// 塗りつぶし指定（初回スコープは単色のみ）。
pub struct Fill {
    pub color: Color,
    pub rule: FillRule,
}

/// ストローク指定（初回スコープは単色・幅のみ）。
pub struct Stroke {
    pub color: Color,
    pub width: f32,
}

/// RGBA カラー（各成分 0..=255、ストレート/非プリマルチプライ）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// 正規化済み `[r, g, b, a]`（0.0..=1.0）。シェーダ頂点色に使う。
    pub fn to_f32(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }
}

/// フォーマットパーサーが実装するトレイト。
pub trait Document {
    fn page_count(&self) -> usize;
    fn render_page(&self, index: usize) -> Result<Page>;
}

/// 出力バックエンドが実装するトレイト。RGBA8（width×height×4）を受け取り表示する。
pub trait OutputBackend {
    fn name(&self) -> &str;
    fn is_supported(&self) -> bool;
    fn display(&self, rgba: &[u8], width: u32, height: u32) -> Result<()>;

    /// 表示開始時の端末準備（代替スクリーンへの切替など）。デフォルトは何もしない。
    fn enter(&self) -> Result<()> {
        Ok(())
    }

    /// 終了時の後始末（描画した画像の削除・端末状態の復帰）。デフォルトは何もしない。
    fn leave(&self) -> Result<()> {
        Ok(())
    }

    /// 画面をテキスト表示用に空にする（描画済み画像の削除＋画面クリア＋カーソル原点）。
    /// ヘルプ等のテキストオーバーレイを出す前に呼ぶ。デフォルトは端末の画面クリアのみ。
    fn clear(&self) -> Result<()> {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        out.write_all(b"\x1b[2J\x1b[H")?;
        out.flush()?;
        Ok(())
    }
}
