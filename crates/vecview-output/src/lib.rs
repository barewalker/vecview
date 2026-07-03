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

/// Detected terminal cell size in pixels `(width, height)`, or `None` if undeterminable.
///
/// Resolution order (cached — the terminal round-trip happens at most once):
/// 1. `VECVIEW_CELL_PX=WxH` manual override.
/// 2. The pixel size reported by the kernel/terminal (`TIOCGWINSZ`) — works outside tmux.
/// 3. A one-shot `CSI 16 t` query to the real terminal (wrapped in tmux passthrough when inside
///    tmux, which otherwise zeroes the pixel size), parsing the `CSI 6 ; height ; width t` reply.
///    Needs an interactive TTY already in raw mode, so call it once at startup after raw mode is on.
pub fn cell_px() -> Option<(u32, u32)> {
    use std::sync::OnceLock;
    static CELL: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *CELL.get_or_init(detect_cell_px)
}

fn detect_cell_px() -> Option<(u32, u32)> {
    if let Some(c) = std::env::var("VECVIEW_CELL_PX")
        .ok()
        .and_then(|s| parse_wxh(&s))
    {
        return Some(c);
    }
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 {
            return Some((
                (ws.width as u32 / ws.columns as u32).max(1),
                (ws.height as u32 / ws.rows as u32).max(1),
            ));
        }
    }
    query_cell_px()
}

/// Parse `"WxH"` (separator `x`/`X`) into pixel dimensions, clamped to 1..=256.
fn parse_wxh(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    Some((w.clamp(1, 256), h.clamp(1, 256)))
}

/// Ask the real terminal for its cell size via `CSI 16 t`. Only on an interactive TTY (the reply
/// must be readable from stdin in raw mode, without echo/line buffering).
fn query_cell_px() -> Option<(u32, u32)> {
    use std::io::Write;
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return None;
    }
    let in_tmux = std::env::var_os("TMUX").is_some();
    {
        let mut out = std::io::stdout().lock();
        let inner = b"\x1b[16t";
        if in_tmux {
            // \ePtmux; <inner, each ESC doubled> \e\\  — reach the outer terminal, not tmux.
            let _ = out.write_all(b"\x1bPtmux;");
            for &b in inner {
                if b == 0x1b {
                    let _ = out.write_all(b"\x1b\x1b");
                } else {
                    let _ = out.write_all(&[b]);
                }
            }
            let _ = out.write_all(b"\x1b\\");
        } else {
            let _ = out.write_all(inner);
        }
        let _ = out.flush();
    }
    read_cell_reply(std::time::Duration::from_millis(200))
}

/// Read a `CSI 6 ; height ; width t` reply from stdin within `timeout`, returning `(width, height)`.
fn read_cell_reply(timeout: std::time::Duration) -> Option<(u32, u32)> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::io::Read;
    use std::os::fd::BorrowedFd;
    use std::time::Instant;

    // SAFETY: fd 0 (stdin) is valid for the lifetime of an interactive process.
    let fd = unsafe { BorrowedFd::borrow_raw(0) };
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    let mut tmp = [0u8; 32];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
        let to = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::ZERO);
        match poll(&mut fds, to) {
            Ok(0) => return None, // timed out
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return None,
        }
        match std::io::stdin().lock().read(&mut tmp) {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => return None,
        }
        if let Some(c) = parse_cell_reply(&buf) {
            return Some(c);
        }
        // A terminator arrived but didn't parse: stop so we don't swallow later keystrokes.
        if buf.contains(&b't') {
            return None;
        }
    }
}

/// Parse `ESC [ 6 ; <height> ; <width> t` anywhere in `buf` into `(width, height)`.
fn parse_cell_reply(buf: &[u8]) -> Option<(u32, u32)> {
    let s = std::str::from_utf8(buf).ok()?;
    let rest = &s[s.find("\x1b[6;")? + 4..];
    let end = rest.find('t')?;
    let mut it = rest[..end].split(';');
    let h: u32 = it.next()?.trim().parse().ok()?;
    let w: u32 = it.next()?.trim().parse().ok()?;
    (w > 0 && h > 0).then_some((w.clamp(1, 256), h.clamp(1, 256)))
}

/// The displayable pixel area of the output target (width/height).
#[derive(Clone, Copy, Debug)]
pub struct DisplaySize {
    pub width: u32,
    pub height: u32,
}

/// Auto-detects the backend. Can also be specified explicitly via `force`
/// (kitty/tmux/herdr/sixel/framebuffer).
pub fn detect_backend(force: Option<&str>) -> Box<dyn OutputBackend> {
    if let Some(name) = force {
        return forced_backend(name);
    }

    let in_tmux = std::env::var_os("TMUX").is_some();
    let in_herdr = is_herdr();
    let kitty_capable = is_kitty_capable();
    let is_tty = std::io::stdout().is_terminal();

    // 0. herdr renders kitty graphics through an embedded ghostty core, but only from *virtual*
    //    placements: it never reports a pixel cell size to the pane, so a direct (pixel) placement
    //    can't be positioned and is silently dropped. Emit raw APC (herdr parses it directly, so no
    //    tmux DCS wrapping) plus Unicode-placeholder placement. Checked before kitty_capable because
    //    KITTY_WINDOW_ID leaks into the pane and would otherwise pick the direct-placement path.
    if in_herdr {
        return Box::new(KittyBackend::placeholder_raw());
    }

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
        "herdr" => Box::new(KittyBackend::placeholder_raw()),
        "sixel" => Box::new(SixelBackend::new(in_tmux)),
        #[cfg(target_os = "linux")]
        "framebuffer" => Box::new(FramebufferBackend::new("/dev/fb0")),
        // An unknown specification falls back to Kitty based on in_tmux.
        _ => Box::new(KittyBackend::new(in_tmux)),
    }
}

/// Whether we're running inside a herdr-managed pane (its env is exported to every pane).
fn is_herdr() -> bool {
    std::env::var_os("HERDR_ENV").is_some() || std::env::var_os("HERDR_PANE_ID").is_some()
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
