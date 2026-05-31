# vecview — Claude Code 実装指示書

## プロジェクト概要

**vecview** は、ベクターグラフィクス（PDF・SVG等）をターミナル内にベクターレンダリングで表示するCLIツール。
ラスタ化せず、lyon テッセレーション + wgpu GPU描画により、ズームしても劣化しない表示を実現する。
ターミナル非依存設計で、Kitty Graphics Protocol / tmux DCS passthrough / Sixel / Linux Framebuffer を自動検出して切り替える。

### 設計上の重要な位置づけ

ターミナルプロトコル（Kitty / Sixel）経由ではセルグリッド解像度が律速となりベクター品質の恩恵が薄い。
**Framebufferバックエンド（`/dev/fb0`）では画面ネイティブ解像度で直接描画するため、ベクターレンダリングの価値が最大限に発揮される。**

主な用途：
- Raspberry Pi・組み込みLinuxでGUIなしにPDF/SVG表示
- ヘッドレスサーバーのコンソール表示
- デジタルサイネージ
- Ghostty / WezTerm 等では Kitty Protocol 経由で日常的な閲覧にも使用可能

---

## フェーズ1：ワークスペース構成（最初に着手）

### ゴール
`cargo build` が全クレートで通るスケルトンを作る。ロジックはまだ書かない。

### ディレクトリ構成

```
vecview/
├── Cargo.toml               # workspace root
├── crates/
│   ├── vecview/             # CLI エントリポイント
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── main.rs
│   ├── vecview-core/        # コアライブラリ（トレイト定義）
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   ├── vecview-pdf/         # PDF パーサー
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   ├── vecview-svg/         # SVG パーサー
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   ├── vecview-renderer/    # ベクターレンダラー（lyon + wgpu）
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   └── vecview-output/      # 出力バックエンド（Kitty / Sixel / tmux / Framebuffer）
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs
└── README.md
```

### workspace Cargo.toml

```toml
[workspace]
members = [
    "crates/vecview",
    "crates/vecview-core",
    "crates/vecview-pdf",
    "crates/vecview-svg",
    "crates/vecview-renderer",
    "crates/vecview-output",
]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2021"
authors = ["Mitsuaki Takeuchi"]
license = "Apache-2.0"
repository = "https://github.com/barewalker/vecview"

[workspace.dependencies]
# コア
anyhow = "1"
thiserror = "2"

# フォーマット
pdfium-render = "0.8"
resvg = "0.47"
usvg = "0.47"

# レンダリング
lyon = "1"
wgpu = "24"

# ターミナル
crossterm = "0.28"
base64 = "0.22"

# Framebuffer（Linux専用、optional）
memmap2 = "0.9"

# CLI
clap = { version = "4", features = ["derive"] }
```

### 各クレートの Cargo.toml（スケルトン）

**crates/vecview/Cargo.toml**
```toml
[package]
name = "vecview"
version.workspace = true
edition.workspace = true

[[bin]]
name = "vecview"
path = "src/main.rs"

[dependencies]
vecview-core = { path = "../vecview-core" }
vecview-pdf  = { path = "../vecview-pdf" }
vecview-svg  = { path = "../vecview-svg" }
vecview-output = { path = "../vecview-output" }
clap.workspace = true
anyhow.workspace = true
```

**crates/vecview-core/Cargo.toml**
```toml
[package]
name = "vecview-core"
version.workspace = true
edition.workspace = true

[dependencies]
anyhow.workspace = true
thiserror.workspace = true
```

**crates/vecview-pdf/Cargo.toml**
```toml
[package]
name = "vecview-pdf"
version.workspace = true
edition.workspace = true

[dependencies]
vecview-core = { path = "../vecview-core" }
pdfium-render.workspace = true
anyhow.workspace = true
```

**crates/vecview-svg/Cargo.toml**
```toml
[package]
name = "vecview-svg"
version.workspace = true
edition.workspace = true

[dependencies]
vecview-core = { path = "../vecview-core" }
resvg.workspace = true
usvg.workspace = true
anyhow.workspace = true
```

**crates/vecview-renderer/Cargo.toml**
```toml
[package]
name = "vecview-renderer"
version.workspace = true
edition.workspace = true

[dependencies]
vecview-core = { path = "../vecview-core" }
lyon.workspace = true
wgpu.workspace = true
anyhow.workspace = true
```

**crates/vecview-output/Cargo.toml**
```toml
[package]
name = "vecview-output"
version.workspace = true
edition.workspace = true

[dependencies]
vecview-core = { path = "../vecview-core" }
crossterm.workspace = true
base64.workspace = true
anyhow.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
memmap2.workspace = true
```

### スケルトン src ファイル

各 `lib.rs` はひとまず空で OK：
```rust
// placeholder
```

`crates/vecview/src/main.rs`：
```rust
fn main() {
    println!("vecview");
}
```

### 確認コマンド
```bash
cargo build
cargo clippy
```

---

## フェーズ2：vecview-core トレイト定義

### ゴール
フォーマット非依存の抽象インターフェースを定義する。
パーサーもレンダラーも出力バックエンドも、このトレイトに依存する。

### 実装内容（crates/vecview-core/src/lib.rs）

```rust
use anyhow::Result;

/// ベクタードキュメント1ページの抽象表現
pub struct Page {
    pub width: f32,
    pub height: f32,
    pub commands: Vec<DrawCommand>,
}

/// 描画コマンド（ベクターパス・テキスト・画像）
pub enum DrawCommand {
    Path(PathData),
    Text(TextData),
    Image(ImageData),
}

pub struct PathData {
    pub points: Vec<[f32; 2]>,
    pub fill: Option<Color>,
    pub stroke: Option<Stroke>,
}

pub struct TextData {
    pub x: f32,
    pub y: f32,
    pub content: String,
    pub size: f32,
    pub color: Color,
}

pub struct ImageData {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub pixels: Vec<u8>, // RGBA
}

#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

pub struct Stroke {
    pub color: Color,
    pub width: f32,
}

/// フォーマットパーサーが実装するトレイト
pub trait Document: Send + Sync {
    fn page_count(&self) -> usize;
    fn render_page(&self, index: usize) -> Result<Page>;
}

/// 出力バックエンドが実装するトレイト
pub trait OutputBackend: Send + Sync {
    fn name(&self) -> &str;
    fn is_supported(&self) -> bool;
    fn display(&self, pixels: &[u8], width: u32, height: u32) -> Result<()>;
}
```

---

## フェーズ3：出力バックエンド自動検出

### ゴール
起動時にターミナル種別を検出し、適切なバックエンドを選択する。

### 検出ロジック（crates/vecview-output/src/lib.rs）

```rust
pub fn detect_backend() -> Box<dyn OutputBackend> {
    // 1. Framebuffer 優先（明示指定 or TTYかつ/dev/fb0が存在）
    if std::path::Path::new("/dev/fb0").exists()
        && !std::io::stdout().is_terminal()  // ターミナルエミュレータ外
    {
        return Box::new(FramebufferBackend::new("/dev/fb0"));
    }

    // 2. tmux 内かどうか
    let in_tmux = std::env::var("TMUX").is_ok();

    // 3. ターミナル種別
    let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term_kitty = std::env::var("KITTY_WINDOW_ID").is_ok();

    if term_kitty || term == "ghostty" || term == "WezTerm" {
        if in_tmux {
            Box::new(KittyTmuxBackend)  // DCS passthrough ラップ
        } else {
            Box::new(KittyBackend)
        }
    } else {
        Box::new(SixelBackend)  // Windows Terminal 等フォールバック
    }
}
```

### バックエンド一覧

| 構造体 | 対象 | 実装方針 |
|---|---|---|
| `KittyBackend` | Kitty / Ghostty / WezTerm | APC `\x1b_Ga=T,...\x1b\\` |
| `KittyTmuxBackend` | tmux inside Kitty 系 | DCS `\x1bPtmux;\x1b` でラップ |
| `SixelBackend` | Windows Terminal 等 | Sixel エスケープシーケンス |
| `FramebufferBackend` | Linux bare console / 組み込み | `/dev/fb0` へ直接 mmap 書き込み |

### FramebufferBackend 実装方針

```rust
// crates/vecview-output/src/framebuffer.rs
use memmap2::MmapMut;
use std::fs::OpenOptions;

pub struct FramebufferBackend {
    path: String,
}

impl FramebufferBackend {
    pub fn new(path: &str) -> Self {
        Self { path: path.to_string() }
    }
}

impl OutputBackend for FramebufferBackend {
    fn name(&self) -> &str { "framebuffer" }

    fn is_supported(&self) -> bool {
        std::path::Path::new(&self.path).exists()
    }

    fn display(&self, pixels: &[u8], width: u32, height: u32) -> Result<()> {
        // 1. /dev/fb0 を開く
        let file = OpenOptions::new().read(true).write(true).open(&self.path)?;

        // 2. フレームバッファ情報を ioctl で取得（解像度・bits_per_pixel）
        //    linux_raw_sys または nix クレートの FBIOGET_VSCREENINFO を使用
        //    → fb_var_screeninfo から xres / yres / bits_per_pixel を取得

        // 3. mmap でフレームバッファにマップ
        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        // 4. RGBA → フレームバッファのピクセルフォーマットに変換して書き込み
        //    一般的なフォーマット：BGR24 または BGRA32
        //    bits_per_pixel に応じて分岐
        for (i, chunk) in pixels.chunks(4).enumerate() {
            let offset = i * 4; // BGRA32 想定
            if offset + 3 >= mmap.len() { break; }
            mmap[offset]     = chunk[2]; // B
            mmap[offset + 1] = chunk[1]; // G
            mmap[offset + 2] = chunk[0]; // R
            mmap[offset + 3] = chunk[3]; // A
        }
        Ok(())
    }
}
```

**注意点：**
- `/dev/fb0` へのアクセスには `video` グループへの所属か `sudo` が必要
- `ioctl` による `fb_var_screeninfo` 取得には `nix` クレート（`nix::ioctl_read!`）を使う
- ピクセルフォーマットはデバイスにより異なる（BGRA32 が最も一般的）
- 解像度が合わない場合はスケーリングが必要（wgpu レンダー時に合わせる）

---

## フェーズ4：SVGパーサー実装（最初の動くデモ）

### ゴール
`vecview diagram.svg` で SVG がターミナルに表示される。

### 実装方針

- `resvg` + `usvg` で SVG を `tiny-skia` の `Pixmap` にレンダリング
- ただし最終的にはラスタ化を排除し、`usvg::Tree` から `DrawCommand` を直接生成する方向へ移行
- フェーズ4では resvg のラスタ出力を使い動作確認し、フェーズ5でベクター化

```rust
// vecview-svg/src/lib.rs の骨格
use vecview_core::{Document, Page, DrawCommand};
use anyhow::Result;

pub struct SvgDocument {
    tree: usvg::Tree,
}

impl SvgDocument {
    pub fn open(path: &str) -> Result<Self> {
        let data = std::fs::read(path)?;
        let tree = usvg::Tree::from_data(&data, &usvg::Options::default())?;
        Ok(Self { tree })
    }
}

impl Document for SvgDocument {
    fn page_count(&self) -> usize { 1 }

    fn render_page(&self, _index: usize) -> Result<Page> {
        // TODO: usvg::Tree を DrawCommand に変換
        todo!()
    }
}
```

---

## フェーズ5：lyon + wgpu ベクターレンダラー

### ゴール
`DrawCommand::Path` を lyon でテッセレーションし、wgpu でピクセルバッファに描画。

### 実装方針

```
usvg::Tree
  └─ 各 Node を走査
       └─ Path → lyon::path::Path に変換
            └─ lyon::tessellation::FillTessellator でメッシュ化
                 └─ wgpu の vertex buffer に投入
                      └─ render pass でオフスクリーン描画
                           └─ RGBA ピクセルバッファ取得
                                └─ OutputBackend::display() へ
```

### 注意点
- wgpu はオフスクリーンレンダリング（`wgpu::TextureUsages::COPY_SRC`）を使う
- フォントレンダリングは `glyphon` クレートが使いやすい
- wgpu の初期化は非同期（`pollster` で block_on）

---

## フェーズ6：PDF対応

### ゴール
`vecview paper.pdf` で PDF が表示される。

### 注意点
- `pdfium-render` は pdfium の動的ライブラリが別途必要
- 開発時は `pdfium-binaries` から pre-built を取得
- `pdfium-render` が返す `PdfPageRenderConfig` を使いラスタ化は可能だが、
  最終的には `PdfPage::objects()` からベクターパスを取り出す方向で実装

---

## フェーズ7：CLIインターフェース整備

### コマンド仕様

```
vecview [OPTIONS] <FILE>

Arguments:
  <FILE>  表示するファイル（PDF / SVG / EPS / XPS）

Options:
  -p, --page <PAGE>      表示ページ（PDF用、1始まり）[default: 1]
  -z, --zoom <ZOOM>      ズーム倍率（%）[default: 100]
  -b, --backend <NAME>   出力バックエンド強制指定 [kitty|sixel|tmux|framebuffer]
  -i, --interactive      インタラクティブモード強制
  -h, --help             ヘルプ表示
  -V, --version          バージョン表示
```

### TTY判定

```rust
use std::io::IsTerminal;

if std::io::stdout().is_terminal() {
    // インタラクティブモード：j/k でページ操作
} else {
    // パイプモード：1ページ出力して終了
}
```

### インタラクティブキーバインド

| キー | 動作 |
|---|---|
| `j` / `Space` | 次ページ |
| `k` | 前ページ |
| `+` / `=` | ズームイン |
| `-` | ズームアウト |
| `0` | ズームリセット |
| `q` / `Esc` | 終了 |

---

## 優先実装順

1. **フェーズ1**：ワークスペース構成 → `cargo build` 確認
2. **フェーズ2**：`vecview-core` トレイト定義
3. **フェーズ3**：出力バックエンド自動検出（Framebuffer含む）
4. **フェーズ4**：SVG表示（resvg ラスタ経由でまず動かす）
5. **フェーズ7**：CLIインターフェース（使える状態にする）
6. **フェーズ5**：lyon + wgpu ベクターレンダラー（品質向上）
7. **フェーズ6**：PDF対応
8. **Framebuffer品質向上**：ioctl による解像度自動取得・スケーリング

---

## 開発環境メモ

- NucBox M7 Pro / Ubuntu 24.04
- Rust stable（`rustup update` で最新に）
- Ghostty ターミナル（Kitty Graphics Protocol 対応）
- `cargo add` でクレート追加時は workspace.dependencies も更新すること
- pdfium pre-built: https://github.com/bblanchon/pdfium-binaries/releases
- Framebuffer動作確認：`sudo usermod -aG video $USER` でグループ追加後ログアウト→ログイン
- Framebuffer解像度確認：`cat /sys/class/graphics/fb0/virtual_size`
