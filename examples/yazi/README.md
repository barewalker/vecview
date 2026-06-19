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
    { url = "*.typ", run = "vv" },              # Typst（yazi 非対応 → これが本命）
    { mime = "image/svg+xml", run = "vv" },     # SVG（mime 指定。url=*.svg だと既定の画像処理に取られる）
    { mime = "application/pdf", run = "vv" },   # PDF（任意。yazi 既定の方が速い場合あり）
]
```

`.typ` だけで十分なら svg/pdf の行は外してよい。SVG / PDF は mime で指定する（`url=` グロブだと
yazi 既定の画像/PDF 処理に先取りされ、こちらの previewer に来ないことがある）。

## tmux での注意（重要）

tmux 越しの kitty 画像転送は重く、**ウィンドウ最大化 ＋ 高頻度のファイル切り替え**で端末
（Ghostty 等）が落ちることがある（tmux 非経由のネイティブでは起きない）。緩和策:

- `[preview] image_delay`（ミリ秒）を上げて切り替え中の転送頻度を抑える（例 `120`）
- 巨大なプレビューを避ける（`[preview] max_width` / `max_height` を控えめに）
- 重く使うときは tmux の外（ネイティブ端末）で yazi を使う

## メモ

- **初回の表示は重い**（`typst compile` ＋ wgpu 初期化）。yazi がページ単位でキャッシュするので、
  同じファイルの2回目以降は即時。
- 画質は端末プロトコルに依存する。Ghostty / kitty なら `~/.config/yazi/yazi.toml` の
  `[preview] preview_protocol = "kitty"` が最高画質（sixel は 256 色に減色）。
- 複数ページ文書はプレビュー上でスクロールするとページが送られる。
