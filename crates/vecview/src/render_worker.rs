//! Background page rasterization.
//!
//! Rasterizing a page costs far more than displaying one — roughly 50 ms against 15 ms for a
//! 1792x1950 PDF page — so doing it on the event loop means every such render is time the loop
//! spends not reading input. Prefetching neighbor pages made that visible: warming a page took
//! longer than drawing one, and a keypress landing mid-warm waited for it. This module moves all
//! rasterization onto its own thread, so the loop only ever hands out requests and takes back
//! finished frames.
//!
//! The thread is also what makes pdfium sound to use here. `vecview-pdf` asserts `Send`/`Sync` on
//! the process-wide pdfium binding on the grounds that a single thread touches it; confining every
//! pdfium call to this one thread is what keeps that assertion true. Nothing outside this module
//! may open or draw a PDF while the worker is running.
//!
//! The worker derives the viewport itself rather than being handed one. That keeps page dimensions
//! — which only the document can answer for — on this side, so the main thread never needs to ask
//! the document anything just to draw.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::{fill_letterbox, fit_zoom_center, viewport_for, Fit, Msg};

/// A document the worker rasterizes pages from.
///
/// Implementations are built on the worker thread and never leave it. Today that is the PDF source;
/// a single-image source (jpg/png, decoded once and scaled per request) fits the same shape — one
/// page, no content box worth computing, no text layer — and so would a multi-frame one, which is
/// why pages are addressed by index rather than assumed singular.
trait PageSource {
    /// Number of addressable pages. 1 for a single image.
    fn page_count(&self) -> usize;

    /// Page dimensions in the source's own units (points for PDF; pixels for a raster image).
    fn page_size(&self, page: usize) -> Result<(f32, f32)>;

    /// Rasterize the viewport rect `[x, y, w, h]` of `page` into `out_w × out_h` RGBA8 pixels.
    fn render(&self, page: usize, viewport: [f32; 4], out_w: u32, out_h: u32) -> Result<Vec<u8>>;

    /// Bounding box of the actual drawn content, for fit-to-content. `None` when the source has
    /// nothing better to offer than the full page.
    fn content_bbox(&self, _page: usize) -> Option<[f32; 4]> {
        None
    }

    /// Glyphs for copy mode, in reading order. Empty for sources with no text layer.
    fn text(&self, _page: usize) -> Result<Vec<vecview_pdf::Glyph>> {
        Ok(Vec::new())
    }

    /// Whether a letterbox needs painting outside the page after rendering. pdfium clears the whole
    /// bitmap to white, so the bands beside a page are indistinguishable from the page itself.
    fn needs_letterbox(&self) -> bool {
        true
    }
}

struct PdfSource {
    doc: vecview_pdf::Pdf,
}

impl PageSource for PdfSource {
    fn page_count(&self) -> usize {
        self.doc.page_count()
    }

    fn page_size(&self, page: usize) -> Result<(f32, f32)> {
        self.doc.page_size(page)
    }

    fn render(&self, page: usize, viewport: [f32; 4], out_w: u32, out_h: u32) -> Result<Vec<u8>> {
        self.doc.render(page, viewport, out_w, out_h)
    }

    fn content_bbox(&self, page: usize) -> Option<[f32; 4]> {
        self.doc.content_bbox(page)
    }

    fn text(&self, page: usize) -> Result<Vec<vecview_pdf::Glyph>> {
        self.doc.page_text(page)
    }
}

/// What to draw. Carries everything the worker needs to derive a viewport, so the main thread can
/// ask for a page without knowing its dimensions.
#[derive(Clone, Copy)]
pub struct ViewRequest {
    pub page: usize,
    pub zoom: u32,
    /// Viewport center in page coordinates. `None` centers the page.
    pub center: Option<(f32, f32)>,
    /// Fit to the content bounding box first, overriding `zoom`/`center`.
    pub fit: Option<Fit>,
    pub out_w: u32,
    pub out_h: u32,
    /// Page mtime at request time. Echoed back so the caller can key its cache without re-reading
    /// the file, and so a frame rendered from a since-replaced document is recognizable.
    pub mtime: SystemTime,
    /// Whether the result may be cached. Decided by the caller, since it turns on view state the
    /// worker doesn't see (a pan in progress, copy mode) as well as on this request's own fields.
    pub cacheable: bool,
}

/// A finished frame.
pub struct RenderedPage {
    pub page: usize,
    pub rgba: Vec<u8>,
    pub out_w: u32,
    pub out_h: u32,
    pub viewport: [f32; 4],
    /// Zoom actually used — a fit request changes it, and the caller adopts this value.
    pub zoom: u32,
    pub mtime: SystemTime,
    /// Echo of [`ViewRequest::cacheable`].
    pub cacheable: bool,
    /// Whether this was requested for immediate display rather than to warm the cache.
    pub for_display: bool,
    /// Request ordinal, so the caller can ignore a display frame that a newer request superseded.
    pub seq: u64,
}

#[derive(Clone, Copy)]
enum Job {
    View { req: ViewRequest, for_display: bool, seq: u64 },
    Text { page: usize },
    /// Reopen the document from disk (the file changed).
    Reopen,
    Quit,
}

/// Handle to the worker thread. Dropping it tells the thread to exit.
pub struct RenderWorker {
    tx: Sender<Job>,
    seq: u64,
}

impl RenderWorker {
    /// Start a worker for `path`, reporting results as [`Msg`] on `out`. The document is opened on
    /// the worker thread; a failure to open arrives as [`Msg::WorkerError`] rather than here, since
    /// the loop already renders that in the status line.
    pub fn spawn(path: PathBuf, out: Sender<Msg>) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("vv-render".into())
            .spawn(move || run(path, rx, out))
            .expect("failed to spawn the render thread");
        Self { tx, seq: 0 }
    }

    /// Request a frame for immediate display. Returns the request ordinal; frames carrying an older
    /// ordinal can be treated as stale.
    pub fn show(&mut self, req: ViewRequest) -> u64 {
        self.seq += 1;
        let _ = self.tx.send(Job::View { req, for_display: true, seq: self.seq });
        self.seq
    }

    /// Request a frame to warm the cache. Never displayed directly, and dropped by the worker
    /// whenever a display request is waiting.
    pub fn prefetch(&self, req: ViewRequest) {
        let _ = self.tx.send(Job::View { req, for_display: false, seq: 0 });
    }

    /// Request a page's text layer (copy mode).
    pub fn text(&self, page: usize) {
        let _ = self.tx.send(Job::Text { page });
    }

    /// Reopen the document after the file changed.
    pub fn reopen(&self) {
        let _ = self.tx.send(Job::Reopen);
    }
}

impl Drop for RenderWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Job::Quit);
    }
}

fn run(path: PathBuf, rx: Receiver<Job>, out: Sender<Msg>) {
    let mut source = match open(&path) {
        Ok(s) => {
            let _ = out.send(Msg::Opened { pages: s.page_count() });
            Some(s)
        }
        Err(e) => {
            let _ = out.send(Msg::WorkerError(format!("{e:#}")));
            None
        }
    };

    // Jobs taken off the channel but not yet acted on.
    //
    // Prefetches wait here while display requests jump ahead of them rather than being discarded:
    // one posted in the same breath as a display request would otherwise be lost for good, and the
    // page it was meant to warm would never warm — the caller has already recorded it as asked for.
    // Exactly one job is carried out per pass, so a display request arriving mid-queue waits for at
    // most the page currently rasterizing.
    let mut queue: Vec<Job> = Vec::new();

    loop {
        if queue.is_empty() {
            match rx.recv() {
                Ok(j) => queue.push(j),
                // The handle is gone, so the session is over.
                Err(_) => return,
            }
        }
        while let Ok(j) = rx.try_recv() {
            queue.push(j);
        }

        if queue.iter().any(|j| matches!(j, Job::Quit)) {
            return;
        }

        if queue.iter().any(|j| matches!(j, Job::Reopen)) {
            // Everything queued refers to the old edition, so none of it survives the reopen.
            queue.clear();
            match open(&path) {
                Ok(s) => {
                    let _ = out.send(Msg::Opened { pages: s.page_count() });
                    source = Some(s);
                }
                Err(e) => {
                    let _ = out.send(Msg::WorkerError(format!("{e:#}")));
                    source = None;
                }
            }
            continue;
        }

        if source.is_none() {
            queue.clear();
            continue;
        }
        let src = source.as_ref().unwrap().as_ref();

        // Newest display request first, and older ones are dropped with it — each is a full redraw
        // of the same view, so an earlier one has nothing left to contribute.
        if let Some(i) = queue
            .iter()
            .rposition(|j| matches!(j, Job::View { for_display: true, .. }))
        {
            let Job::View { req, seq, .. } = queue[i] else {
                unreachable!("just matched a display View")
            };
            queue.retain(|j| !matches!(j, Job::View { for_display: true, .. }));
            emit_view(src, &out, req, true, seq);
            continue;
        }

        // Then the text layer: copy mode is waiting on it, and it costs far less than a page.
        if let Some(i) = queue.iter().position(|j| matches!(j, Job::Text { .. })) {
            let Job::Text { page } = queue.remove(i) else {
                unreachable!("just matched a Text")
            };
            let t0 = std::env::var_os("VECVIEW_TIMING")
                .is_some()
                .then(std::time::Instant::now);
            match src.text(page) {
                Ok(glyphs) => {
                    if let Some(t0) = t0 {
                        eprintln!(
                            "vv-timing worker text page {} = {:.1} ms ({} glyphs)",
                            page + 1,
                            t0.elapsed().as_secs_f64() * 1000.0,
                            glyphs.len()
                        );
                    }
                    let _ = out.send(Msg::Text { page, glyphs });
                }
                Err(e) => {
                    let _ = out.send(Msg::WorkerError(format!("{e:#}")));
                }
            }
            continue;
        }

        // Finally one prefetch, oldest (nearest the page in view when it was posted) first.
        if !queue.is_empty() {
            if let Job::View { req, .. } = queue.remove(0) {
                emit_view(src, &out, req, false, 0);
            }
        }
    }
}

fn emit_view(
    src: &dyn PageSource,
    out: &Sender<Msg>,
    req: ViewRequest,
    for_display: bool,
    seq: u64,
) {
    // VECVIEW_TIMING: rasterization now happens off the event loop, so this is the figure to read
    // for "how long does a page take", separate from what the loop spends transferring it.
    let t0 = std::env::var_os("VECVIEW_TIMING")
        .is_some()
        .then(std::time::Instant::now);
    match render_view(src, req) {
        Ok(mut p) => {
            p.for_display = for_display;
            p.seq = seq;
            if let Some(t0) = t0 {
                eprintln!(
                    "vv-timing worker page {} ({}) = {:.1} ms",
                    req.page + 1,
                    if for_display { "display" } else { "prefetch" },
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
            let _ = out.send(Msg::Rendered(Box::new(p)));
        }
        // A page that won't render is reported once and otherwise ignored; the loop keeps showing
        // whatever is already on screen.
        Err(e) => {
            let _ = out.send(Msg::WorkerError(format!("{e:#}")));
        }
    }
}

fn render_view(src: &dyn PageSource, req: ViewRequest) -> Result<RenderedPage> {
    let (pw, ph) = src.page_size(req.page)?;
    let (pw, ph) = (pw.max(1.0), ph.max(1.0));
    let (out_w, out_h) = (req.out_w, req.out_h);

    // A fit request derives zoom and center from the content box; without one, the caller's zoom
    // and center stand, defaulting to the page center.
    let (zoom, center) = match req.fit.and_then(|f| {
        src.content_bbox(req.page)
            .map(|bbox| fit_zoom_center(f, bbox, pw, ph, out_w, out_h))
    }) {
        Some((z, c)) => (z, c),
        None => (req.zoom, req.center.unwrap_or((pw / 2.0, ph / 2.0))),
    };

    let viewport = viewport_for(pw, ph, out_w, out_h, zoom, center);
    let mut rgba = src.render(req.page, viewport, out_w, out_h)?;
    if src.needs_letterbox() {
        fill_letterbox(&mut rgba, out_w, out_h, viewport, pw, ph);
    }
    Ok(RenderedPage {
        page: req.page,
        rgba,
        out_w,
        out_h,
        viewport,
        zoom,
        mtime: req.mtime,
        cacheable: req.cacheable,
        // Filled in by the caller, which knows which queue this came off.
        for_display: false,
        seq: 0,
    })
}

fn open(path: &Path) -> Result<Box<dyn PageSource>> {
    let doc = vecview_pdf::Pdf::open(path).context("cannot open PDF")?;
    Ok(Box::new(PdfSource { doc }))
}
