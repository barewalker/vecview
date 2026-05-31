//! 出力バックエンド。レンダラーが生成した RGBA8 を端末/フレームバッファへ送る。
//!
//! - [`KittyBackend`]: Kitty Graphics Protocol（Ghostty / WezTerm / kitty）。tmux 内では
//!   DCS passthrough でラップ。開発環境（Ghostty+tmux）での即時確認に使う。
//! - [`FramebufferBackend`]: `/dev/fb0` への直接描画。本番のネイティブ解像度ベクター表示。
//!
//! [`detect_backend`] が環境変数と TTY 状態から自動選択する。

mod kitty;

#[cfg(target_os = "linux")]
mod framebuffer;

pub use kitty::KittyBackend;

#[cfg(target_os = "linux")]
pub use framebuffer::{blit, FbInfo, FramebufferBackend};

use std::io::IsTerminal;

use vecview_core::OutputBackend;

/// 出力先の表示可能ピクセル領域（幅・高さ）。
#[derive(Clone, Copy, Debug)]
pub struct DisplaySize {
    pub width: u32,
    pub height: u32,
}

/// バックエンドを自動検出する。`force` で明示指定（kitty/tmux/framebuffer）も可能。
pub fn detect_backend(force: Option<&str>) -> Box<dyn OutputBackend> {
    if let Some(name) = force {
        return forced_backend(name);
    }

    let in_tmux = std::env::var_os("TMUX").is_some();
    let kitty_capable = is_kitty_capable();
    let is_tty = std::io::stdout().is_terminal();

    // 1. Kitty 対応端末（tmux 内ならラップ）。開発環境はここに該当。
    if kitty_capable || in_tmux {
        return Box::new(KittyBackend::new(in_tmux));
    }

    // 2. bare TTY かつ /dev/fb0 が存在 → フレームバッファ。
    #[cfg(target_os = "linux")]
    if is_tty && std::path::Path::new("/dev/fb0").exists() {
        return Box::new(FramebufferBackend::new("/dev/fb0"));
    }

    // 3. フォールバック（最善努力で Kitty）。
    let _ = is_tty;
    Box::new(KittyBackend::new(in_tmux))
}

fn forced_backend(name: &str) -> Box<dyn OutputBackend> {
    match name {
        "kitty" => Box::new(KittyBackend::new(false)),
        "tmux" => Box::new(KittyBackend::new(true)),
        #[cfg(target_os = "linux")]
        "framebuffer" => Box::new(FramebufferBackend::new("/dev/fb0")),
        _ => {
            // 不明な指定は環境変数で in_tmux を見て Kitty にフォールバック。
            let in_tmux = std::env::var_os("TMUX").is_some();
            Box::new(KittyBackend::new(in_tmux))
        }
    }
}

/// Kitty Graphics Protocol を解する端末か（tmux で実体が隠れている場合を除く）。
fn is_kitty_capable() -> bool {
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("ghostty") | Ok("WezTerm") => true,
        _ => std::env::var("TERM")
            .map(|t| t.contains("kitty"))
            .unwrap_or(false),
    }
}
