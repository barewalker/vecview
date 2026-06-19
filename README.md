# vecview

ベクターグラフィクス（SVG / Typst / PDF）を**ラスタ化せずベクター品質のまま**ターミナルに表示する CLI ツール。
SVG / Typst は `lyon` でテッセレーションし `wgpu`（GPU）で表示解像度に合わせてその都度アンチエイリアス描画するため、
拡大しても劣化しない。PDF は `pdfium` で直接ラスタライズする。

主目的は **nvim で編集する Typst ドキュメントのライブプレビューをターミナル内で完結させること**。
ブラウザを開かず、保存するたびにターミナル上のプレビューが更新される。

## インストール

```bash
cargo install --path crates/vecview
# または: cargo build --release  →  target/release/vv
```

インストールされるコマンド名は **`vv`**（正式名称は vecview）。

### 実行時の依存

| 依存 | 用途 | 備考 |
|---|---|---|
| **libpdfium**（`libpdfium.so` / `.dylib` / `.dll`） | PDF 表示・Typst/PDF のテキスト層 | 必須。`pdfium-render` がリンク・ロードする |
| **typst** | `.typ` のライブプレビュー | `.typ` を渡すときのみ。PATH にあること |
| **Vulkan ドライバ**（Mesa RADV/ANV 等） | SVG/Typst の GPU 描画 | ヘッドレス wgpu 描画に必要 |

libpdfium のビルド済みバイナリは [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) から入手できる。
ライブラリ検索パス（`LD_LIBRARY_PATH` 等）が通る場所に置く。

## 使い方

```bash
vv <FILE>            # SVG / Typst (.typ) / PDF
vv doc.typ           # 内部で `typst watch` を起動し、保存ごとにライブ再描画
vv paper.pdf         # PDF を表示（ファイル変更を監視して再描画）
vv diagram.svg       # SVG を表示（任意の SVG ビューアとしても使える）

# オプション
vv doc.typ -z 150            # 初期ズーム 150%
vv doc.typ -s 2              # スーパーサンプリング倍率（既定 1）
vv doc.typ -b sixel          # バックエンド強制 [kitty|tmux|sixel|framebuffer]
```

### ヘッドレス描画（`--render`）

端末も対話もなしで1ページを PNG に描いて終了するモード。エディタ・ファイラ連携（yazi の
プレビューや nvim プラグイン）が「指定サイズの画像を1枚作る」ための土台。描画経路は通常表示と
同じ（PDF=pdfium、SVG/Typst=wgpu）。

```bash
vv --render doc.typ --size 800x1000 -o preview.png   # 1ページ目を PNG 出力
vv --render paper.pdf --size 700x900 --page 3 -o -    # 3ページ目を stdout へ
```

| フラグ | 説明 |
|---|---|
| `--render` | ヘッドレス描画モードを有効化 |
| `--size 幅x高` | 出力ピクセルサイズ（必須） |
| `--page N` | 描画するページ（1始まり、既定 1） |
| `-o, --output` | 出力先 PNG パス。`-` で stdout（既定 `-`） |
| `-z, --zoom` | ズーム%（通常表示と共通） |

### 操作キー（TTY で起動時のインタラクティブモード）

| キー | 動作 |
|---|---|
| `+` / `=` | ズームイン |
| `-` | ズームアウト |
| `0` | ズームリセット（フィット表示） |
| `w` / `v` | 本文の左右 / 上下フィット |
| 矢印 | パン（拡大時に表示位置を移動） |
| `j` / `Space` / `PageDown` | 次ページ |
| `k` / `PageUp` / `Backspace` | 前ページ |
| `h` / `l` | 先頭 / 最終ページ |
| マウスホイール | ページ送り（下＝次 / 上＝前） |
| `y` | テキスト選択（copy mode）へ入る |
| `?` | ヘルプ表示 |
| `q` / `Esc` / `Ctrl-C` | 終了 |

キーは `config.toml` の `[keys]` で再割り当てできる（`?` のヘルプにパスを表示）。

ズームはページ全体のフィット表示（100%）を基準に、**ページ内の一部を拡大**できる。拡大時は
キャッシュ画像の拡大ではなく**その解像度でビューポートを再テッセレーション→再描画**するため、
どれだけ拡大しても劣化しない（PDF は pdfium が同等にビューポートを再ラスタライズする）。矢印で拡大位置をパンする。
複数ページの文書はページ送りで閲覧できる。

#### テキスト選択・コピー（copy mode）

`y`（またはマウスドラッグ）でテキストを選択しクリップボードへコピーできる。画像出力のため
**端末標準の文字選択は効かない**ので、専用の copy mode を持つ。

| キー | 動作 |
|---|---|
| `h` `j` `k` `l` / 矢印 | キャレット移動（文字 / 行） |
| `0` / `$` | 行頭 / 行末 |
| `g` / `G` | 文頭 / 文末 |
| `Space` | 選択開始 / 解除 |
| `Enter` / `y` | コピーして抜ける |
| `Esc` / `q` | 取り消し |
| マウスドラッグ | 範囲選択し、離すとコピー |

コピーは **OSC 52** で送出するので X11/Wayland 非依存、SSH・tmux 越しでもホスト側
クリップボードに入る（tmux では `allow-passthrough on` が必要）。

対応フォーマット: **PDF** はそのままテキスト層を持つ。**Typst** は表示は SVG（ベクター品質）の
まま、copy mode 突入時に同じ `.typ` を裏で PDF にもコンパイルし、その文字・座標を選択に使う
（Typst の SVG はグリフがパス化され文字を持たないため）。単体 `.svg` はテキスト層を持たない。

### エディタ・ファイラ連携（yazi / nvim）

`vv --render`（ヘッドレス PNG 出力）を土台にした連携プラグインを同梱している。

- **[examples/nvim](examples/nvim/)** — `vv.nvim`。Neovim 内に SVG / Typst / PDF のライブ
  プレビューを出す（純 Lua・第三者依存なし、Kitty graphics）。`.typ` を `:VV` で別窓に表示し、
  保存のたびに再描画。**論文・資料作成が nvim 内で完結**する。
- **[examples/yazi](examples/yazi/)** — `vv.yazi`。yazi のプレビューペインに SVG / Typst / PDF を
  表示する previewer。とくに **Typst は yazi 単体では見られない**ため有用。

いずれもプラグインを使わず、tmux の別ペインで `vv doc.typ` を起動して横に並べる使い方でもよい
（nvim で `doc.typ` を保存するたびにプレビューが更新される。プラグイン不要）。

## 出力バックエンド

| バックエンド | 対象端末 | 備考 |
|---|---|---|
| `kitty` | Ghostty / kitty / WezTerm | Kitty Graphics Protocol（RGBA 直接転送） |
| `kitty (tmux placeholder)` | tmux 内の上記端末 | Unicode プレースホルダ＋DCS passthrough でペイン内に正しく配置 |
| `sixel` | Sixel 対応端末（Windows Terminal / xterm / foot / mlterm 等） | Kitty 非対応端末向け。256 色に減色 |
| `sixel (tmux passthrough / native)` | tmux 内の Sixel 端末 | tmux が sixel 対応なら native、非対応なら passthrough |
| `framebuffer` | Linux bare TTY / 組み込み | `/dev/fb0` へ直接描画。ネイティブ解像度でベクター品質が最大限活きる |

起動時に環境変数と TTY 状態から自動選択する。`--backend`（または `VECVIEW_BACKEND`）で強制も可能。

### tmux で使う場合

Kitty / Sixel グラフィクスを tmux 経由で通すには passthrough を有効化する：

```tmux
set -g allow-passthrough on
```

tmux の Sixel をネイティブに使う場合は、端末の sixel 対応を tmux に認識させる：

```tmux
set -as terminal-features '*:sixel'
```

### 環境変数

| 変数 | 既定 | 説明 |
|---|---|---|
| `VECVIEW_BACKEND` | 自動検出 | バックエンド強制 `[kitty\|tmux\|sixel\|framebuffer]` |
| `VECVIEW_SCALE` | `1` | スーパーサンプリング倍率（1..=4）。`-s` でも指定可。高倍率はシャープだが転送量が倍率²で増える |
| `VECVIEW_CELL_PX` | `8x16` | 端末がピクセル寸法を報告しないとき（SSH+tmux 等）の概算セルサイズ `幅x高`。表示が縮む/はみ出す場合に調整 |
| `VECVIEW_MIN_FRAME_MS` | `200`（tmux 経由）/ `80`（直接） | 連続入力中の画像転送の最小間隔（ms）。小さいほど滑らかだが端末を追い越すと不安定になる |
| `VECVIEW_REDRAW_MS` | `1000` | tmux passthrough sixel を tmux 再描画から復元するための再送間隔（ms） |
| `VECVIEW_SIXEL_NATIVE` | 無効 | `1` で tmux ネイティブ sixel を試す（要 `client_termfeatures` に sixel） |
| `VECVIEW_PROBE` | 無効 | `1` で端末が報告するサイズと描画解像度を表示して終了（解像度調査用） |

> 補足: tmux 経由の Kitty / Sixel はターミナルプロトコル上、画像の高頻度更新に弱い端末がある。
> ズーム/パンの連打で表示が乱れる・端末が不安定になる場合は `VECVIEW_SCALE=1`（既定）に加えて
> `VECVIEW_MIN_FRAME_MS` を大きめ（例 `300`）にすると安定する。

### Framebuffer で使う場合

- bare TTY（`Ctrl+Alt+F3` 等のコンソール）で実行する。GUI セッション内ではコンポジタが
  画面を占有しているため表示できない。
- `/dev/fb0` への読み書き権限が必要（`video` グループ等）。

## アーキテクチャ

```
.typ ──(typst watch)──┐
                      ├─> SVG ──usvg──> ベクター木 ──lyon──> メッシュ
.svg ─────────────────┘                                      │
                                                             ▼
                            wgpu（オフスクリーン・MSAA・表示解像度）──> RGBA
.pdf ──pdfium──(ビューポート再ラスタライズ)─────────────────────> RGBA
                                                             │
                          ┌──────────────────────┬───────────┴──────────┐
                          ▼                       ▼                      ▼
                Kitty Graphics Protocol         Sixel            /dev/fb0 直接描画
```

クレート構成（Cargo workspace）：

| クレート | 役割 |
|---|---|
| `vecview` | CLI エントリ。`typst watch` 起動、ファイル監視、ライブ再描画ループ |
| `vecview-core` | フォーマット非依存の抽象（`Document` / `OutputBackend` / `Page` / `PathData`） |
| `vecview-svg` | `usvg` で SVG をパースし `Page` に変換（曲線情報を保持） |
| `vecview-renderer` | `lyon` テッセレーション + `wgpu` ヘッドレス描画 + RGBA 読み戻し |
| `vecview-pdf` | `pdfium` で PDF を直接ラスタライズ・テキスト層抽出 |
| `vecview-output` | バックエンド検出と Kitty / Sixel / Framebuffer 実装 |

## ビルドと検証

```bash
cargo build
cargo test                 # blit 変換・アスペクト計算・GPU 描画のスモークテスト等
cargo clippy --all-targets
```

GPU 描画のヘッドレス動作には Vulkan ドライバ（Mesa RADV/ANV 等）が必要。

## 現状と今後

実装済み：SVG / Typst / PDF の表示、ファイル変更でのライブ再描画、Kitty（+tmux placeholder）/ Sixel（+tmux）/
Framebuffer 出力、ベクター品質の高解像度描画、インタラクティブなズーム / 複数ページ送り、テキスト選択・コピー、
tmux ペイン内への正しい配置・ウィンドウ切替や多重起動への対応。

未対応 / 既知の制約：グラデーション / クリップパスの忠実な描画、Framebuffer の実機表示確認、
一部端末での高頻度画像更新の安定性（上記の環境変数で緩和）。

## ライセンス

Apache License 2.0 — 詳細は [LICENSE](LICENSE) を参照。Copyright 2026 Mitsuaki Takeuchi.
