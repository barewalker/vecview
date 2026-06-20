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
    { url = "*.svg", run = "vv" },              # SVG（任意）
    { mime = "application/pdf", run = "vv" },   # PDF（任意。yazi 既定の方が速い場合あり）
]
# SVG は画像 mime のため、yazi 標準の画像プリローダが外部 `resvg` で svg→PNG 変換しようとする
# （resvg 未導入だと "Failed to start resvg" エラー）。プリローダを noop にして vv 経路へ一本化する。
prepend_preloaders = [
    { url = "*.svg", run = "noop" },
]
```

`.typ` だけで十分なら svg/pdf の行は外してよい。

> SVG を vv で扱わず **yazi 標準の svg プレビュー**で済ませたい場合は、上の svg 行と preloaders を
> 消して、代わりに `resvg` を入れる（`cargo install resvg`）。resvg は usvg ベースで vv と同じ描画
> エンジンなので画質は同等。

## tmux + Ghostty のクラッシュについて（重要）

**Ghostty を tmux 越しで使うと、画像プレビューで Ghostty 自体が落ちることがある。これは
vecview ではなく Ghostty 側の既知バグ**で、yazi 標準の画像（png/jpg）でも、ネイティブの kitty 画像
全般でも起きる（tmux 非経由のネイティブでは起きない）。報告された発生条件:

- tmux の **`mouse on`** で発生（**`mouse off` だと起きない**）
- ウィンドウが **~90x40 セルより大きい**（最大化）と発生
- 画像が **~100KB 超**で発生

参考: ghostty-org/ghostty discussions [#11909](https://github.com/ghostty-org/ghostty/discussions/11909) /
[#4266](https://github.com/ghostty-org/ghostty/discussions/4266) /
[#9197](https://github.com/ghostty-org/ghostty/discussions/9197)

回避策:

- **tmux のマウスを切る**（`set -g mouse off`、または `prefix+m` でトグル運用）— 最も確実
- ウィンドウを最大化しない（小さめなら `mouse on` でも出にくい）
- Ghostty を更新する（活発に修正されている領域）
- 重く使うときは tmux の外（ネイティブ端末）で yazi を使う

なお `[preview] image_delay`（0〜100ms）や `max_width`/`max_height` を控えめにすると緩和には
なるが、上記バグの根治にはならない。

## メモ

- **初回の表示は重い**（`typst compile` ＋ wgpu 初期化）。yazi がページ単位でキャッシュするので、
  同じファイルの2回目以降は即時。
- 画質は端末プロトコルに依存する。Ghostty / kitty なら `~/.config/yazi/yazi.toml` の
  `[preview] preview_protocol = "kitty"` が最高画質（sixel は 256 色に減色）。
- 複数ページ文書はプレビュー上でスクロールするとページが送られる。
