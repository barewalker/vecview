//! PDF rendering. Rasterizes the PDF directly at the display resolution via pdfium.
//!
//! Previously the PDF was converted to SVG with pdftocairo and read by usvg, but usvg
//! has a bug that double-applies the placement transform of nested `<use>` elements
//! (template references that pdftocairo emits for legends and the like), which shifts
//! the figure frames. pdfium draws the PDF directly, so it reproduces text, figures,
//! raster images, masks, and gradients correctly, structurally avoiding this problem.
//!
//! The output resolution is supplied on every display (projecting the post-zoom/pan
//! viewport onto out_w×out_h). Vector quality is guaranteed by re-rasterizing at the
//! native resolution every time.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use pdfium_render::prelude::*;

/// Wrapper for placing `Pdfium` into a `static` (which requires Send+Sync).
/// SAFETY: this app uses pdfium only from the main thread (drawing happens on the main
/// loop; the watch/key-input threads never touch pdfium). Since it is never shared
/// across threads, asserting Send/Sync is fine.
struct SyncPdfium(Pdfium);
unsafe impl Sync for SyncPdfium {}
unsafe impl Send for SyncPdfium {}

/// The single pdfium binding held for the entire process.
static PDFIUM: OnceLock<SyncPdfium> = OnceLock::new();

/// Binds pdfium to the process exactly once and returns it.
fn pdfium() -> Result<&'static Pdfium> {
    if let Some(p) = PDFIUM.get() {
        return Ok(&p.0);
    }
    let bindings = bind().context("failed to bind the pdfium library")?;
    // Idempotent even under contention (the first set wins). This app initializes on a single thread at startup.
    let _ = PDFIUM.set(SyncPdfium(Pdfium::new(bindings)));
    Ok(&PDFIUM.get().unwrap().0)
}

/// Locates and binds libpdfium. Priority: the VECVIEW_PDFIUM_LIB env var > default paths > system.
fn bind() -> Result<Box<dyn PdfiumLibraryBindings>> {
    if let Some(p) = std::env::var_os("VECVIEW_PDFIUM_LIB") {
        return Pdfium::bind_to_library(PathBuf::from(&p))
            .map_err(|e| anyhow!("failed to bind VECVIEW_PDFIUM_LIB ({:?}): {e}", p));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join(".local/lib/libpdfium.so"));
    }
    candidates.push(PathBuf::from("/usr/lib/libpdfium.so"));
    candidates.push(PathBuf::from("/usr/local/lib/libpdfium.so"));
    for c in &candidates {
        if c.is_file() {
            if let Ok(b) = Pdfium::bind_to_library(c) {
                return Ok(b);
            }
        }
    }
    Pdfium::bind_to_system_library().map_err(|e| {
        anyhow!(
            "libpdfium not found. Place it at VECVIEW_PDFIUM_LIB or ~/.local/lib/libpdfium.so: {e}"
        )
    })
}

/// Whether pdfium is available (i.e. whether the library can be bound).
pub fn is_available() -> bool {
    pdfium().is_ok()
}

/// A single character in the text layer. Holds the rectangle `rect`=[x, y, w, h] in page
/// coordinates (top-left origin, Y downward, pt) and the character `ch` itself. `page_text`
/// returns these in reading order (pdfium's character index order).
#[derive(Clone, Debug)]
pub struct Glyph {
    pub ch: char,
    pub rect: [f32; 4],
}

/// An opened PDF. Provides page rasterization and retrieval of content bounds.
pub struct Pdf {
    doc: PdfDocument<'static>,
}

impl Pdf {
    /// Opens a PDF file.
    pub fn open(path: &Path) -> Result<Self> {
        let doc = pdfium()?
            .load_pdf_from_file(path, None)
            .with_context(|| format!("cannot open PDF: {}", path.display()))?;
        Ok(Self { doc })
    }

    /// Page count.
    pub fn page_count(&self) -> usize {
        self.doc.pages().len() as usize
    }

    /// Page dimensions (points, width×height).
    pub fn page_size(&self, index: usize) -> Result<(f32, f32)> {
        let page = self.page(index)?;
        Ok((page.width().value, page.height().value))
    }

    /// Rasterizes the viewport rectangle `[x, y, w, h]` (page coordinates, top-left origin,
    /// Y downward, units of pt) of page `index` into `out_w × out_h` pixels and returns RGBA8.
    /// The caller must keep the aspect ratio matched to out (the viewport's w:h = out_w:out_h).
    pub fn render(&self, index: usize, viewport: [f32; 4], out_w: u32, out_h: u32) -> Result<Vec<u8>> {
        let page = self.page(index)?;
        let [vx, vy, vw, _vh] = viewport;
        let s = out_w as f32 / vw.max(1.0); // pixels/point.

        // pdfium's matrix handles the Y flip internally, so a plain scale + translation from
        // page coordinates (top-left origin, Y downward, pt) to the bitmap (top-left origin,
        // Y downward, px) suffices (no Y flip needed).
        //   Dx = s*(Px - vx),  Dy = s*(Py - vy)
        // FS_MATRIX {a,b,c,d,e,f}: Dx = a*Px + c*Py + e, Dy = b*Px + d*Py + f.
        let (a, b, c, d, e, f) = (s, 0.0, 0.0, s, -s * vx, -s * vy);

        let mut bitmap = PdfBitmap::empty(
            out_w as i32,
            out_h as i32,
            PdfBitmapFormat::default(),
            pdfium()?.bindings(),
        )
        .map_err(|e| anyhow!("failed to create bitmap: {e}"))?;

        let config = PdfRenderConfig::new()
            // Match the output size to the bitmap (so the clear rectangle covers the whole surface).
            .set_fixed_size(out_w as i32, out_h as i32)
            // Fill areas outside the page (letterbox) and undrawn regions with opaque white.
            .set_clear_color(PdfColor::new(255, 255, 255, 255))
            .transform(a, b, c, d, e, f)
            .map_err(|e| anyhow!("failed to set matrix: {e}"))?
            .clip(0, 0, out_w as i32, out_h as i32);

        page.render_into_bitmap_with_config(&mut bitmap, &config)
            .map_err(|e| anyhow!("failed to draw PDF: {e}"))?;

        Ok(bitmap.as_rgba_bytes())
    }

    /// Returns the bounding rectangle of the content (all drawing objects) in page coordinates
    /// (top-left origin, Y downward, pt). Used for content fitting (horizontal/vertical).
    /// None if it cannot be obtained.
    pub fn content_bbox(&self, index: usize) -> Option<[f32; 4]> {
        let page = self.page(index).ok()?;
        let ph = page.height().value;
        let group = page.objects().create_group(|_| true).ok()?;
        let b = group.bounds().ok()?;
        let (left, right) = (b.left().value, b.right().value);
        let (top, bottom) = (b.top().value, b.bottom().value); // In PDF, top > bottom (Y upward).
        let (w, h) = (right - left, top - bottom);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        // Convert to a top-left origin (y_top = ph - top).
        Some([left, ph - top, w, h])
    }

    /// Returns the text layer of page `index` in reading order. Each character holds a
    /// rectangle in page coordinates (top-left origin, Y downward, pt). pdfium's
    /// `loose_bounds` (a loose rectangle enclosing the whole glyph) is converted to a
    /// top-left origin via `ph - top`. Characters without a glyph rectangle (control
    /// characters, etc.) are skipped. For Typst text selection, this API reads the
    /// companion PDF whose pt dimensions match the display SVG.
    pub fn page_text(&self, index: usize) -> Result<Vec<Glyph>> {
        let page = self.page(index)?;
        let ph = page.height().value;
        let text = page
            .text()
            .map_err(|e| anyhow!("failed to get text: {e}"))?;
        let chars = text.chars();
        let mut out = Vec::with_capacity(chars.len());
        for c in chars.iter() {
            let Some(ch) = c.unicode_char() else { continue };
            // Newlines, tabs, etc. have no rectangle but matter when concatenating, so keep them with a zero rectangle.
            let rect = match c.loose_bounds() {
                Ok(b) => {
                    let (left, right) = (b.left().value, b.right().value);
                    let (top, bottom) = (b.top().value, b.bottom().value); // In PDF, top > bottom.
                    [left, ph - top, (right - left).max(0.0), (top - bottom).max(0.0)]
                }
                Err(_) => [0.0, 0.0, 0.0, 0.0],
            };
            out.push(Glyph { ch, rect });
        }
        Ok(out)
    }

    fn page(&self, index: usize) -> Result<PdfPage<'_>> {
        let count = self.page_count();
        if index >= count {
            bail!("page out of range: {index} (of {count} pages total)");
        }
        self.doc
            .pages()
            .get(index as u16)
            .map_err(|e| anyhow!("failed to get page: {e}"))
    }
}
