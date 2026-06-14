//! Sixel バックエンド。RGBA8 を減色して Sixel（DEC の DCS グラフィクス）で出力する。
//!
//! Kitty Graphics Protocol 非対応だが Sixel 対応の端末（Windows Terminal 1.22+, xterm,
//! foot, mlterm 等）向け。減色は icy_sixel（Wu 量子化）に任せる。Kitty/Framebuffer の
//! フルカラーと違い 256 色パレットに落ちるため、アンチエイリアスの縁やグラデーションは
//! わずかに階調が減る。等倍画素表示なのでスーパーサンプリングはしない。

use std::io::Write;

use anyhow::{anyhow, Result};
use icy_sixel::{sixel_encode, EncodeOptions};
use vecview_core::OutputBackend;

pub struct SixelBackend {
    /// tmux passthrough（DCS を二重 ESC でラップ）を使うか。
    tmux: bool,
}

impl SixelBackend {
    pub fn new(tmux: bool) -> Self {
        Self { tmux }
    }

    /// Sixel の DCS シーケンスを（必要なら tmux ラップして）書き出す。
    fn write_sixel(&self, out: &mut impl Write, sixel: &str) -> std::io::Result<()> {
        if self.tmux {
            // tmux passthrough: \x1bPtmux; + 内側の ESC を二重化 + \x1b\\。
            // tmux 側 `set -g allow-passthrough on` が必要。位置追跡はされないため最善努力。
            out.write_all(b"\x1bPtmux;")?;
            for &b in sixel.as_bytes() {
                if b == 0x1b {
                    out.write_all(b"\x1b\x1b")?;
                } else {
                    out.write_all(&[b])?;
                }
            }
            out.write_all(b"\x1b\\")?;
        } else {
            out.write_all(sixel.as_bytes())?;
        }
        Ok(())
    }
}

impl OutputBackend for SixelBackend {
    fn name(&self) -> &str {
        if self.tmux {
            "sixel (tmux)"
        } else {
            "sixel"
        }
    }

    fn is_supported(&self) -> bool {
        true
    }

    fn enter(&self) -> Result<()> {
        // 代替スクリーンへ切替 + カーソル非表示。終了時に元の画面へ戻す。
        let mut out = std::io::stdout().lock();
        out.write_all(b"\x1b[?1049h\x1b[?25l")?;
        out.flush()?;
        Ok(())
    }

    fn leave(&self) -> Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l")?;
        out.flush()?;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(b"\x1b[2J\x1b[H")?;
        out.flush()?;
        Ok(())
    }

    fn display(&self, rgba: &[u8], width: u32, height: u32) -> Result<()> {
        // RGBA をそのまま Sixel 化（レンダラー出力は不透明なので背景合成は不要）。
        let opts = EncodeOptions::default(); // 256 色, Wu 量子化, 既定ディフュージョン。
        let sixel = sixel_encode(rgba, width as usize, height as usize, &opts)
            .map_err(|e| anyhow!("Sixel エンコード失敗: {e}"))?;

        let mut out = std::io::stdout().lock();
        // 画面クリア + 左上へ移動してから画像を描く。
        out.write_all(b"\x1b[2J\x1b[H")?;
        self.write_sixel(&mut out, &sixel)?;
        out.flush()?;
        Ok(())
    }
}
