# vv.yazi — yazi previewer

[yazi](https://yazi-rs.github.io/) のプレビューペインに **SVG / Typst / PDF** を
`vv --render` で描画して表示する previewer プラグイン。とくに **Typst（`.typ`）は yazi が
ネイティブにプレビューできない**ため、ここが主な価値。

描画は vv 本体と同じ経路（PDF=pdfium、SVG/Typst=wgpu）なので、本表示とプレビューで見た目が一致する。

## 必要なもの

- PATH に `vv`（本リポジトリの vecview。`cargo install --path crates/vecview`）
- yazi **26 以降**（`ya.mgr_emit` を使用）
- Typst プレビューには `typst`、PDF には `libpdfium`（vv の実行時依存と同じ）

## インストール

プラグイン本体を yazi のプラグインディレクトリへ置く:

```bash
mkdir -p ~/.config/yazi/plugins
cp -r examples/yazi/vv.yazi ~/.config/yazi/plugins/
```

`~/.config/yazi/yazi.toml` の `[plugin]` に previewer ルールを追加する（既定より優先させるため
`prepend_previewers`）:

```toml
[plugin]
prepend_previewers = [
    { url = "*.typ", run = "vv" },   # Typst（yazi 非対応 → これが本命）
    { url = "*.svg", run = "vv" },   # SVG（ベクター品質。任意。yazi 既定でも一応見られる）
    { mime = "application/pdf", run = "vv" },  # PDF（任意。yazi 既定の方が速い場合あり）
]
```

`.typ` だけで十分なら svg/pdf の行は外してよい。

## メモ

- **初回の表示は重い**（`typst compile` ＋ wgpu 初期化）。yazi がページ単位でキャッシュするので、
  同じファイルの2回目以降は即時。
- 画質は端末プロトコルに依存する。Ghostty / kitty なら `~/.config/yazi/yazi.toml` の
  `[preview] preview_protocol = "kitty"` が最高画質（sixel は 256 色に減色）。
- 複数ページ文書はプレビュー上でスクロールするとページが送られる。
