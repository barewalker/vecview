//! Kitty Graphics Protocol backend. Transfers RGBA8 directly with `f=32`.
//!
//! It has two placement modes:
//! - **Direct placement** (non-tmux): `a=T` displays the image directly at the cursor position.
//! - **Unicode placeholder** (tmux): the image is transferred as a virtual placement (`U=1`), and
//!   "text cells" are drawn using the `U+10EEEE` placeholder character with row/column diacritics.
//!   tmux tracks these as ordinary text cells, so the image stays correctly within the pane
//!   boundaries. This mode is used inside tmux because `a=T` direct placement would otherwise
//!   always end up at the top-left of the window, since tmux does not understand the pane
//!   placement of graphics. Requires `set -g allow-passthrough on` on the tmux side.

use std::cell::RefCell;
use std::io::Write;

use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use vecview_core::OutputBackend;

/// Compresses RGBA with zlib (RFC 1950). Prepares data to send via kitty graphics `o=z`. The
/// compression level favors speed (mostly-white pages compress well even at a low level). Writing
/// to a Vec never fails.
fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

/// Maximum length of one chunk of the base64 payload (Kitty's recommended 4096 bytes).
const CHUNK: usize = 4096;

/// Unicode placeholder character U+10EEEE.
const PLACEHOLDER: char = '\u{10EEEE}';

pub struct KittyBackend {
    /// Whether to use tmux passthrough (plus placeholder placement).
    tmux: bool,
    /// Base of the image IDs (23 bits) unique to this vecview instance. Kitty image IDs are global
    /// to the terminal, so when multiple vecviews run across multiple windows, a fixed ID would
    /// cause them to overwrite and wrongly delete each other's images. We derive a unique value
    /// from the PID and encode it in the placeholder's foreground color (24-bit truecolor). For
    /// double buffering we alternate this with the value that has bit23 set ([`next_id`](Self::next_id)).
    base_id: u32,
    /// The buffer to use next (false=base_id, true=base_id|0x800000). Flips on every display.
    toggle: RefCell<bool>,
    /// The image ID currently displayed (the previous frame). To be freed after the next frame is drawn.
    live: RefCell<Option<u32>>,
    /// The most recent placeholder cell footprint (cols, rows). Only on a change do we clear the
    /// whole screen to sweep away the previous frame's leftover cells.
    last_footprint: RefCell<Option<(u32, u32)>>,
}

impl KittyBackend {
    pub fn new(tmux: bool) -> Self {
        // Fit the PID into 23 bits to use as the base ID (avoiding 0). Leave bit23 free, as it is
        // used for the double-buffer toggle. The chance of colliding with another concurrent
        // instance is effectively zero.
        let base_id = (std::process::id() & 0x007F_FFFF).max(1);
        Self {
            tmux,
            base_id,
            toggle: RefCell::new(false),
            live: RefCell::new(None),
            last_footprint: RefCell::new(None),
        }
    }

    /// The image ID to use for the next frame (alternating between base_id and base_id|bit23). With
    /// double buffering, the new frame is transferred under a different ID than the old frame, and
    /// the old ID is freed after drawing, so we switch without clearing the screen during transfer
    /// (preventing flicker). Each ID is always freed before reuse, so nothing accumulates on the
    /// terminal side.
    fn next_id(&self) -> u32 {
        let mut t = self.toggle.borrow_mut();
        *t = !*t;
        if *t {
            self.base_id | 0x0080_0000
        } else {
            self.base_id
        }
    }

    /// Writes an APC that frees the image and placement for the given ID (d=I,i=id). It only deletes
    /// our own ID, so it does not affect other instances' images.
    fn delete_id(&self, out: &mut impl Write, id: u32) -> std::io::Result<()> {
        let body = format!("_Ga=d,d=I,i={id}");
        self.write_apc(out, body.as_bytes())
    }

    /// Frees both image IDs used by this instance (on clear/exit).
    fn delete_own(&self, out: &mut impl Write) -> std::io::Result<()> {
        self.delete_id(out, self.base_id)?;
        self.delete_id(out, self.base_id | 0x0080_0000)?;
        Ok(())
    }

    /// Writes out an APC graphics sequence (wrapping it for tmux if necessary).
    /// `body` is the `_G...` part (the content, without the leading ESC or trailing ST).
    fn write_apc(&self, out: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
        if self.tmux {
            out.write_all(b"\x1bPtmux;")?;
            // Embed the entire inner APC sequence (including the ESC), doubling each ESC.
            let mut seq = Vec::with_capacity(body.len() + 4);
            seq.extend_from_slice(b"\x1b");
            seq.extend_from_slice(body);
            seq.extend_from_slice(b"\x1b\\");
            for &b in &seq {
                if b == 0x1b {
                    out.write_all(b"\x1b\x1b")?;
                } else {
                    out.write_all(&[b])?;
                }
            }
            out.write_all(b"\x1b\\")?;
        } else {
            out.write_all(b"\x1b")?;
            out.write_all(body)?;
            out.write_all(b"\x1b\\")?;
        }
        Ok(())
    }

    /// Transfers the image data in 4096-byte chunks. `first_control` is the control-key string
    /// attached to the first chunk (e.g. `a=T,f=32,s=W,v=H`).
    fn transmit(&self, out: &mut impl Write, rgba: &[u8], first_control: &str) -> std::io::Result<()> {
        // RGBA tends to be mostly a single color (white background), so zlib compression (o=z) is
        // dramatically effective. s/v (pixel dimensions) stay at the uncompressed size. This greatly
        // reduces the amount transferred over tmux passthrough.
        let payload = STANDARD.encode(zlib_compress(rgba));
        let bytes = payload.as_bytes();
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK).collect();
        let last = chunks.len().saturating_sub(1);
        for (i, chunk) in chunks.iter().enumerate() {
            let more = if i == last { 0 } else { 1 };
            let mut body = Vec::new();
            if i == 0 {
                write!(body, "_G{first_control},o=z,m={more};")?;
            } else {
                write!(body, "_Gm={more};")?;
            }
            body.extend_from_slice(chunk);
            self.write_apc(out, &body)?;
        }
        Ok(())
    }

    /// Direct placement (non-tmux): with double buffering, the new image is overlaid at the same
    /// origin while the old image stays displayed, and the old image is freed only after being
    /// covered. Since we don't clear the whole screen with 2J, there is no blank frame (flicker)
    /// between erasing and the new image arriving.
    fn display_direct(&self, out: &mut impl Write, rgba: &[u8], w: u32, h: u32) -> std::io::Result<()> {
        let new_id = self.next_id();
        let old = *self.live.borrow();
        // Move the cursor back to the origin and overlay the new image at the same size while the
        // old image stays displayed (it covers it, so there is no blank).
        out.write_all(b"\x1b[H")?;
        self.transmit(out, rgba, &format!("a=T,q=2,i={new_id},f=32,s={w},v={h}"))?;
        // The new image now covers it, so free the old image (the terminal always holds 1-2 images).
        if let Some(old_id) = old {
            if old_id != new_id {
                self.delete_id(out, old_id)?;
            }
        }
        *self.live.borrow_mut() = Some(new_id);
        Ok(())
    }

    /// Unicode placeholder placement (tmux): transfers the image as a virtual placement and draws
    /// placeholder cells. The cells are ordinary text, so tmux places them correctly within the pane.
    fn display_placeholder(&self, out: &mut impl Write, rgba: &[u8], w: u32, h: u32) -> std::io::Result<()> {
        let (cols, rows) = cell_footprint(w, h);
        let new_id = self.next_id();
        let old = *self.live.borrow();

        // Only when the cell footprint changes (terminal resize etc.) do we clear the whole screen
        // to sweep away the previous frame's extra cells. Normally the size is the same, so we don't
        // clear and instead overwrite the new cells in place below. Clearing the whole screen with
        // 2J every frame would leave the screen blank between erasing and the new image arriving,
        // causing flicker (transfer is especially slow over SSH+tmux).
        if *self.last_footprint.borrow() != Some((cols, rows)) {
            out.write_all(b"\x1b[2J\x1b[H")?;
        }

        // Transfer the new frame as a virtual placement (U=1) under a different ID than the old
        // frame. The old image stays displayed, so the screen doesn't go blank during transfer.
        // c/r is the image's cell footprint. i is the instance-unique ID.
        self.transmit(
            out,
            rgba,
            &format!("a=T,q=2,U=1,i={new_id},f=32,s={w},v={h},c={cols},r={rows}"),
        )?;

        // Redraw the placeholder cells in the new ID's color (id = r<<16 | g<<8 | b). We overwrite
        // the previous frame's cells in place, so no blank appears (we just replace, not erase).
        let (r, g, b) = ((new_id >> 16) & 0xff, (new_id >> 8) & 0xff, new_id & 0xff);
        write!(out, "\x1b[38;2;{r};{g};{b}m")?;
        let mut buf = [0u8; 4];
        for y in 0..rows {
            // To the start of the line (in tmux this is pane-relative cursor movement).
            write!(out, "\x1b[{};1H", y + 1)?;
            for x in 0..cols {
                out.write_all(PLACEHOLDER.encode_utf8(&mut buf).as_bytes())?;
                out.write_all(diacritic(y).encode_utf8(&mut buf).as_bytes())?;
                out.write_all(diacritic(x).encode_utf8(&mut buf).as_bytes())?;
            }
        }
        out.write_all(b"\x1b[0m")?;

        // Free the old image and its placement (d=I,i=oldID). The new cells no longer reference the
        // old ID, so this doesn't affect the appearance. This keeps the images held on the terminal
        // side at always 1-2, so even repeated redraws (e.g. mashing the caret in copy mode) won't
        // balloon memory and crash the terminal.
        if let Some(old_id) = old {
            if old_id != new_id {
                self.delete_id(out, old_id)?;
            }
        }
        *self.live.borrow_mut() = Some(new_id);
        *self.last_footprint.borrow_mut() = Some((cols, rows));
        Ok(())
    }
}

impl OutputBackend for KittyBackend {
    fn name(&self) -> &str {
        if self.tmux {
            "kitty (tmux placeholder)"
        } else {
            "kitty"
        }
    }

    fn is_supported(&self) -> bool {
        true
    }

    fn enter(&self) -> Result<()> {
        // Switch to the alternate screen + hide the cursor. On exit, the original screen is restored
        // and nothing drawn remains.
        let mut out = std::io::stdout().lock();
        out.write_all(b"\x1b[?1049h\x1b[?25l")?;
        out.flush()?;
        Ok(())
    }

    fn leave(&self) -> Result<()> {
        let mut out = std::io::stdout().lock();
        // Delete our own already-transferred images (both buffers), then show the cursor and leave
        // the alternate screen. We only delete our own IDs, so another vecview's images in another
        // window remain.
        self.delete_own(&mut out)?;
        out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l")?;
        out.flush()?;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        // Clearing the screen alone leaves Kitty images behind, so first delete our own images
        // (both buffers) while preserving other instances'.
        let mut out = std::io::stdout().lock();
        self.delete_own(&mut out)?;
        out.write_all(b"\x1b[2J\x1b[H")?;
        out.flush()?;
        // Since we deleted the images, reset the double-buffer state so the next display performs a
        // clean full-clear + redraw (leaving no leftover cells or references to freed IDs).
        *self.live.borrow_mut() = None;
        *self.last_footprint.borrow_mut() = None;
        Ok(())
    }

    fn display(&self, rgba: &[u8], width: u32, height: u32) -> Result<()> {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if self.tmux {
            self.display_placeholder(&mut out, rgba, width, height)?;
        } else {
            self.display_direct(&mut out, rgba, width, height)?;
        }
        out.flush()?;
        Ok(())
    }
}

/// Computes the number of cells (columns/rows) the image should occupy from the terminal's cell
/// size. Caps it by the size of the diacritics table and the terminal's cell count.
fn cell_footprint(w: u32, h: u32) -> (u32, u32) {
    let (cell_w, cell_h, max_cols, max_rows) = match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 => (
            (ws.width as u32 / ws.columns as u32).max(1),
            (ws.height as u32 / ws.rows as u32).max(1),
            ws.columns as u32,
            ws.rows as u32,
        ),
        Ok(ws) if ws.columns > 0 && ws.rows > 0 => (8, 16, ws.columns as u32, ws.rows as u32),
        _ => (8, 16, 80, 24),
    };
    let cols = (w / cell_w)
        .max(1)
        .min(max_cols)
        .min(DIACRITICS.len() as u32);
    let rows = (h / cell_h)
        .max(1)
        .min(max_rows)
        .min(DIACRITICS.len() as u32);
    (cols, rows)
}

/// Returns the n-th row/column diacritic (out-of-range values are clamped to the last).
fn diacritic(n: u32) -> char {
    let idx = (n as usize).min(DIACRITICS.len() - 1);
    char::from_u32(DIACRITICS[idx]).unwrap_or('\u{0305}')
}

/// Kitty's row/column diacritics table (`gen/rowcolumn-diacritics.txt`). The N-th diacritic
/// represents the value N (row/column number).
const DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];
