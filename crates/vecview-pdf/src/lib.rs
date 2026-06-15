//! PDF レンダリング。pdfium で PDF を「表示解像度に直接ラスタライズ」する。
//!
//! 以前は pdftocairo で PDF を SVG 化し usvg で読んでいたが、usvg はネストした `<use>`
//! （pdftocairo が凡例等で出すテンプレート参照）の配置 transform を二重適用するバグがあり、
//! 図の枠がずれる。pdfium は PDF を直接描画するため文字・図・ラスター画像・マスク・
//! グラデーションを正しく再現でき、この問題を構造的に回避する。
//!
//! 出力解像度は表示のたびに与えられる（ズーム/パン後のビューポートを out_w×out_h に投影）。
//! ベクター品質は「毎回ネイティブ解像度でラスタライズし直す」ことで担保する。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use pdfium_render::prelude::*;

/// `Pdfium` を `static`（要 Send+Sync）へ入れるためのラッパ。
/// SAFETY: 本アプリは pdfium をメインスレッドからのみ使う（描画はメインループ、監視/キー入力
/// スレッドは pdfium に触れない）。スレッド間共有しないため Send/Sync を表明してよい。
struct SyncPdfium(Pdfium);
unsafe impl Sync for SyncPdfium {}
unsafe impl Send for SyncPdfium {}

/// プロセス全体で1つだけ持つ pdfium バインディング。
static PDFIUM: OnceLock<SyncPdfium> = OnceLock::new();

/// pdfium をプロセスに一度だけバインドして返す。
fn pdfium() -> Result<&'static Pdfium> {
    if let Some(p) = PDFIUM.get() {
        return Ok(&p.0);
    }
    let bindings = bind().context("pdfium ライブラリのバインドに失敗")?;
    // 競合しても冪等（最初の set が勝つ）。本アプリは起動時に単一スレッドで初期化する。
    let _ = PDFIUM.set(SyncPdfium(Pdfium::new(bindings)));
    Ok(&PDFIUM.get().unwrap().0)
}

/// libpdfium を探してバインドする。優先順位: 環境変数 VECVIEW_PDFIUM_LIB > 既定パス > システム。
fn bind() -> Result<Box<dyn PdfiumLibraryBindings>> {
    if let Some(p) = std::env::var_os("VECVIEW_PDFIUM_LIB") {
        return Pdfium::bind_to_library(PathBuf::from(&p))
            .map_err(|e| anyhow!("VECVIEW_PDFIUM_LIB のバインド失敗 ({:?}): {e}", p));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join(".local/lib/libpdfium.so"));
    }
    candidates.push(PathBuf::from("/usr/lib/libpdfium.so"));
    candidates.push(PathBuf::from("/usr/local/lib/libpdfium.so"));
    for c in &candidates {
        if c.is_file() {
            if let Ok(b) = Pdfium::bind_to_library(c) {
                return Ok(b);
            }
        }
    }
    Pdfium::bind_to_system_library().map_err(|e| {
        anyhow!(
            "libpdfium が見つかりません。VECVIEW_PDFIUM_LIB か ~/.local/lib/libpdfium.so に配置してください: {e}"
        )
    })
}

/// pdfium が利用可能か（ライブラリにバインドできるか）。
pub fn is_available() -> bool {
    pdfium().is_ok()
}

/// テキスト層の1文字。ページ座標（左上原点・Y下向き, pt）の矩形 `rect`=[x, y, w, h] と
/// その文字 `ch` を持つ。`page_text` が読み順（pdfium の文字インデックス順）で返す。
#[derive(Clone, Debug)]
pub struct Glyph {
    pub ch: char,
    pub rect: [f32; 4],
}

/// 開いた PDF。ページのラスタライズと本文境界の取得を提供する。
pub struct Pdf {
    doc: PdfDocument<'static>,
}

impl Pdf {
    /// PDF ファイルを開く。
    pub fn open(path: &Path) -> Result<Self> {
        let doc = pdfium()?
            .load_pdf_from_file(path, None)
            .with_context(|| format!("PDF を開けません: {}", path.display()))?;
        Ok(Self { doc })
    }

    /// ページ数。
    pub fn page_count(&self) -> usize {
        self.doc.pages().len() as usize
    }

    /// ページ寸法（ポイント, 幅×高さ）。
    pub fn page_size(&self, index: usize) -> Result<(f32, f32)> {
        let page = self.page(index)?;
        Ok((page.width().value, page.height().value))
    }

    /// ページ `index` のビューポート矩形 `[x, y, w, h]`（ページ座標, 左上原点・Y下向き, 単位pt）を
    /// `out_w × out_h` ピクセルへラスタライズし RGBA8 を返す。アスペクト比は呼び出し側で out と
    /// 一致させること（viewport の w:h ＝ out_w:out_h）。
    pub fn render(&self, index: usize, viewport: [f32; 4], out_w: u32, out_h: u32) -> Result<Vec<u8>> {
        let page = self.page(index)?;
        let [vx, vy, vw, _vh] = viewport;
        let s = out_w as f32 / vw.max(1.0); // ピクセル/ポイント。

        // pdfium の行列は内部で Y 反転を処理するため、ページ座標（左上原点・Y下向き, pt）から
        // ビットマップ（左上原点・Y下向き, px）への素直な拡大＋平行移動でよい（Y反転は不要）。
        //   Dx = s*(Px - vx),  Dy = s*(Py - vy)
        // FS_MATRIX {a,b,c,d,e,f}: Dx = a*Px + c*Py + e, Dy = b*Px + d*Py + f。
        let (a, b, c, d, e, f) = (s, 0.0, 0.0, s, -s * vx, -s * vy);

        let mut bitmap = PdfBitmap::empty(
            out_w as i32,
            out_h as i32,
            PdfBitmapFormat::default(),
            pdfium()?.bindings(),
        )
        .map_err(|e| anyhow!("ビットマップ生成失敗: {e}"))?;

        let config = PdfRenderConfig::new()
            // 出力サイズをビットマップに一致させる（クリア矩形が全面を覆うように）。
            .set_fixed_size(out_w as i32, out_h as i32)
            // ページ範囲外（letterbox）や未描画部は不透明な白で塗る。
            .set_clear_color(PdfColor::new(255, 255, 255, 255))
            .transform(a, b, c, d, e, f)
            .map_err(|e| anyhow!("行列設定失敗: {e}"))?
            .clip(0, 0, out_w as i32, out_h as i32);

        page.render_into_bitmap_with_config(&mut bitmap, &config)
            .map_err(|e| anyhow!("PDF 描画失敗: {e}"))?;

        Ok(bitmap.as_rgba_bytes())
    }

    /// 本文（全描画オブジェクト）の外接矩形を、ページ座標（左上原点・Y下向き, pt）で返す。
    /// 本文フィット（左右/上下）に使う。取得できなければ None。
    pub fn content_bbox(&self, index: usize) -> Option<[f32; 4]> {
        let page = self.page(index).ok()?;
        let ph = page.height().value;
        let group = page.objects().create_group(|_| true).ok()?;
        let b = group.bounds().ok()?;
        let (left, right) = (b.left().value, b.right().value);
        let (top, bottom) = (b.top().value, b.bottom().value); // PDF は top > bottom（Y上向き）。
        let (w, h) = (right - left, top - bottom);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        // 左上原点へ変換（y_top = ph - top）。
        Some([left, ph - top, w, h])
    }

    /// ページ `index` のテキスト層を読み順で返す。各文字はページ座標（左上原点・Y下向き, pt）の
    /// 矩形を持つ。pdfium の `loose_bounds`（グリフ全体を含む緩い矩形）を `ph - top` で左上原点へ
    /// 変換する。グリフ矩形が取れない文字（制御文字等）はスキップする。Typst のテキスト選択では、
    /// 表示用 SVG と pt 寸法が一致する併用 PDF をこの API で読む。
    pub fn page_text(&self, index: usize) -> Result<Vec<Glyph>> {
        let page = self.page(index)?;
        let ph = page.height().value;
        let text = page
            .text()
            .map_err(|e| anyhow!("テキスト取得失敗: {e}"))?;
        let chars = text.chars();
        let mut out = Vec::with_capacity(chars.len());
        for c in chars.iter() {
            let Some(ch) = c.unicode_char() else { continue };
            // 改行・タブ等は矩形を持たず連結時に効くので、矩形ゼロで保持する。
            let rect = match c.loose_bounds() {
                Ok(b) => {
                    let (left, right) = (b.left().value, b.right().value);
                    let (top, bottom) = (b.top().value, b.bottom().value); // PDF は top > bottom。
                    [left, ph - top, (right - left).max(0.0), (top - bottom).max(0.0)]
                }
                Err(_) => [0.0, 0.0, 0.0, 0.0],
            };
            out.push(Glyph { ch, rect });
        }
        Ok(out)
    }

    fn page(&self, index: usize) -> Result<PdfPage<'_>> {
        let count = self.page_count();
        if index >= count {
            bail!("ページ範囲外: {index}（総 {count} ページ）");
        }
        self.doc
            .pages()
            .get(index as u16)
            .map_err(|e| anyhow!("ページ取得失敗: {e}"))
    }
}
