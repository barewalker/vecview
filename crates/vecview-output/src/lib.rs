//! Output backends. Send the RGBA8 produced by the renderer to the terminal/framebuffer.
//!
//! - [`KittyBackend`]: Kitty Graphics Protocol (Ghostty / WezTerm / kitty). Inside tmux, wrapped in
//!   DCS passthrough. Used for immediate verification in the dev environment (Ghostty+tmux).
//! - [`FramebufferBackend`]: direct drawing to `/dev/fb0`. The production native-resolution vector display.
//! - [`SixelBackend`]: Sixel (DEC DCS graphics). For terminals that don't support Kitty but do
//!   support Sixel (Windows Terminal 1.22+ etc.). Color-reduces to 256 colors.
//!
//! [`detect_backend`] selects automatically based on environment variables and TTY state.

mod kitty;
mod sixel;

#[cfg(target_os = "linux")]
mod framebuffer;

pub use kitty::KittyBackend;
pub use sixel::SixelBackend;

#[cfg(target_os = "linux")]
pub use framebuffer::{blit, FbInfo, FramebufferBackend};

use std::io::IsTerminal;

use vecview_core::OutputBackend;

/// The displayable pixel area of the output target (width/height).
#[derive(Clone, Copy, Debug)]
pub struct DisplaySize {
    pub width: u32,
    pub height: u32,
}

/// Auto-detects the backend. Can also be specified explicitly via `force` (kitty/tmux/framebuffer).
pub fn detect_backend(force: Option<&str>) -> Box<dyn OutputBackend> {
    if let Some(name) = force {
        return forced_backend(name);
    }

    let in_tmux = std::env::var_os("TMUX").is_some();
    let kitty_capable = is_kitty_capable();
    let is_tty = std::io::stdout().is_terminal();

    // 1. Kitty-capable terminal (wrapped if inside tmux). The dev environment falls here.
    if kitty_capable {
        return Box::new(KittyBackend::new(in_tmux));
    }

    // 2. Windows Terminal (Sixel support). However, WT_SESSION often doesn't propagate inside WSL,
    //    so auto-detection is unreliable. For certainty, use `--backend sixel` or VECVIEW_BACKEND.
    if is_windows_terminal() {
        return Box::new(SixelBackend::new(in_tmux));
    }

    // 3. Even for an unknown terminal, if inside tmux, try Kitty (passthrough) as before.
    if in_tmux {
        return Box::new(KittyBackend::new(true));
    }

    // 4. Bare TTY and /dev/fb0 exists -> framebuffer.
    #[cfg(target_os = "linux")]
    if is_tty && std::path::Path::new("/dev/fb0").exists() {
        return Box::new(FramebufferBackend::new("/dev/fb0"));
    }

    // 5. Fallback (best-effort Kitty).
    let _ = is_tty;
    Box::new(KittyBackend::new(in_tmux))
}

fn forced_backend(name: &str) -> Box<dyn OutputBackend> {
    let in_tmux = std::env::var_os("TMUX").is_some();
    match name {
        "kitty" => Box::new(KittyBackend::new(false)),
        "tmux" => Box::new(KittyBackend::new(true)),
        "sixel" => Box::new(SixelBackend::new(in_tmux)),
        #[cfg(target_os = "linux")]
        "framebuffer" => Box::new(FramebufferBackend::new("/dev/fb0")),
        // An unknown specification falls back to Kitty based on in_tmux.
        _ => Box::new(KittyBackend::new(in_tmux)),
    }
}

/// Whether we're on Windows Terminal (best-effort). Note that WT_SESSION is often not visible inside WSL.
fn is_windows_terminal() -> bool {
    std::env::var_os("WT_SESSION").is_some() || std::env::var_os("WT_PROFILE_ID").is_some()
}

/// Whether the terminal understands the Kitty Graphics Protocol (excluding cases where the real one is hidden behind tmux).
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
