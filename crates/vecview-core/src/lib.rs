//! vecview's format-independent abstract representation.
//!
//! Parsers (SVG/PDF) convert documents into [`Page`]s, the renderer tessellates a
//! [`Page`] and draws it into RGBA, and output backends send that RGBA to the
//! terminal/Framebuffer. To preserve vector quality, paths are kept as curve segments
//! rather than approximated with straight lines.

use anyhow::Result;

/// Abstract representation of a single page of a vector document. The coordinate system has its origin at the top-left with Y pointing down (per SVG).
pub struct Page {
    /// Page width (in user units).
    pub width: f32,
    /// Page height (in user units).
    pub height: f32,
    /// Commands in draw order.
    pub commands: Vec<DrawCommand>,
}

/// A draw command. Either a vector path or an embedded raster image (e.g. a PDF figure).
pub enum DrawCommand {
    Path(PathData),
    Image(ImageData),
}

/// A single embedded raster image. Pastes RGBA8 pixels into the page-coordinate rectangle `rect`.
/// Corresponds to bitmap figures inside a PDF (which pdftocairo emits as SVG `<image>` elements).
pub struct ImageData {
    /// RGBA8 with straight alpha. The length is `px_width * px_height * 4`.
    pub rgba: Vec<u8>,
    /// Pixel width.
    pub px_width: u32,
    /// Pixel height.
    pub px_height: u32,
    /// Placement rectangle in page coordinates [x, y, w, h] (top-left origin). The image is scaled to fit this rectangle.
    pub rect: [f32; 4],
}

/// A path segment. Coordinates are absolute (with the parent transform already applied).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathSegment {
    MoveTo([f32; 2]),
    LineTo([f32; 2]),
    /// Quadratic Bezier with one control point.
    QuadTo([f32; 2], [f32; 2]),
    /// Cubic Bezier with two control points.
    CubicTo([f32; 2], [f32; 2], [f32; 2]),
    Close,
}

/// Fill rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

/// A single vector path. Has a fill and/or a stroke.
pub struct PathData {
    pub segments: Vec<PathSegment>,
    pub fill: Option<Fill>,
    pub stroke: Option<Stroke>,
}

/// Fill specification (only a solid color in the initial scope).
pub struct Fill {
    pub color: Color,
    pub rule: FillRule,
}

/// Stroke specification (only a solid color and width in the initial scope).
pub struct Stroke {
    pub color: Color,
    pub width: f32,
}

/// An RGBA color (each component 0..=255, straight/non-premultiplied).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Normalized `[r, g, b, a]` (0.0..=1.0). Used for shader vertex colors.
    pub fn to_f32(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }
}

/// Trait implemented by format parsers.
pub trait Document {
    fn page_count(&self) -> usize;
    fn render_page(&self, index: usize) -> Result<Page>;
}

/// Trait implemented by output backends. Receives RGBA8 (width x height x 4) and displays it.
pub trait OutputBackend {
    fn name(&self) -> &str;
    fn is_supported(&self) -> bool;
    fn display(&self, rgba: &[u8], width: u32, height: u32) -> Result<()>;

    /// Terminal setup at display start (e.g. switching to the alternate screen). Does nothing by default.
    fn enter(&self) -> Result<()> {
        Ok(())
    }

    /// Cleanup on exit (deleting drawn images, restoring terminal state). Does nothing by default.
    fn leave(&self) -> Result<()> {
        Ok(())
    }

    /// Empties the screen for text display (deletes drawn images, clears the screen, and homes the cursor).
    /// Called before showing a text overlay such as help. By default only clears the terminal screen.
    fn clear(&self) -> Result<()> {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        out.write_all(b"\x1b[2J\x1b[H")?;
        out.flush()?;
        Ok(())
    }

    /// Resends the most recently displayed image. tmux passthrough sixel is not tracked by tmux, so the
    /// image disappears when tmux redraws the pane (status updates, activity in other panes, etc.); this is
    /// used to restore it. Re-emits the cached frame without re-rasterizing. Does nothing by default.
    fn redraw(&self) -> Result<()> {
        Ok(())
    }

    /// Whether the main loop should call [`redraw`](Self::redraw) periodically. Only true for methods where
    /// the image disappears on its own due to external factors (tmux passthrough sixel). Defaults to false.
    fn wants_periodic_redraw(&self) -> bool {
        false
    }
}
