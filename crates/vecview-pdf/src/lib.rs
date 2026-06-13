//! PDF→SVG 変換。poppler の `pdftocairo` を用いて PDF を「1ページ1 SVG」に変換する。
//!
//! ベクター品質を保つためラスタ化せず SVG を経由し、生成した SVG を既存の SVG 描画
//! パイプライン（usvg → レンダラー → 出力バックエンド）にそのまま載せる。命名は Typst
//! プレビューと揃え（`vecview-<stem>-<p>.svg`、1始まり）、ページ送り・監視を共通化する。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// 想定ページ数の上限。範囲外ページの判定が万一すり抜けても無限ループにしない安全弁。
const MAX_PAGES: usize = 10_000;

/// 変換に使う外部コマンド名（poppler 同梱）。
const PDFTOCAIRO: &str = "pdftocairo";

/// `pdftocairo` が PATH 上で利用可能か。
pub fn is_available() -> bool {
    which(PDFTOCAIRO).is_some()
}

/// `pdf` の全ページを `out_dir/vecview-<stem>-<p>.svg`（p は1始まり）へ変換し、ページ数を返す。
///
/// 同名の既存ページ SVG は事前に削除する（再変換でページ数が減っても残骸を残さない）。
/// ページ数は `pdftocairo` を1ページずつ呼び、範囲外で失敗するまでの回数で求める
/// （`pdfinfo` 等への追加依存を避けるため）。
pub fn convert_to_svgs(pdf: &Path, out_dir: &Path, stem: &str) -> Result<usize> {
    if !is_available() {
        bail!("{PDFTOCAIRO} が PATH にありません。PDF プレビューには poppler ({PDFTOCAIRO}) が必要です。");
    }
    cleanup(out_dir, stem);

    let mut pages = 0usize;
    while pages < MAX_PAGES {
        let n = pages + 1;
        let out = out_dir.join(format!("vecview-{stem}-{n}.svg"));
        let status = Command::new(PDFTOCAIRO)
            .arg("-svg")
            .args(["-f", &n.to_string(), "-l", &n.to_string()])
            .arg(pdf)
            .arg(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("{PDFTOCAIRO} の起動に失敗"))?;

        // 範囲外ページは「失敗」または「空ファイル生成」で表れる。そこで打ち切り、
        // 残った空ファイルは掃除する。
        let produced = status.success()
            && std::fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false);
        if !produced {
            let _ = std::fs::remove_file(&out);
            break;
        }
        pages = n;
    }

    if pages == 0 {
        bail!(
            "PDF を変換できませんでした（ページ0、破損または非対応）: {}",
            pdf.display()
        );
    }
    Ok(pages)
}

/// `out_dir` 内の `vecview-<stem>-*.svg` を削除する。
fn cleanup(out_dir: &Path, stem: &str) {
    let prefix = format!("vecview-{stem}-");
    let Ok(entries) = std::fs::read_dir(out_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with(&prefix) && name.ends_with(".svg") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// PATH から実行ファイル `name` を探す。
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}
