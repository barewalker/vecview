# vecview

ベクターグラフィクス（SVG / Typst）を**ラスタ化せずベクター品質のまま**ターミナルに表示する CLI ツール。
`lyon` でテッセレーションし、`wgpu`（GPU）で表示解像度に合わせてその都度アンチエイリアス描画するため、
拡大しても劣化しない。

主目的は **nvim で編集する Typst ドキュメントのライブプレビューをターミナル内で完結させること**。
ブラウザを開かず、保存するたびにターミナル上のプレビューが更新される。

## 使い方

```bash
vecview <FILE>            # SVG または Typst (.typ)
vecview doc.typ           # 内部で `typst watch` を起動し、保存ごとにライブ再描画
vecview diagram.svg       # SVG を表示（ファイル変更を監視して再描画）

# オプション
vecview doc.typ -z 150            # 初期ズーム 150%
vecview doc.typ -b kitty          # バックエンド強制 [kitty|tmux|framebuffer]
```

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
| `y` | テキスト選択（copy mode）へ入る |
| `?` | ヘルプ表示 |
| `q` / `Esc` / `Ctrl-C` | 終了 |

キーは `config.toml` の `[keys]` で再割り当てできる（`?` のヘルプにパスを表示）。

ズームはページ全体のフィット表示（100%）を基準に、**ページ内の一部を拡大**できる。拡大時は
キャッシュ画像の拡大ではなく**その解像度でビューポートを再テッセレーション→再描画**するため、
どれだけ拡大しても劣化しない。矢印で拡大位置をパンする。
複数ページの Typst 文書はページ送りで閲覧できる。

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

- `.typ` を渡すと `typst watch <file> <tmp>-{p}.svg` を起動し、1ページ目の SVG を監視・表示する。
  `typst` が PATH にあること。
- `.svg` を渡すとそのファイルを直接監視する（任意の SVG ビューアとしても使える）。
- 終了は `Ctrl-C`。

### nvim との組み合わせ

tmux の別ペインで `vecview doc.typ` を起動しておけば、nvim で `doc.typ` を編集・保存するたびに
プレビューが更新される。プラグインは不要。

## 出力バックエンド

| バックエンド | 対象 | 備考 |
|---|---|---|
| `kitty` | Ghostty / kitty / WezTerm | Kitty Graphics Protocol（RGBA 直接転送） |
| `kitty (tmux passthrough)` | tmux 内の上記端末 | DCS passthrough でラップ。tmux 設定が必要（下記） |
| `framebuffer` | Linux bare TTY / 組み込み | `/dev/fb0` へ直接描画。ネイティブ解像度でベクター品質が最大限活きる |

起動時に環境変数と TTY 状態から自動選択する。`--backend` で強制も可能。

### tmux で使う場合

Kitty グラフィクスを tmux 経由で通すには passthrough を有効化する：

```tmux
set -g allow-passthrough on
```

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
                                                             │
                          ┌──────────────────────────────────┤
                          ▼                                   ▼
                 Kitty Graphics Protocol              /dev/fb0 直接描画
```

クレート構成（Cargo workspace）：

| クレート | 役割 |
|---|---|
| `vecview` | CLI エントリ。`typst watch` 起動、ファイル監視、ライブ再描画ループ |
| `vecview-core` | フォーマット非依存の抽象（`Document` / `OutputBackend` / `Page` / `PathData`） |
| `vecview-svg` | `usvg` で SVG をパースし `Page` に変換（曲線情報を保持） |
| `vecview-renderer` | `lyon` テッセレーション + `wgpu` ヘッドレス描画 + RGBA 読み戻し |
| `vecview-output` | バックエンド検出と Kitty / Framebuffer 実装 |
| `vecview-pdf` | PDF 対応（未実装・予定） |

## ビルドと検証

```bash
cargo build
cargo test         # blit 変換・アスペクト計算・GPU 描画のスモークテスト
cargo clippy --all-targets
```

GPU 描画のヘッドレス動作には Vulkan ドライバ（Mesa RADV/ANV 等）が必要。

## 現状（初回スコープ）と今後

実装済み：SVG / Typst の表示、ファイル変更でのライブ再描画、Kitty（+tmux placeholder）/ Framebuffer 出力、
ベクター品質の高解像度描画、インタラクティブなズーム / 複数ページ送り、tmux ペイン内への正しい配置。

未対応（予定）：グラデーション / クリップパスの忠実な描画、PDF 対応、Framebuffer の実機表示確認、
Sixel フォールバック。
