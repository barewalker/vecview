//! Sixel backend. Color-reduces RGBA8 and outputs it as Sixel (DEC's DCS graphics).
//!
//! For terminals that don't support the Kitty Graphics Protocol but do support Sixel (Windows
//! Terminal 1.22+, xterm, foot, mlterm, etc.). Color reduction is left to icy_sixel (Wu
//! quantization). Unlike the full color of Kitty/Framebuffer, it drops to a 256-color palette, so
//! anti-aliased edges and gradients lose a little gradation. Display is at 1:1 pixels, so there is
//! no supersampling.

use std::cell::RefCell;
use std::io::Write;

use anyhow::{anyhow, Result};
use icy_sixel::{sixel_encode, EncodeOptions};
use vecview_core::OutputBackend;

pub struct SixelBackend {
    /// Whether we are inside tmux (for the name display).
    in_tmux: bool,
    /// Whether to use tmux passthrough (wrapping the DCS in doubled ESCs). When tmux can render
    /// sixel natively, set this to false and let tmux interpret raw sixel and clip-draw it within
    /// the pane. With passthrough, tmux does not track or clip the content, so when an image
    /// overflows the pane, the whole physical screen (including neighboring panes) scrolls and
    /// breaks. Native support avoids this.
    passthrough: bool,
    /// The most recently displayed sixel (icy_sixel's output). A cache for restoring, without
    /// re-rasterizing, an image that tmux erased during passthrough. Updated on every display().
    last: RefCell<Option<String>>,
    /// vecview's own tmux pane ID ($TMUX_PANE). During passthrough, used to determine whether this
    /// pane is active (focused) and to suppress drawing while it is inactive.
    pane: Option<String>,
}

impl SixelBackend {
    pub fn new(tmux: bool) -> Self {
        // tmux native sixel sometimes can't redraw the actual image over WT->SSH->tmux and instead
        // shows a placeholder ("SIXEL IMAGE (WxH)") with no picture. So the default is passthrough.
        // In environments where native works, you can try it with VECVIEW_SIXEL_NATIVE=1 (requires
        // sixel in client_termfeatures).
        let native = tmux
            && std::env::var_os("VECVIEW_SIXEL_NATIVE").is_some()
            && tmux_supports_sixel();
        Self {
            in_tmux: tmux,
            passthrough: tmux && !native,
            last: RefCell::new(None),
            pane: std::env::var("TMUX_PANE").ok(),
        }
    }

    /// During passthrough, whether our own pane is active (focused). Passthrough sixel is drawn at
    /// the physical cursor position, and tmux only places the physical cursor in the focused pane,
    /// so if inactive the image would be drawn in another pane. We suppress output while that is the
    /// case. Outside tmux, or when it can't be determined, returns true (always draw).
    fn pane_is_active(&self) -> bool {
        if !self.passthrough {
            return true;
        }
        let Some(pane) = self.pane.as_deref() else {
            return true;
        };
        std::process::Command::new("tmux")
            .args(["display-message", "-p", "-t", pane, "#{pane_active}"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
            .unwrap_or(true)
    }

    /// Writes out the Sixel DCS sequence (wrapping it in tmux passthrough if necessary).
    fn write_sixel(&self, out: &mut impl Write, sixel: &str) -> std::io::Result<()> {
        if self.passthrough {
            // Passthrough for old tmux (builds without sixel support): \x1bPtmux; + inner ESC
            // doubling + \x1b\\. Requires `set -g allow-passthrough on` on the tmux side. Best-effort:
            // the position is not tracked and it is not clipped.
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
            // Non-tmux, or tmux native sixel: emit it raw (tmux clip-draws it within the pane).
            out.write_all(sixel.as_bytes())?;
        }
        Ok(())
    }
}

/// Determines whether tmux can render sixel natively. `sixel` is included in `client_termfeatures`
/// only when the tmux build supports sixel and the outer terminal also advertises sixel support.
/// If it can't be obtained, returns false (i.e. fall back to the traditional passthrough).
fn tmux_supports_sixel() -> bool {
    std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("sixel"))
        .unwrap_or(false)
}

impl OutputBackend for SixelBackend {
    fn name(&self) -> &str {
        match (self.in_tmux, self.passthrough) {
            (true, true) => "sixel (tmux passthrough)",
            (true, false) => "sixel (tmux native)",
            _ => "sixel",
        }
    }

    fn is_supported(&self) -> bool {
        true
    }

    fn enter(&self) -> Result<()> {
        // Switch to the alternate screen + hide the cursor. On exit, return to the original screen.
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
        // We're switching to text display, so discard the redraw cache too (stop restoring the
        // erased image).
        *self.last.borrow_mut() = None;
        let mut out = std::io::stdout().lock();
        out.write_all(b"\x1b[2J\x1b[H")?;
        out.flush()?;
        Ok(())
    }

    fn display(&self, rgba: &[u8], width: u32, height: u32) -> Result<()> {
        // Turn the RGBA directly into Sixel (the renderer output is opaque, so no background
        // compositing is needed).
        let opts = EncodeOptions::default(); // 256 colors, Wu quantization, default diffusion.
        let sixel = sixel_encode(rgba, width as usize, height as usize, &opts)
            .map_err(|e| anyhow!("Sixel encoding failed: {e}"))?;

        // Cache it for passthrough redraws (so we can restore without re-rasterizing if erased).
        *self.last.borrow_mut() = Some(sixel);
        // Don't draw while our own pane is inactive (so we don't pollute another pane). When focus
        // returns, the periodic redraw restores it.
        if !self.pane_is_active() {
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        // Move to the top-left and overwrite the same area. Clearing the whole screen with 2J would
        // insert a blank frame between erasing and the next sixel appearing, causing flicker
        // (especially pronounced over SSH+tmux where transfer is slow; during a typst build, a
        // redraw runs on every save, so it flickers badly). The image is always the whole pane at
        // the same size, so the previous frame is fully covered by the new frame and no clear is
        // needed. The same minimal update as redraw().
        out.write_all(b"\x1b[H")?;
        if let Some(sixel) = self.last.borrow().as_ref() {
            self.write_sixel(&mut out, sixel)?;
        }
        out.flush()?;
        Ok(())
    }

    fn redraw(&self) -> Result<()> {
        // Don't draw if our own pane is inactive (to prevent drawing into another pane).
        if !self.pane_is_active() {
            return Ok(());
        }
        // Resend the most recent frame, which tmux may have erased, in place (at the pane origin).
        // We don't clear the screen (2J) and just overwrite the same pixels, which keeps flicker down.
        if let Some(sixel) = self.last.borrow().as_ref() {
            let mut out = std::io::stdout().lock();
            out.write_all(b"\x1b[H")?;
            self.write_sixel(&mut out, sixel)?;
            out.flush()?;
        }
        Ok(())
    }

    fn wants_periodic_redraw(&self) -> bool {
        // Passthrough only: tmux doesn't track it and erases it on its own, so periodic restoration
        // is needed. For native / non-tmux, tmux/the terminal retains the image, so it's not needed.
        self.passthrough
    }
}
