//! vecview CLI entry point.
//!
//! `vecview <FILE>` displays SVG / Typst / PDF as vector graphics inside the terminal. For Typst
//! (`.typ`), it launches `typst watch` internally to generate SVG, then watches that SVG and
//! live-redraws. For PDF (`.pdf`), it rasterizes directly at the display resolution with `pdfium`,
//! watches the source PDF, and reopens it on every save. In all cases this gives a no-browser,
//! fully-in-terminal preview.
//!
//! When launched in a terminal (TTY) it runs in interactive mode, with keys for zoom, page
//! navigation, and quitting.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use vecview_core::{Document, DrawCommand, OutputBackend, Page, PathSegment};
use vecview_output::{cell_px, detect_backend};
use vecview_renderer::Renderer;
use vecview_svg::SvgDocument;

/// Message to the redraw loop.
enum Msg {
    /// A watched file changed.
    Reload,
    /// Quit request (Ctrl-C, etc.).
    Quit,
    /// Key input.
    Key(KeyEvent),
    /// Mouse input (for text selection).
    Mouse(MouseEvent),
    /// A line from the typst child's stderr (compile status / errors), shown in our status line
    /// instead of being written straight to the terminal (which corrupts the image in tmux splits).
    TypstMsg(String),
}

/// Display source. SVG/Typst use per-page SVG files; PDF is drawn directly by pdfium.
#[derive(Clone)]
enum Source {
    /// A single SVG file.
    Svg(PathBuf),
    /// Typst (`typst watch` emits `vecview-<stem>-<tag>-<p>.svg`, one file per page).
    /// `tag` is process-specific (PID). Opening the same document in multiple instances avoids
    /// output-path collisions, so instances don't fight over or delete each other's files.
    Typst { dir: PathBuf, stem: String, tag: u32 },
    /// PDF (rasterized directly by pdfium; holds no file, the document is kept on the main side).
    /// Watches the source PDF and reopens it on every save.
    Pdf { pdf: PathBuf },
}

impl Source {
    /// SVG path for page `idx` (0-based) (SVG/Typst only; unused for PDF since it isn't file-based).
    fn page_path(&self, idx: usize) -> PathBuf {
        match self {
            Source::Svg(p) => p.clone(),
            Source::Typst { dir, stem, tag } => {
                dir.join(format!("vecview-{stem}-{tag}-{}.svg", idx + 1))
            }
            Source::Pdf { pdf } => pdf.clone(),
        }
    }

    /// SVG=1, Typst=number of sequential files. PDF uses pdfium's page count, handled in
    /// [`current_page_count`].
    fn page_count(&self) -> usize {
        match self {
            Source::Svg(_) | Source::Pdf { .. } => 1,
            Source::Typst { dir, stem, tag } => typst_page_count(dir, stem, *tag),
        }
    }

    /// The directory to watch. For Typst, the output directory; for PDF/SVG, the directory holding
    /// the source file.
    fn watch_dir(&self) -> PathBuf {
        let base = match self {
            Source::Svg(p) => p.parent().map(Path::to_path_buf),
            Source::Typst { dir, .. } => Some(dir.clone()),
            Source::Pdf { pdf } => pdf.parent().map(Path::to_path_buf),
        };
        base.filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// On exit, clean up the temp files this instance created (Typst only). Because paths include
    /// the PID, files belonging to other instances (including other processes viewing the same
    /// document) are never deleted.
    fn cleanup(&self) {
        if let Source::Typst { dir, stem, tag } = self {
            let prefix = format!("vecview-{stem}-{tag}-");
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    if let Some(n) = entry.file_name().to_str() {
                        if n.starts_with(&prefix) {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }
    }

    /// Whether the changed path is something this source watches (a page file, or the source PDF).
    fn owns(&self, path: &Path) -> bool {
        match self {
            Source::Svg(p) => path == p,
            Source::Pdf { pdf } => path == pdf,
            Source::Typst { dir, stem, tag } => {
                path.parent() == Some(dir.as_path())
                    && path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(|n| {
                            n.starts_with(&format!("vecview-{stem}-{tag}-")) && n.ends_with(".svg")
                        })
                        .unwrap_or(false)
            }
        }
    }
}

/// Typst's current page count = number of sequential page SVGs that exist. We don't decide based
/// on mtime (hitting the brief moment when a page is being written during compilation would
/// misjudge a current page as stale, dropping the page count and making the display vanish — a
/// race). Removal of old pages left behind when the edition shrinks is handled safely after
/// compilation finishes by [`prune_stale_typst_pages`].
fn typst_page_count(dir: &Path, stem: &str, tag: u32) -> usize {
    let mut n = 0;
    while dir
        .join(format!("vecview-{stem}-{tag}-{}.svg", n + 1))
        .exists()
    {
        n += 1;
    }
    n.max(1)
}

/// Delete trailing page SVGs from this instance left behind when the edition shrinks. Since typst
/// writes all current pages within a few dozen ms in a single compilation, the current pages form
/// a dense cluster at the latest mtime. Scanning from the end, only the contiguous run that is
/// clearly older than the latest mtime (beyond `margin`) is treated as leftover and removed. This
/// is meant to be called after compilation finishes (on a debounced Reload), so it won't
/// mistakenly delete a current page that's still being written.
fn prune_stale_typst_pages(dir: &Path, stem: &str, tag: u32) {
    let mut entries: Vec<(PathBuf, SystemTime)> = Vec::new();
    let mut n = 0;
    loop {
        let p = dir.join(format!("vecview-{stem}-{tag}-{}.svg", n + 1));
        match std::fs::metadata(&p).and_then(|m| m.modified()) {
            Ok(t) => {
                entries.push((p, t));
                n += 1;
            }
            Err(_) => break,
        }
    }
    let Some(latest) = entries.iter().map(|(_, t)| *t).max() else {
        return;
    };
    let margin = Duration::from_secs(1);
    // From the end (high page numbers), delete only files older than `latest` by more than
    // `margin`, stopping once we reach the newer cluster (never touching the current pages near
    // the front). Leftovers are always a contiguous run at the end.
    for (path, t) in entries.iter().rev() {
        let stale = latest.duration_since(*t).map(|d| d > margin).unwrap_or(false);
        if !stale {
            break;
        }
        let _ = std::fs::remove_file(path);
    }
}

/// The current page count. For PDF, the value from the open pdfium document; for SVG/Typst,
/// [`Source::page_count`].
fn current_page_count(source: &Source, pdf: Option<&vecview_pdf::Pdf>) -> usize {
    match source {
        Source::Pdf { .. } => pdf.map(|p| p.page_count()).unwrap_or(1),
        other => other.page_count(),
    }
}

#[derive(Parser, Debug)]
#[command(name = "vv", version, about = "vecview - display vector graphics in the terminal")]
struct Args {
    /// File to display (SVG / Typst .typ / PDF).
    file: PathBuf,

    /// Initial zoom factor (%).
    #[arg(short, long, default_value_t = 100)]
    zoom: u32,

    /// Force the output backend [kitty|tmux|sixel|framebuffer]. Can also be set via the
    /// VECVIEW_BACKEND environment variable.
    #[arg(short, long)]
    backend: Option<String>,

    /// Supersampling factor (1..=4). In exchange for sharper tmux display, transfer size grows with
    /// the square of the factor, and under continuous operation the terminal may fail to keep up
    /// with image updates and crash. If unset, the VECVIEW_SCALE environment variable, otherwise 1
    /// (native scale). Use 2 or higher for sharpness if your terminal can handle it.
    #[arg(short, long)]
    scale: Option<u32>,

    /// Headless render mode. Draws one page to PNG without interaction and exits (the foundation
    /// for yazi / nvim integration). Requires `--size`. Output goes to `--output` (default stdout).
    #[arg(long)]
    render: bool,

    /// Output pixel size `WxH` for headless render (e.g. `800x1000`). Required with `--render`.
    #[arg(long)]
    size: Option<String>,

    /// Output destination for headless render. A PNG path, or `-` for stdout. Only valid with
    /// `--render` (default `-`).
    #[arg(short, long)]
    output: Option<String>,

    /// Page to render (1-based). Only valid with `--render` (default 1).
    #[arg(long, default_value_t = 1)]
    page: usize,

    /// Export the document to PDF and exit (Typst / Markdown only). Output goes to `--output`, or
    /// `<file>.pdf` next to the source by default.
    #[arg(long)]
    pdf: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let scale = resolve_scale(args.scale);
    // Output backend: CLI argument > VECVIEW_BACKEND environment variable (auto-detected if unset).
    let backend_choice = args
        .backend
        .clone()
        .or_else(|| std::env::var("VECVIEW_BACKEND").ok());

    // Diagnostic mode: with VECVIEW_PROBE=1, print the sizes the terminal reports and exit (for
    // investigating resolution).
    if std::env::var_os("VECVIEW_PROBE").is_some() {
        probe_and_exit(backend_choice.as_deref(), scale);
    }

    if !args.file.exists() {
        bail!("file not found: {}", args.file.display());
    }

    // Headless render mode (--render): draw one page to PNG with no terminal and no interaction,
    // then exit immediately. The foundation a yazi previewer or nvim plugin uses to "produce a
    // single image at a given size."
    if args.render {
        return render_headless(&args);
    }

    let ext = args
        .file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // PDF export mode (--pdf): compile Typst/Markdown to PDF and exit, no terminal/interaction.
    if args.pdf {
        let out = match args.output.as_deref() {
            Some(o) if o != "-" => PathBuf::from(o),
            _ => args.file.with_extension("pdf"),
        };
        export_to_pdf(&args.file, &ext, &out)?;
        eprintln!("vecview: exported PDF -> {}", out.display());
        return Ok(());
    }

    // PDF is drawn directly with pdfium (the pdftocairo->SVG->usvg path shifts figures due to a
    // double-application-of-transform bug on nested <use>). The opened document is kept here.
    let mut pdf_doc: Option<vecview_pdf::Pdf> = None;

    // Typst launches `typst watch` to generate SVG. SVG is watched as-is.
    let (source, mut child) = match ext.as_str() {
        "typ" => {
            let (source, child) = spawn_typst_watch(&args.file)?;
            (source, Some(child))
        }
        "md" | "markdown" => {
            let (source, child) = spawn_markdown_watch(&args.file)?;
            (source, Some(child))
        }
        "svg" => {
            let canonical = std::fs::canonicalize(&args.file).unwrap_or_else(|_| args.file.clone());
            (Source::Svg(canonical), None)
        }
        "pdf" => {
            let canonical = std::fs::canonicalize(&args.file).unwrap_or_else(|_| args.file.clone());
            pdf_doc = Some(vecview_pdf::Pdf::open(&canonical).context("cannot open PDF")?);
            (Source::Pdf { pdf: canonical }, None)
        }
        other => bail!("unsupported extension: .{other} (only svg / typ / pdf are supported)"),
    };

    let backend = detect_backend(backend_choice.as_deref());
    // The GPU renderer is used only for SVG/Typst vector drawing. PDF is drawn by pdfium, so we
    // don't initialize it.
    let renderer = if matches!(source, Source::Pdf { .. }) {
        None
    } else {
        Some(Renderer::new().context("renderer initialization")?)
    };
    eprintln!(
        "vecview: backend={} | {} | {}",
        backend.name(),
        renderer
            .as_ref()
            .map(|r| format!("GPU={}", r.adapter_info))
            .unwrap_or_else(|| "engine=pdfium".to_string()),
        source.watch_dir().display()
    );
    // Build the key bindings from the config file plus defaults.
    let keymap = Keymap::load();
    let help_key = keymap
        .help
        .iter()
        .find(|(n, _)| *n == "help")
        .and_then(|(_, k)| k.first().cloned())
        .unwrap_or_else(|| "?".to_string());
    eprintln!("controls: press {help_key} for help (keys can be changed in {})", config_path().map(|p| p.display().to_string()).unwrap_or_default());

    // File watching (watch the parent directory NonRecursive so atomic renames aren't missed).
    // Changes other than the target source's page files (temp_dir noise, etc.) are ignored.
    let (tx, rx) = mpsc::channel::<Msg>();
    let watch_tx = tx.clone();
    let owns_source = source.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(200),
        None,
        move |res: DebounceEventResult| {
            if let Ok(events) = res {
                let hit = events
                    .iter()
                    .any(|ev| ev.paths.iter().any(|p| owns_source.owns(p)));
                if hit {
                    let _ = watch_tx.send(Msg::Reload);
                }
            }
        },
    )?;
    debouncer.watch(source.watch_dir(), RecursiveMode::NonRecursive)?;

    let quit_tx = tx.clone();
    ctrlc::set_handler(move || {
        let _ = quit_tx.send(Msg::Quit);
    })
    .context("Ctrl-C handler setup")?;

    // Drain the typst child's stderr on a thread and forward each line as a message, so its compile
    // status/errors appear in our status line rather than being written straight to the terminal
    // (which interleaves with the image and garbles tmux split panes).
    if let Some(stderr) = child.as_mut().and_then(|c| c.stderr.take()) {
        let log_tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr).lines() {
                match line {
                    Ok(l) => {
                        if log_tx.send(Msg::TypstMsg(l)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // If a TTY, run interactively (raw mode + key input thread).
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdout());
    if interactive {
        crossterm::terminal::enable_raw_mode().ok();
        // Detect the terminal cell size now, while we have exclusive stdin in raw mode — it may send
        // a `CSI 16 t` query and read the reply, which must not race the key-input thread below. The
        // result is cached for available_area()/cell_footprint().
        let _ = cell_px();
        // Enable mouse reporting for text selection (disabled on exit).
        crossterm::execute!(std::io::stdout(), EnableMouseCapture).ok();
        let key_tx = tx.clone();
        std::thread::spawn(move || loop {
            match crossterm::event::read() {
                Ok(Event::Key(k)) => {
                    if key_tx.send(Msg::Key(k)).is_err() {
                        break;
                    }
                }
                Ok(Event::Mouse(m)) => {
                    if key_tx.send(Msg::Mouse(m)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        });
    }

    // Switch to the alternate screen (restore terminal state on exit, leaving no drawn images
    // behind).
    backend.enter().ok();

    let mut state = ViewState {
        page: 0,
        zoom: args.zoom.clamp(ZOOM_MIN, ZOOM_MAX),
        center: None,
        last_vw: 0.0,
        last_vh: 0.0,
        scale,
        pending_fit: None,
        help: false,
        copy: None,
        last_viewport: None,
        status: None,
    };
    // The last drawn (page, mtime). Reading the SVG on every draw changes its atime and re-fires
    // notify (self-trigger), so we skip drawing if the page is the same and the mtime is unchanged.
    let mut last_render: Option<(usize, SystemTime)> = None;
    // Base image (before the selection overlay) reused across caret moves in copy mode.
    let mut base_frame: Option<BaseFrame> = None;

    // Initial draw (a .typ may not exist yet since it's still being generated).
    render_current(
        &source,
        pdf_doc.as_ref(),
        &mut state,
        renderer.as_ref(),
        backend.as_ref(),
        &mut last_render,
        &mut base_frame,
    );

    // tmux passthrough sixel gets cleared by tmux, so we time out the input wait and periodically
    // resend the most recent frame to restore it. The interval is VECVIEW_REDRAW_MS (default
    // 1000ms, minimum 100ms).
    let refresh = if backend.wants_periodic_redraw() {
        Some(Duration::from_millis(redraw_interval_ms()))
    } else {
        None
    };

    // Rate-limit image transfer. Streaming a large image to the terminal every frame on a key
    // hold (zoom/pan/caret) lets the producer outpace the consumer (especially terminals over
    // tmux), overflowing the input buffer and crashing the terminal itself. During continuous
    // input, throttle to at most 1/MIN_FRAME, and when input stops draw the final state once
    // (debounce). The interval is VECVIEW_MIN_FRAME_MS (default 80ms ≈ 12fps, minimum 16ms).
    // Over tmux passthrough (placeholder / sixel) transfer is heavy and images pile up on the
    // terminal side, so be more conservative by default. Direct placement is native and light, so
    // a smaller value is fine.
    let min_frame_default = if backend.name().contains("placeholder") || backend.name().contains("tmux") {
        200
    } else {
        80
    };
    let min_frame = Duration::from_millis(min_frame_ms(min_frame_default));
    let mut last_draw: Option<Instant> = None;
    let mut pending_full = false; // A full redraw (GPU re-rasterization) is pending.
    let mut pending_overlay = false; // A lightweight overlay redraw is pending.
    let mut last_sixel = Instant::now(); // Previous time of the periodic sixel redraw.

    // With tmux placeholder kitty, switching to another window leaves the image stuck in the
    // foreground window's pane, because the terminal doesn't clip images per tmux window. Optionally
    // poll whether our own window is visible (via $TMUX_PANE's window_active) and clear the image
    // when hidden / redraw when it returns.
    //
    // This polling is OFF by default: each tick runs `tmux display-message`, and that query makes
    // tmux refresh the client, which forces the terminal (Ghostty) to recomposite the large
    // placeholder image — pinning a CPU core even while idle. Enable it with VECVIEW_VIS_POLL_MS=<ms>
    // only if the lingering-image-on-window-switch bothers you and your terminal tolerates it.
    let kitty_ph = backend.name().contains("placeholder");
    let vis_pane = std::env::var("TMUX_PANE").ok();
    let vis_poll = vis_poll_ms().map(Duration::from_millis);
    let mut last_vis_poll = Instant::now();
    let mut was_visible = true;

    loop {
        // Next input-wait duration: the shortest of the periodic sixel redraw, visibility polling,
        // and the deadline of a pending draw that's being throttled.
        let wait = {
            let mut w = refresh;
            // A pending draw contributes its (throttled) deadline only when we could actually draw
            // it. While our tmux window is hidden, drawing is suppressed (see the draw decision),
            // so the pending flag must NOT drive the wait toward zero — otherwise the deadline is
            // perpetually "now", recv_timeout(0) returns immediately, and the loop spins a full
            // core until the window returns. When hidden, only the visibility poll (below) wakes us.
            let can_attempt_draw = !kitty_ph || was_visible;
            if (pending_full || pending_overlay) && can_attempt_draw {
                let remaining = last_draw
                    .map(|t| min_frame.saturating_sub(Instant::now().duration_since(t)))
                    .unwrap_or(Duration::ZERO);
                w = Some(w.map_or(remaining, |x| x.min(remaining)));
            }
            if let (true, Some(vp)) = (kitty_ph && vis_pane.is_some(), vis_poll) {
                let remaining = vp.saturating_sub(Instant::now().duration_since(last_vis_poll));
                w = Some(w.map_or(remaining, |x| x.min(remaining)));
            }
            w
        };

        // Receive input (with a timeout if there's a wait duration). On timeout, proceed to the
        // draw decision with msgs empty.
        let msgs: Vec<Msg> = match wait {
            Some(d) => match rx.recv_timeout(d) {
                Ok(first) => drain_burst(first, &rx),
                Err(mpsc::RecvTimeoutError::Timeout) => Vec::new(),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(first) => drain_burst(first, &rx),
                Err(_) => break,
            },
        };

        let mut quit = false;
        let mut reload = false;
        let mut dirty = false; // A key action etc. requires a full redraw (GPU re-rasterization) of the normal display.
        let mut overlay_only = false; // Only the copy-mode caret/selection changed = lightweight redraw reusing the base image.
        let mut help_changed = false; // Help visibility was toggled.

        let pages = current_page_count(&source, pdf_doc.as_ref());
        for m in msgs {
            match m {
                Msg::Quit => {
                    quit = true;
                    break;
                }
                Msg::Key(k) => {
                    if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        continue;
                    }
                    // While in copy mode, consume keys there (don't let them collide with normal navigation).
                    if let Some(mut cm) = state.copy.take() {
                        match handle_copy_key(&k, &mut cm) {
                            CopyOutcome::Yank => {
                                let text = cm.selected_text();
                                let n = text.chars().count();
                                copy_to_clipboard(&text);
                                state.status = Some(format!("copied {n} chars"));
                                overlay_only = true; // Don't put cm back = exit. View is unchanged, so lightweight redraw.
                            }
                            CopyOutcome::Exit => overlay_only = true,
                            CopyOutcome::Redraw => {
                                state.copy = Some(cm);
                                overlay_only = true; // Only caret/selection changed = no GPU redraw.
                            }
                            CopyOutcome::Ignore => state.copy = Some(cm),
                        }
                        continue;
                    }
                    match keymap.action(&k) {
                        Some(Action::Quit) => {
                            quit = true;
                            break;
                        }
                        Some(Action::ToggleHelp) => {
                            state.help = !state.help;
                            help_changed = true;
                        }
                        // While help is shown, any key closes it (and that keypress is consumed).
                        _ if state.help => {
                            state.help = false;
                            help_changed = true;
                        }
                        Some(Action::EnterCopyMode) => {
                            match build_text_layer(&source, &args.file, pdf_doc.as_ref(), state.page)
                            {
                                Ok(glyphs) => match CopyMode::new(glyphs) {
                                    Some(cm) => state.copy = Some(cm),
                                    None => {
                                        state.status =
                                            Some("no selectable text on this page".to_string())
                                    }
                                },
                                Err(e) => state.status = Some(format!("text layer error: {e:#}")),
                            }
                            dirty = true;
                        }
                        Some(Action::ExportPdf) => {
                            let out = args.file.with_extension("pdf");
                            match export_to_pdf(&args.file, &ext, &out) {
                                Ok(()) => {
                                    state.status =
                                        Some(format!("exported PDF -> {}", out.display()))
                                }
                                Err(e) => {
                                    state.status = Some(format!("PDF export failed: {e:#}"))
                                }
                            }
                            overlay_only = true;
                        }
                        Some(action) => {
                            apply_action(action, pages, &mut state);
                            dirty = true;
                        }
                        None => {}
                    }
                }
                Msg::Mouse(m) => {
                    match m.kind {
                        // Press: if not in copy mode, build the text layer, enter it, and anchor on the nearest character.
                        MouseEventKind::Down(MouseButton::Left) => {
                            if state.copy.is_none() {
                                if let Ok(glyphs) = build_text_layer(
                                    &source,
                                    &args.file,
                                    pdf_doc.as_ref(),
                                    state.page,
                                ) {
                                    state.copy = CopyMode::new(glyphs);
                                }
                            }
                            if let Some(idx) = mouse_to_glyph(&state, m.column, m.row) {
                                if let Some(cm) = state.copy.as_mut() {
                                    cm.cursor = idx;
                                    cm.anchor = Some(idx);
                                    dirty = true;
                                }
                            }
                        }
                        // Drag: extend the caret (keep the anchor).
                        MouseEventKind::Drag(MouseButton::Left) => {
                            if let Some(idx) = mouse_to_glyph(&state, m.column, m.row) {
                                if let Some(cm) = state.copy.as_mut() {
                                    cm.cursor = idx;
                                    overlay_only = true; // Only the selection changed = lightweight redraw.
                                }
                            }
                        }
                        // Release: if we actually dragged (there's a range), copy and exit.
                        // A single click just places the caret and continues (then you can select with the keyboard).
                        MouseEventKind::Up(MouseButton::Left) => {
                            if let Some(cm) = state.copy.take() {
                                let drag = matches!(cm.anchor, Some(a) if a != cm.cursor);
                                if drag {
                                    let text = cm.selected_text();
                                    let n = text.chars().count();
                                    copy_to_clipboard(&text);
                                    state.status = Some(format!("copied {n} chars"));
                                } else {
                                    state.copy = Some(cm);
                                }
                                overlay_only = true; // View unchanged = lightweight redraw.
                            }
                        }
                        // Wheel: page turn (down = next page, up = previous page). Redraw only when
                        // the page actually changes. In copy mode, glyph coordinates go stale, so exit.
                        MouseEventKind::ScrollDown => {
                            let prev = state.page;
                            apply_action(Action::NextPage, pages, &mut state);
                            if state.page != prev {
                                state.copy = None;
                                dirty = true;
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            let prev = state.page;
                            apply_action(Action::PrevPage, pages, &mut state);
                            if state.page != prev {
                                state.copy = None;
                                dirty = true;
                            }
                        }
                        _ => {}
                    }
                }
                Msg::Reload => reload = true,
                Msg::TypstMsg(line) => {
                    // Surface typst/cmarker output in our status line. Show errors; clear on a clean
                    // compile. Ignore the routine "watching…/writing to…" chatter.
                    let l = line.trim();
                    let low = l.to_ascii_lowercase();
                    if low.contains("error") {
                        let shown: String = l.chars().take(200).collect();
                        state.status = Some(shown);
                        overlay_only = true;
                    } else if low.contains("compiled successfully") {
                        // Clear any previous error message (empty status blanks the line once).
                        state.status = Some(String::new());
                        overlay_only = true;
                    }
                }
            }
        }

        // Quit takes priority over re-conversion/drawing within the burst (exit before heavy work).
        if quit {
            break;
        }

        if reload {
            if let Source::Pdf { pdf } = &source {
                // The source PDF changed, so reopen it (the watched target is the source PDF, and
                // drawing reads from the in-memory document via pdfium, so no self-trigger occurs).
                match vecview_pdf::Pdf::open(pdf) {
                    Ok(doc) => {
                        let pc = doc.page_count();
                        pdf_doc = Some(doc);
                        if state.page >= pc {
                            state.page = pc - 1;
                            state.center = None;
                        }
                        dirty = true;
                    }
                    // Opening a PDF mid-write fails temporarily. Retry on the next Reload.
                    Err(e) => eprintln!("vecview: PDF reopen error: {e:#}"),
                }
            } else {
                // We're past compilation (debounced), so it's safe here to delete old trailing
                // pages left behind when the edition shrank (we're not mid-write, so current pages
                // aren't deleted).
                if let Source::Typst { dir, stem, tag } = &source {
                    prune_stale_typst_pages(dir, stem, *tag);
                }
                // If the edition shrank and the page number exceeds the current count, clamp it
                // (same as on the PDF side).
                let pc = current_page_count(&source, pdf_doc.as_ref());
                if state.page >= pc {
                    state.page = pc - 1;
                    state.center = None;
                }
                // For SVG/Typst, drawing changes the atime and re-fires notify (self-trigger).
                // Skip drawing if the page is unchanged and the mtime is the same.
                let path = source.page_path(state.page);
                let current = mtime_of(&path);
                let unchanged = current.is_none()
                    || (last_render.map(|(p, _)| p) == Some(state.page)
                        && current == last_render.map(|(_, m)| m));
                if !unchanged {
                    dirty = true;
                }
            }
            // If a reload changed the content, copy-mode glyph coordinates go stale, so exit.
            if dirty {
                state.copy = None;
            }
        }

        // Fold the draws needed this iteration into pending flags (actual drawing happens later,
        // rate-limited).
        if dirty {
            pending_full = true;
        }
        if overlay_only {
            pending_overlay = true;
        }
        // Right after closing help, the image needs to be redrawn.
        if help_changed && !state.help {
            pending_full = true;
        }

        // Draw decision. While help is shown, suppress image drawing (to prevent flicker; the
        // latest appears once it's closed), and throttle image drawing by min_frame so we don't
        // outpace the terminal (flood = crash prevention).
        if state.help {
            if help_changed {
                draw_help(backend.as_ref(), &keymap);
            }
            // Don't accumulate while shown (it's re-set via help_changed when closed).
            pending_full = false;
            pending_overlay = false;
        } else if (pending_full || pending_overlay) && (!kitty_ph || was_visible) {
            // Don't draw while our own window is hidden (prevents the image lingering on the
            // foreground window). Keep the pending flags and draw once it becomes visible again.
            let now = Instant::now();
            let can_draw = last_draw.is_none_or(|t| now.duration_since(t) >= min_frame);
            if can_draw {
                if pending_full {
                    render_current(
                        &source,
                        pdf_doc.as_ref(),
                        &mut state,
                        renderer.as_ref(),
                        backend.as_ref(),
                        &mut last_render,
                        &mut base_frame,
                    );
                } else if !redraw_overlay(backend.as_ref(), &state, &base_frame) {
                    // Lightweight copy-mode redraw. If there's no cache, fall back to a full draw.
                    render_current(
                        &source,
                        pdf_doc.as_ref(),
                        &mut state,
                        renderer.as_ref(),
                        backend.as_ref(),
                        &mut last_render,
                        &mut base_frame,
                    );
                }
                // Print the copy-mode status / control hint on the bottom line.
                draw_overlay_text(backend.as_ref(), &mut state);
                last_draw = Some(now);
                pending_full = false;
                pending_overlay = false;
            }
            // If not can_draw, leave it pending. The wait deadline will wake us, so it's drawn next iteration.
        }

        // Periodic sixel redraw (restoring an image cleared by tmux). At the refresh interval,
        // independent of the draw rate.
        if let Some(r) = refresh {
            let now = Instant::now();
            if !state.help && now.duration_since(last_sixel) >= r {
                let _ = backend.redraw();
                last_sixel = now;
            }
        }

        // Visibility polling (countering cross-window lingering of tmux placeholder). Clear the
        // image when hidden, redraw when it returns.
        if let (true, Some(vp)) = (kitty_ph, vis_poll) {
            if let Some(pane) = vis_pane.as_deref() {
                let now = Instant::now();
                if now.duration_since(last_vis_poll) >= vp {
                    last_vis_poll = now;
                    if let Some(visible) = pane_window_active(pane) {
                        if !visible && was_visible {
                            // Switched to another window: clear the transferred image to stop it lingering in the foreground.
                            let _ = backend.clear();
                            was_visible = false;
                        } else if visible && !was_visible {
                            // Returned: redraw to restore the image.
                            was_visible = true;
                            pending_full = true;
                        }
                    }
                }
            }
        }
    }

    // Cleanup: disable mouse capture + leave raw mode + restore the terminal + stop the typst child process.
    if interactive {
        crossterm::execute!(std::io::stdout(), DisableMouseCapture).ok();
        crossterm::terminal::disable_raw_mode().ok();
    }
    backend.leave().ok();
    if let Some(child) = child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Clean up the temp files this instance wrote to /tmp (PID-tagged, so other instances are untouched).
    source.cleanup();

    Ok(())
}

/// Current display state. `zoom` is a % relative to the fit factor. `center` is the viewport
/// center (page coordinates; None means page center). `last_vw`/`vh` are the most recent viewport
/// dimensions (the basis for pan amounts).
struct ViewState {
    page: usize,
    zoom: u32,
    center: Option<(f32, f32)>,
    last_vw: f32,
    last_vh: f32,
    /// Supersampling factor (1..=4). Multiplies the render resolution.
    scale: u32,
    /// Request to fit to the content boundary on the next draw (at draw time, the content bbox is
    /// consulted and reflected into zoom/center).
    pending_fit: Option<Fit>,
    /// Whether help (the shortcut list) is being shown.
    help: bool,
    /// Text selection (copy mode). None means normal display.
    copy: Option<CopyMode>,
    /// The most recently drawn in-page viewport [x, y, w, h]. Used to inverse-map mouse coords to page coords.
    last_viewport: Option<[f32; 4]>,
    /// The most recent copy-result message (printed once on the status line).
    status: Option<String>,
}

/// Text-selection (copy mode) state. `glyphs` is the current page's text layer (reading order),
/// `lines` is the glyph indices per visible line (each line in ascending x), `line_of` maps glyph
/// to line number. `cursor` is the caret position (glyph index), `anchor` is the selection start
/// (None means no selection).
struct CopyMode {
    glyphs: Vec<vecview_pdf::Glyph>,
    lines: Vec<Vec<usize>>,
    line_of: Vec<usize>,
    cursor: usize,
    anchor: Option<usize>,
}

impl CopyMode {
    /// Build a copy mode by grouping glyphs from the text layer into visible lines. None if glyphs
    /// is empty. Control characters (e.g. `\r`/`\n` that pdfium inserts between lines; they have no
    /// rect) are excluded, and line breaks are reconstructed from line geometry ([`selected_text`]
    /// inserts `\n` on a line change). Whitespace is kept since it's needed between words.
    fn new(glyphs: Vec<vecview_pdf::Glyph>) -> Option<Self> {
        let glyphs: Vec<vecview_pdf::Glyph> =
            glyphs.into_iter().filter(|g| !g.ch.is_control()).collect();
        if glyphs.is_empty() {
            return None;
        }
        // In reading order, bundle into lines, treating it as a line break when the y center
        // deviates from the previous line by more than half a character.
        let mut lines: Vec<Vec<usize>> = Vec::new();
        let mut line_of = vec![0usize; glyphs.len()];
        let mut cur_y = f32::NAN;
        for (i, g) in glyphs.iter().enumerate() {
            let cy = g.rect[1] + g.rect[3] / 2.0;
            let h = g.rect[3].max(1.0);
            let new_line = lines.is_empty() || (cy - cur_y).abs() > h * 0.5;
            if new_line {
                lines.push(Vec::new());
                cur_y = cy;
            }
            let li = lines.len() - 1;
            lines[li].push(i);
            line_of[i] = li;
        }
        Some(Self {
            glyphs,
            lines,
            line_of,
            cursor: 0,
            anchor: None,
        })
    }

    /// Glyph center x. Used to preserve the column for j/k.
    fn center_x(&self, idx: usize) -> f32 {
        let r = self.glyphs[idx].rect;
        r[0] + r[2] / 2.0
    }

    /// Move to the line above/below, to the glyph closest to the current x.
    fn move_line(&mut self, delta: i32) {
        let line = self.line_of[self.cursor];
        let target = line as i32 + delta;
        if target < 0 || target as usize >= self.lines.len() {
            return;
        }
        let x = self.center_x(self.cursor);
        let best = self.lines[target as usize]
            .iter()
            .copied()
            .min_by(|&a, &b| {
                (self.center_x(a) - x)
                    .abs()
                    .total_cmp(&(self.center_x(b) - x).abs())
            });
        if let Some(b) = best {
            self.cursor = b;
        }
    }

    /// The first/last glyph of the current line.
    fn line_edge(&mut self, end: bool) {
        let line = &self.lines[self.line_of[self.cursor]];
        if let Some(&i) = if end { line.last() } else { line.first() } {
            self.cursor = i;
        }
    }

    /// The selection range [start, end] (reading order, both ends inclusive). If unselected, the single caret character.
    fn range(&self) -> (usize, usize) {
        match self.anchor {
            Some(a) => (a.min(self.cursor), a.max(self.cursor)),
            None => (self.cursor, self.cursor),
        }
    }

    /// The selected text. Concatenated, inserting a newline where the line changes.
    fn selected_text(&self) -> String {
        let (s, e) = self.range();
        let mut out = String::new();
        let mut prev_line: Option<usize> = None;
        for i in s..=e {
            if let Some(pl) = prev_line {
                if self.line_of[i] != pl {
                    out.push('\n');
                }
            }
            out.push(self.glyphs[i].ch);
            prev_line = Some(self.line_of[i]);
        }
        out
    }
}

/// The fit direction toward the content (ink) boundary.
#[derive(Clone, Copy)]
enum Fit {
    /// Fit the full left-right extent of the content (fit horizontally; vertical may overflow and is reached by panning).
    Width,
    /// Fit the full top-bottom extent of the content (fit vertically).
    Height,
}

/// Key action.
#[derive(Clone, Copy)]
enum Action {
    Quit,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// Move the viewport in the (dx, dy) direction (sign only; the amount is derived from last_vw/vh).
    Pan(f32, f32),
    NextPage,
    PrevPage,
    FirstPage,
    LastPage,
    /// Fit to the content boundary (left-right / top-bottom).
    FitContent(Fit),
    /// Toggle showing/hiding the shortcut list.
    ToggleHelp,
    /// Enter text selection (copy mode).
    EnterCopyMode,
    /// Export the current document to PDF (Typst / Markdown only).
    ExportPdf,
}

/// Zoom factor (%). The minimum is fit (100), the maximum is 16x.
const ZOOM_MIN: u32 = 100;
const ZOOM_MAX: u32 = 1600;

/// The list of configurable actions: (config name, action, default keys). The config name is the
/// key under `[keys]` in config.toml, and the help display order follows this order. Defaults:
/// directional movement when zoomed = arrows, next/previous page = j/k, first/last page = h/l.
const ACTIONS: &[(&str, Action, &[&str])] = &[
    ("zoom_in", Action::ZoomIn, &["+", "="]),
    ("zoom_out", Action::ZoomOut, &["-", "_"]),
    ("zoom_reset", Action::ZoomReset, &["0"]),
    ("fit_width", Action::FitContent(Fit::Width), &["w"]),
    ("fit_height", Action::FitContent(Fit::Height), &["v"]),
    ("pan_left", Action::Pan(-1.0, 0.0), &["left"]),
    ("pan_right", Action::Pan(1.0, 0.0), &["right"]),
    ("pan_up", Action::Pan(0.0, -1.0), &["up"]),
    ("pan_down", Action::Pan(0.0, 1.0), &["down"]),
    ("next_page", Action::NextPage, &["j", "space", "pagedown"]),
    ("prev_page", Action::PrevPage, &["k", "pageup", "backspace"]),
    ("first_page", Action::FirstPage, &["h"]),
    ("last_page", Action::LastPage, &["l"]),
    ("copy_mode", Action::EnterCopyMode, &["y"]),
    ("export_pdf", Action::ExportPdf, &["e"]),
    ("help", Action::ToggleHelp, &["?"]),
    ("quit", Action::Quit, &["q", "esc", "ctrl+c"]),
];

/// Holds the key (code + whether Ctrl) -> action lookup table, plus the effective bindings for the
/// help display.
struct Keymap {
    lookup: std::collections::HashMap<(KeyCode, bool), Action>,
    /// (config name, actual key strings) in ACTIONS order. For the help display and config reference.
    help: Vec<(&'static str, Vec<String>)>,
}

impl Keymap {
    /// Build the keymap from defaults plus config-file overrides.
    fn load() -> Self {
        Self::build(&read_key_overrides())
    }

    /// Build the keymap by overlaying `overrides` (config name -> key strings) onto the defaults.
    fn build(overrides: &std::collections::HashMap<String, Vec<String>>) -> Self {
        let mut lookup = std::collections::HashMap::new();
        let mut help = Vec::new();
        for (name, action, defaults) in ACTIONS {
            let keys: Vec<String> = match overrides.get(*name) {
                Some(v) => v.clone(),
                None => defaults.iter().map(|s| s.to_string()).collect(),
            };
            for spec in &keys {
                match parse_key(spec) {
                    Some(key) => {
                        lookup.insert(key, *action);
                    }
                    None => eprintln!("vecview: unknown key spec {spec:?} ({name})"),
                }
            }
            help.push((*name, keys));
        }
        Keymap { lookup, help }
    }

    /// Return the action corresponding to a key event.
    fn action(&self, k: &KeyEvent) -> Option<Action> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        self.lookup.get(&(k.code, ctrl)).copied()
    }
}

/// Read "config name -> sequence of key strings" from `[keys]` in the config file. Empty (defaults only) if it doesn't exist or fails to parse.
fn read_key_overrides() -> std::collections::HashMap<String, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    let Some(path) = config_path() else {
        return out;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return out;
    };
    let table = match text.parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("vecview: config file parse error ({}): {e}", path.display());
            return out;
        }
    };
    let Some(keys) = table.get("keys").and_then(|v| v.as_table()) else {
        return out;
    };
    for (action, val) in keys {
        let list: Vec<String> = match val {
            toml::Value::String(s) => vec![s.clone()],
            toml::Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => continue,
        };
        out.insert(action.clone(), list);
    }
    out
}

/// The config file path. Precedence: VECVIEW_CONFIG environment variable >
/// $XDG_CONFIG_HOME/vecview/config.toml > ~/.config/vecview/config.toml.
fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VECVIEW_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("vecview").join("config.toml"))
}

/// Parse a key-spec string into (KeyCode, whether Ctrl). E.g. "q", "+", "left", "space", "ctrl+c".
/// Named keys are case-insensitive; single-character keys use the symbol and case as-is.
fn parse_key(spec: &str) -> Option<(KeyCode, bool)> {
    let spec = spec.trim();
    let (ctrl, rest) = match spec.strip_prefix("ctrl+").or_else(|| spec.strip_prefix("Ctrl+")) {
        Some(r) => (true, r),
        None => (false, spec),
    };
    let code = match rest.to_ascii_lowercase().as_str() {
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "space" => KeyCode::Char(' '),
        "tab" => KeyCode::Tab,
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        _ => {
            // Single-character key (symbol or alphanumeric). Use the original case/symbol as-is.
            let mut chars = rest.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    };
    Some((code, ctrl))
}

fn apply_action(action: Action, pages: usize, state: &mut ViewState) {
    match action {
        Action::Quit => {}
        // Multiplicative (about 1.5x each step). Fit (100%) is the minimum; below that only widens the white background, so it's disallowed.
        Action::ZoomIn => state.zoom = (state.zoom * 3 / 2).clamp(ZOOM_MIN, ZOOM_MAX),
        Action::ZoomOut => state.zoom = (state.zoom * 2 / 3).clamp(ZOOM_MIN, ZOOM_MAX),
        Action::ZoomReset => {
            state.zoom = ZOOM_MIN;
            state.center = None;
        }
        Action::Pan(dx, dy) => {
            if let Some((cx, cy)) = state.center {
                let step_x = (state.last_vw * 0.15).max(1.0);
                let step_y = (state.last_vh * 0.15).max(1.0);
                state.center = Some((cx + dx * step_x, cy + dy * step_y));
            }
        }
        Action::NextPage => {
            if state.page + 1 < pages {
                state.page += 1;
                state.center = None;
            }
        }
        Action::PrevPage => {
            if state.page > 0 {
                state.page -= 1;
                state.center = None;
            }
        }
        Action::FirstPage => {
            if state.page != 0 {
                state.page = 0;
                state.center = None;
            }
        }
        Action::LastPage => {
            let last = pages.saturating_sub(1);
            if state.page != last {
                state.page = last;
                state.center = None;
            }
        }
        // The content bbox is only known at draw time (the page must be read), so just raise the
        // request and reflect it into zoom/center at draw time (render_pdf / render_and_display).
        Action::FitContent(fit) => state.pending_fit = Some(fit),
        // Help, copy mode, and PDF export are handled in the main loop (their draw/input paths
        // differ from normal display).
        Action::ToggleHelp => {}
        Action::EnterCopyMode => {}
        Action::ExportPdf => {}
    }
}

/// Generate the shortcut list (help display) from the current keymap. The config names are exactly
/// the keys under `[keys]` in config.toml, so this doubles as a config reference.
fn help_lines(keymap: &Keymap) -> Vec<String> {
    let mut lines = vec![
        "vecview - keyboard shortcuts".to_string(),
        String::new(),
    ];
    for (name, keys) in &keymap.help {
        lines.push(format!("  {name:<12} {}", keys.join(", ")));
    }
    lines.push(String::new());
    lines.push("  mouse:".to_string());
    lines.push("    wheel          next / prev page".to_string());
    lines.push("    drag           select text & copy on release".to_string());
    lines.push(String::new());
    lines.push("  copy mode (text selection):".to_string());
    lines.push("    hjkl / arrows  move caret    0 / $   line start / end".to_string());
    lines.push("    g / G          doc start/end space   start/clear selection".to_string());
    lines.push("    enter or y     copy & exit   esc / q cancel".to_string());
    lines.push(String::new());
    let path = config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/vecview/config.toml".to_string());
    lines.push(format!("  config: {path}"));
    lines.push("    rebind under [keys] with  <action> = [\"key\", ...]".to_string());
    lines.push("  press any key to close".to_string());
    lines
}

/// Draw the help screen. Clear the image and lay out the text in the top-left.
fn draw_help(backend: &dyn OutputBackend, keymap: &Keymap) {
    use std::io::Write;
    let _ = backend.clear();
    let mut out = std::io::stdout().lock();
    for (i, line) in help_lines(keymap).iter().enumerate() {
        // Move to the start of the line (pane-relative) and print. Specify the column explicitly too, since we're in raw mode.
        let _ = write!(out, "\x1b[{};3H{line}", i + 2);
    }
    let _ = out.flush();
}

/// The result of a single keypress in copy mode.
enum CopyOutcome {
    /// Redraw (the cursor/selection moved).
    Redraw,
    /// Copy the selection and exit copy mode.
    Yank,
    /// Exit copy mode without copying.
    Exit,
    /// Do nothing (unassigned key).
    Ignore,
}

/// Key handling in copy mode. Movement is vim/tmux-style (hjkl/arrows, 0/$, g/G), space starts a
/// selection, enter/y yanks, esc/q cancels.
fn handle_copy_key(k: &KeyEvent, cm: &mut CopyMode) -> CopyOutcome {
    // In raw mode, Ctrl-C arrives as a key event. Exit so we don't trap it inside copy mode.
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return CopyOutcome::Exit;
    }
    let last = cm.glyphs.len() - 1;
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => CopyOutcome::Exit,
        KeyCode::Enter | KeyCode::Char('y') => CopyOutcome::Yank,
        // Toggle starting/clearing the selection.
        KeyCode::Char(' ') | KeyCode::Char('v') => {
            cm.anchor = match cm.anchor {
                Some(_) => None,
                None => Some(cm.cursor),
            };
            CopyOutcome::Redraw
        }
        KeyCode::Char('h') | KeyCode::Left => {
            cm.cursor = cm.cursor.saturating_sub(1);
            CopyOutcome::Redraw
        }
        KeyCode::Char('l') | KeyCode::Right => {
            cm.cursor = (cm.cursor + 1).min(last);
            CopyOutcome::Redraw
        }
        KeyCode::Char('j') | KeyCode::Down => {
            cm.move_line(1);
            CopyOutcome::Redraw
        }
        KeyCode::Char('k') | KeyCode::Up => {
            cm.move_line(-1);
            CopyOutcome::Redraw
        }
        KeyCode::Char('0') | KeyCode::Home => {
            cm.line_edge(false);
            CopyOutcome::Redraw
        }
        KeyCode::Char('$') | KeyCode::End => {
            cm.line_edge(true);
            CopyOutcome::Redraw
        }
        KeyCode::Char('g') => {
            cm.cursor = 0;
            CopyOutcome::Redraw
        }
        KeyCode::Char('G') => {
            cm.cursor = last;
            CopyOutcome::Redraw
        }
        _ => CopyOutcome::Ignore,
    }
}

/// Build the text layer (glyphs in reading order) when entering copy mode. For PDF, the open
/// document; for Typst, temporarily generate a PDF whose pt dimensions match the display SVG and
/// read that. A standalone SVG has no text layer.
fn build_text_layer(
    source: &Source,
    typ_path: &Path,
    pdf: Option<&vecview_pdf::Pdf>,
    page: usize,
) -> Result<Vec<vecview_pdf::Glyph>> {
    match source {
        Source::Pdf { .. } => {
            let doc = pdf.ok_or_else(|| anyhow!("PDF is not open"))?;
            doc.page_text(page)
        }
        Source::Typst { dir, stem, tag } => {
            // Typst SVG outlines glyphs into paths and carries no characters, so compile the same
            // .typ to PDF as well and use its characters + coordinates (pt dimensions match the
            // SVG). PID-tagged to avoid collisions with other instances.
            let pdf_path = dir.join(format!("vecview-{stem}-{tag}-text.pdf"));
            let ok = Command::new("typst")
                .arg("compile")
                .arg(typ_path)
                .arg(&pdf_path)
                .arg("--format")
                .arg("pdf")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("failed to launch typst compile (text PDF)")?
                .success();
            if !ok {
                bail!("typst compile (text PDF) failed");
            }
            let doc = vecview_pdf::Pdf::open(&pdf_path).context("cannot open text PDF")?;
            let mut glyphs = doc.page_text(page)?;
            // Display uses SVG (usvg), and usvg normalizes the SVG's pt specifications to px at
            // 96dpi, so the display and viewport are in "pt × 96/72" units. The glyph rects,
            // however, are still in PDF pt. Unless both are scaled to the same unit by their page
            // dimension ratio, the selection highlight drifts further off the lower/right it goes.
            // Take the ratio directly from the SVG dimensions the renderer actually uses and the
            // PDF pt dimensions (independent of usvg's DPI).
            if let Ok((pdf_w, pdf_h)) = doc.page_size(page) {
                let svg_dims = source
                    .page_path(page)
                    .to_str()
                    .and_then(|p| SvgDocument::open(p).ok())
                    .and_then(|d| d.render_page(0).ok())
                    .map(|pg| (pg.width, pg.height));
                if let Some((sw, sh)) = svg_dims {
                    if pdf_w > 0.0 && pdf_h > 0.0 {
                        let (rx, ry) = (sw / pdf_w, sh / pdf_h);
                        for g in &mut glyphs {
                            g.rect[0] *= rx;
                            g.rect[1] *= ry;
                            g.rect[2] *= rx;
                            g.rect[3] *= ry;
                        }
                    }
                }
            }
            Ok(glyphs)
        }
        // A standalone SVG (including Typst-derived ones) often has no <text>, so no text layer can be built.
        Source::Svg(_) => Ok(Vec::new()),
    }
}

/// Convert a mouse cell coordinate (col,row) to the index of the nearest glyph. Estimate the page
/// coordinate from the most recent viewport and the terminal cell count, and return the glyph with
/// the smallest center distance.
fn mouse_to_glyph(state: &ViewState, col: u16, row: u16) -> Option<usize> {
    let cm = state.copy.as_ref()?;
    let [vx, vy, vw, vh] = state.last_viewport?;
    let (cols, rows) = crossterm::terminal::size().ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    // Cell-center fraction -> page coordinate. Assumes the image covers the whole pane (cols×rows).
    let px = vx + (col as f32 + 0.5) / cols as f32 * vw;
    let py = vy + (row as f32 + 0.5) / rows as f32 * vh;
    cm.glyphs
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| glyph_dist2(a.rect, px, py).total_cmp(&glyph_dist2(b.rect, px, py)))
        .map(|(i, _)| i)
}

/// The squared distance from point (px,py) to the center of a glyph rect.
fn glyph_dist2(r: [f32; 4], px: f32, py: f32) -> f32 {
    let cx = r[0] + r[2] / 2.0;
    let cy = r[1] + r[3] / 2.0;
    (cx - px) * (cx - px) + (cy - py) * (cy - py)
}

/// Send the selected text to the clipboard via OSC 52. Inside tmux, wrap it in passthrough.
/// Independent of X11/Wayland, and lands in the host-side clipboard even over SSH.
fn copy_to_clipboard(text: &str) {
    use base64::Engine;
    use std::io::Write;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{b64}\x07");
    let mut out = std::io::stdout().lock();
    if std::env::var_os("TMUX").is_some() {
        // tmux passthrough: double the inner ESC and wrap in \ePtmux;...\e\\.
        let inner = seq.replace('\x1b', "\x1b\x1b");
        let _ = write!(out, "\x1bPtmux;{inner}\x1b\\");
    } else {
        let _ = write!(out, "{seq}");
    }
    let _ = out.flush();
}

/// Blend the selection highlight and caret directly into the RGBA image. Page coords -> output
/// pixels are obtained by the inverse transform of the viewport.
fn overlay_selection(rgba: &mut [u8], out_w: u32, out_h: u32, viewport: [f32; 4], cm: &CopyMode) {
    let [vx, vy, vw, vh] = viewport;
    let sx = out_w as f32 / vw.max(1.0);
    let sy = out_h as f32 / vh.max(1.0);
    let selecting = cm.anchor.is_some();
    let (s, e) = cm.range();
    if selecting {
        for i in s..=e {
            let r = cm.glyphs[i].rect;
            if r[2] <= 0.0 || r[3] <= 0.0 {
                continue; // A character with no rect, such as a line break.
            }
            let x0 = (r[0] - vx) * sx;
            let y0 = (r[1] - vy) * sy;
            let x1 = (r[0] + r[2] - vx) * sx;
            let y1 = (r[1] + r[3] - vy) * sy;
            blend_rect(rgba, out_w, out_h, [x0, y0, x1, y1], [40, 120, 255], 0.38);
        }
    }
    // Caret (vertical line).
    let cr = cm.glyphs[cm.cursor].rect;
    let cx = (cr[0] - vx) * sx;
    let cy0 = (cr[1] - vy) * sy;
    let ch = cr[3].max(8.0) * sy;
    let cw = (sx * 1.5).max(2.0);
    blend_rect(rgba, out_w, out_h, [cx, cy0, cx + cw, cy0 + ch], [255, 40, 40], 0.9);
}

/// Alpha-blend `color` into the rect [x0,y0,x1,y1] (output pixels) with coefficient `a`.
fn blend_rect(rgba: &mut [u8], w: u32, h: u32, rect: [f32; 4], color: [u8; 3], a: f32) {
    let [x0, y0, x1, y1] = rect;
    let xa = x0.min(x1).floor().max(0.0) as u32;
    let xb = (x0.max(x1).ceil() as i64).clamp(0, w as i64) as u32;
    let ya = y0.min(y1).floor().max(0.0) as u32;
    let yb = (y0.max(y1).ceil() as i64).clamp(0, h as i64) as u32;
    for y in ya..yb {
        for x in xa..xb {
            let idx = ((y * w + x) * 4) as usize;
            if idx + 3 >= rgba.len() {
                continue;
            }
            for c in 0..3 {
                let bg = rgba[idx + c] as f32;
                let fg = color[c] as f32;
                rgba[idx + c] = (bg * (1.0 - a) + fg * a).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// Print the copy-mode status (the most recent copy result) or a control hint on the bottom line.
fn draw_overlay_text(backend: &dyn OutputBackend, state: &mut ViewState) {
    use std::io::Write;
    let _ = backend;
    let (_, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut out = std::io::stdout().lock();
    if let Some(msg) = state.status.take() {
        let _ = write!(out, "\x1b[{rows};1H\x1b[2K{msg}");
        let _ = out.flush();
    } else if state.copy.is_some() {
        let _ = write!(
            out,
            "\x1b[{rows};1H\x1b[2K-- COPY: hjkl/arrows move, space select, enter yank, esc cancel --"
        );
        let _ = out.flush();
    }
}

/// Base-image cache (before the selection overlay) used to avoid GPU re-rasterization on caret
/// moves in copy mode. Reused only while in copy mode, where all keys are consumed by copy mode
/// and the view doesn't change. Redrawing a complex figure on every keypress would hammer the GPU
/// and could take down the terminal with it.
struct BaseFrame {
    out_w: u32,
    out_h: u32,
    viewport: [f32; 4],
    rgba: Vec<u8>,
}

/// Lightweight redraw when only the copy-mode caret/selection changed. Reuse the most recent base
/// image and overlay just the selection (no GPU redraw, no SVG reload). Returns false if there's
/// no cache, and the caller falls back to a full draw.
fn redraw_overlay(backend: &dyn OutputBackend, state: &ViewState, base: &Option<BaseFrame>) -> bool {
    let Some(bf) = base else { return false };
    let mut rgba = bf.rgba.clone();
    if let Some(cm) = &state.copy {
        overlay_selection(&mut rgba, bf.out_w, bf.out_h, bf.viewport, cm);
    }
    backend.display(&rgba, bf.out_w, bf.out_h).is_ok()
}

/// Render and display the current page. PDF via pdfium, SVG/Typst via the GPU renderer.
/// On failure (e.g. the file isn't generated yet), silently skip.
fn render_current(
    source: &Source,
    pdf: Option<&vecview_pdf::Pdf>,
    state: &mut ViewState,
    renderer: Option<&Renderer>,
    backend: &dyn OutputBackend,
    last_render: &mut Option<(usize, SystemTime)>,
    base: &mut Option<BaseFrame>,
) {
    match source {
        Source::Pdf { .. } => {
            let Some(doc) = pdf else { return };
            if let Err(e) = render_pdf(doc, backend, state, base) {
                eprintln!("vecview: render error: {e:#}");
            }
        }
        _ => {
            let path = source.page_path(state.page);
            if !path.exists() {
                return;
            }
            let Some(renderer) = renderer else { return };
            match render_and_display(&path, renderer, backend, state, base) {
                Ok(()) => *last_render = mtime_of(&path).map(|m| (state.page, m)),
                Err(e) => eprintln!("vecview: render error: {e:#}"),
            }
        }
    }
}

/// Have pdfium rasterize the PDF's current page at the zoom/pan-state viewport and display it.
fn render_pdf(
    pdf: &vecview_pdf::Pdf,
    backend: &dyn OutputBackend,
    state: &mut ViewState,
    base: &mut Option<BaseFrame>,
) -> Result<()> {
    let (pw, ph) = pdf.page_size(state.page)?;
    let (pw, ph) = (pw.max(1.0), ph.max(1.0));

    // The output is always the pane (display area) size. Zoom is expressed by the size of the
    // viewport rect.
    let (out_w, out_h) = available_area(backend.name(), state.scale);

    // If there's a content-fit request, compute zoom/center from the content boundary and delegate
    // to the normal viewport computation.
    if let Some(fit) = state.pending_fit.take() {
        if let Some(bbox) = pdf.content_bbox(state.page) {
            apply_fit(fit, bbox, pw, ph, out_w, out_h, state);
        }
    }

    let center = state.center.unwrap_or((pw / 2.0, ph / 2.0));
    let viewport = viewport_for(pw, ph, out_w, out_h, state.zoom, center);
    state.last_vw = viewport[2];
    state.last_vh = viewport[3];
    state.center = Some((viewport[0] + viewport[2] / 2.0, viewport[1] + viewport[3] / 2.0));
    state.last_viewport = Some(viewport);

    let mut rgba = pdf.render(state.page, viewport, out_w, out_h)?;
    // pdfium fills the entire bitmap (including the letterbox outside the page) with the white
    // clear_color, so fitting a tall page into a wide pane leaves white bands on the sides that are
    // indistinguishable from the page's white and look like "extra left-right margin." Repaint
    // outside the page with the same dark color as the SVG/Typst renderer to make the page
    // boundary visible.
    fill_letterbox(&mut rgba, out_w, out_h, viewport, pw, ph);
    // While in copy mode, cache the base before applying the overlay (reused across subsequent caret moves).
    if state.copy.is_some() {
        *base = Some(BaseFrame { out_w, out_h, viewport, rgba: rgba.clone() });
    }
    if let Some(cm) = &state.copy {
        overlay_selection(&mut rgba, out_w, out_h, viewport, cm);
    }
    backend.display(&rgba, out_w, out_h)?;
    Ok(())
}

/// Fill outside the page (the letterbox) with a dark color. Because pdfium paints even outside the
/// page with the white clear_color, match the SVG renderer (`LETTERBOX` = 0.10 gray equivalent) to
/// make the page boundary visible. Forward-transform the page rect [0,0,pw,ph] (pt) from viewport
/// to output pixels and repaint only outside it. When zoomed in (the page covers the output), the
/// rect contains the whole output, so this does nothing.
fn fill_letterbox(rgba: &mut [u8], out_w: u32, out_h: u32, viewport: [f32; 4], pw: f32, ph: f32) {
    let [vx, vy, vw, vh] = viewport;
    let sx = out_w as f32 / vw.max(1.0);
    let sy = out_h as f32 / vh.max(1.0);
    let x0 = ((0.0 - vx) * sx).round();
    let y0 = ((0.0 - vy) * sy).round();
    let x1 = ((pw - vx) * sx).round();
    let y1 = ((ph - vy) * sy).round();
    const C: u8 = 26; // LETTERBOX(0.10) to 8-bit (0.10*255 ≈ 26).
    for y in 0..out_h {
        let inside_y = (y as f32) >= y0 && (y as f32) < y1;
        for x in 0..out_w {
            if inside_y && (x as f32) >= x0 && (x as f32) < x1 {
                continue; // Inside the page: leave as-is.
            }
            let idx = ((y * out_w + x) * 4) as usize;
            rgba[idx] = C;
            rgba[idx + 1] = C;
            rgba[idx + 2] = C;
            rgba[idx + 3] = 255;
        }
    }
}

/// Headless render (`--render`): draw one page to RGBA with no terminal and no interaction, and
/// output it as PNG. The render path is the same as interactive mode (PDF=pdfium, SVG/Typst=wgpu),
/// and the output is the actual pixels of `--size`.
fn render_headless(args: &Args) -> Result<()> {
    let (out_w, out_h) = match args.size.as_deref() {
        Some(s) => parse_size(s)
            .ok_or_else(|| anyhow!("invalid --size format (e.g. 800x1000): {s}"))?,
        None => bail!("--render requires --size (e.g. --size 800x1000)"),
    };
    let output = args.output.as_deref().unwrap_or("-");
    let zoom = args.zoom.clamp(ZOOM_MIN, ZOOM_MAX);
    let page_idx = args.page.saturating_sub(1);

    let ext = args
        .file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let rgba = match ext.as_str() {
        "pdf" => render_pdf_headless(&args.file, page_idx, out_w, out_h, zoom)?,
        "svg" => {
            let renderer = Renderer::new().context("renderer initialization")?;
            render_svg_file(&args.file, &renderer, out_w, out_h, zoom)?
        }
        "typ" => render_typ_headless(&args.file, page_idx, out_w, out_h, zoom)?,
        "md" | "markdown" => render_md_headless(&args.file, page_idx, out_w, out_h, zoom)?,
        other => bail!(
            "unsupported extension: .{other} (only svg / typ / md / markdown / pdf are supported)"
        ),
    };
    write_png(&rgba, out_w, out_h, output)
}

/// Headlessly rasterize one page of a PDF at the viewport (page center, given zoom).
fn render_pdf_headless(file: &Path, page: usize, out_w: u32, out_h: u32, zoom: u32) -> Result<Vec<u8>> {
    let pdf = vecview_pdf::Pdf::open(file).context("cannot open PDF")?;
    let pages = pdf.page_count();
    if page >= pages {
        bail!("page out of range: {} (of {} total pages)", page + 1, pages);
    }
    let (pw, ph) = pdf.page_size(page)?;
    let (pw, ph) = (pw.max(1.0), ph.max(1.0));
    let viewport = viewport_for(pw, ph, out_w, out_h, zoom, (pw / 2.0, ph / 2.0));
    let mut rgba = pdf.render(page, viewport, out_w, out_h)?;
    fill_letterbox(&mut rgba, out_w, out_h, viewport, pw, ph);
    Ok(rgba)
}

/// Headlessly run a one-shot Typst compile (`typst compile`) to generate SVG and draw the
/// requested page. The output is created in the temp directory as `vv-render-<stem>-<pid>-{p}.svg`
/// and cleaned up after drawing.
fn render_typ_headless(file: &Path, page: usize, out_w: u32, out_h: u32, zoom: u32) -> Result<Vec<u8>> {
    if which_typst().is_none() {
        bail!("typst is not on PATH. Typst rendering requires typst.");
    }
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("vv");
    compile_typst_headless(file, None, stem, page, out_w, out_h, zoom)
}

/// Headlessly render one page of a Markdown file: generate a temp typst wrapper that renders it via
/// the cmarker package, compile it, and draw the page. Same cleanup as the typst path.
fn render_md_headless(file: &Path, page: usize, out_w: u32, out_h: u32, zoom: u32) -> Result<Vec<u8>> {
    if which_typst().is_none() {
        bail!("typst is not on PATH. Markdown rendering uses typst + the cmarker package.");
    }
    let dir = std::env::temp_dir();
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("vv");
    let tag = std::process::id();
    // Named with the same prefix the compile cleanup sweeps, so the wrapper is removed too.
    let wrapper = dir.join(format!("vv-render-{stem}-{tag}-main.typ"));
    std::fs::write(&wrapper, cmarker_wrapper_src(file)?)
        .context("failed to write markdown wrapper")?;
    // --root "/" so typst may read the .md (and absolute-path images) outside the temp dir.
    compile_typst_headless(&wrapper, Some("/"), stem, page, out_w, out_h, zoom)
}

/// Shared by [`render_typ_headless`] / [`render_md_headless`]: compile `main` with `typst compile`
/// (optional `--root`), then rasterize page `page` of the emitted SVGs. Temp SVGs (and any temp
/// `vv-render-<stem>-<pid>-*` files such as a markdown wrapper) are cleaned up afterward.
fn compile_typst_headless(
    main: &Path,
    root: Option<&str>,
    stem: &str,
    page: usize,
    out_w: u32,
    out_h: u32,
    zoom: u32,
) -> Result<Vec<u8>> {
    let dir = std::env::temp_dir();
    let tag = std::process::id();
    let template = dir.join(format!("vv-render-{stem}-{tag}-{{p}}.svg"));
    let mut cmd = Command::new("typst");
    cmd.arg("compile");
    if let Some(r) = root {
        cmd.arg("--root").arg(r);
    }
    let ok = cmd
        .arg(main)
        .arg(&template)
        .arg("--format")
        .arg("svg")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit()) // Show compile errors.
        .status()
        .context("failed to launch typst compile")?
        .success();
    // Delete the temp files we emitted (all page SVGs plus any wrapper).
    let cleanup = || {
        let prefix = format!("vv-render-{stem}-{tag}-");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix)) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    };
    if !ok {
        cleanup();
        bail!("typst compile failed");
    }
    let page_svg = dir.join(format!("vv-render-{stem}-{tag}-{}.svg", page + 1));
    if !page_svg.exists() {
        cleanup();
        bail!("page out of range, or SVG was not generated: page {}", page + 1);
    }
    let renderer = Renderer::new().context("renderer initialization")?;
    let res = render_svg_file(&page_svg, &renderer, out_w, out_h, zoom);
    cleanup();
    res
}

/// Build the source of a temp typst file that renders a Markdown file via the cmarker package.
/// `typst watch`/`compile` tracks the `read()` dependency, so editing the `.md` triggers a rebuild.
fn cmarker_wrapper_src(md: &Path) -> Result<String> {
    let md_abs = std::fs::canonicalize(md).unwrap_or_else(|_| md.to_path_buf());
    // Escape the path for a typst string literal.
    let esc = md_abs
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    // Page geometry. Default to a paged size (A4) so a long document splits into pages that vv's
    // page navigation (j/k, PageUp/Down) can flip through, instead of one giant scroll-only page.
    // VECVIEW_MD_PAGE overrides: "auto" = single continuous page; otherwise a typst paper name
    // (e.g. "a4", "us-letter").
    let page_set = match std::env::var("VECVIEW_MD_PAGE").ok().map(|s| s.trim().to_ascii_lowercase())
    {
        Some(s) if s == "auto" => "#set page(width: 21cm, height: auto, margin: 1.5cm)".to_string(),
        Some(s) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => {
            format!("#set page(paper: \"{s}\", margin: 1.5cm)")
        }
        _ => "#set page(paper: \"a4\", margin: 1.5cm)".to_string(),
    };
    Ok(format!(
        "#import \"@preview/cmarker:0.1.9\"\n\
         {page_set}\n\
         #set text(size: 11pt)\n\
         #cmarker.render(read(\"{esc}\"))\n"
    ))
}

/// Open an SVG file and draw it to RGBA at the page-center, given-zoom viewport (shared by headless).
fn render_svg_file(path: &Path, renderer: &Renderer, out_w: u32, out_h: u32, zoom: u32) -> Result<Vec<u8>> {
    let doc = SvgDocument::open(
        path.to_str()
            .ok_or_else(|| anyhow!("path is not UTF-8"))?,
    )?;
    let page = doc.render_page(0)?;
    let pw = page.width.max(1.0);
    let ph = page.height.max(1.0);
    let viewport = viewport_for(pw, ph, out_w, out_h, zoom, (pw / 2.0, ph / 2.0));
    renderer.render(&page, out_w, out_h, viewport)
}

/// Write RGBA8 (out_w×out_h) as PNG to `output`. If `output` is `-`, write to stdout.
fn write_png(rgba: &[u8], w: u32, h: u32, output: &str) -> Result<()> {
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder};
    use std::io::Write;
    let encode = |writer: &mut dyn std::io::Write| -> Result<()> {
        PngEncoder::new(writer)
            .write_image(rgba, w, h, ExtendedColorType::Rgba8)
            .map_err(|e| anyhow!("PNG encode failed: {e}"))
    };
    if output == "-" {
        let mut out = std::io::stdout().lock();
        encode(&mut out)?;
        out.flush()?;
    } else {
        let f = std::fs::File::create(output)
            .with_context(|| format!("cannot create output file: {output}"))?;
        let mut w = std::io::BufWriter::new(f);
        encode(&mut w)?;
        w.flush()?;
    }
    Ok(())
}

/// Parse `"WxH"` (separator `x`/`X`) into (width, height) pixels. 0 is disallowed; clamp the upper bound to 16384.
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w.min(16384), h.min(16384)))
}

/// Among `vecview-<stem>-<pid>-…` files in `dir`, delete those whose `<pid>` process is no longer
/// alive (i.e. files left behind without cleanup after a past crash, etc.). Files of other running
/// instances are kept since their PID is alive. Process-liveness uses Linux's /proc; on other OSes
/// it does nothing (leaving it to `/tmp` being cleared on reboot).
fn sweep_dead_typst_pages(dir: &Path, stem: &str) {
    let prefix = format!("vecview-{stem}-");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Strip "vecview-<stem>-" and extract the following digit string (the PID).
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let pid_str: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if !process_alive(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether the PID's process is alive. On Linux, judge by the presence of `/proc/<pid>`. On other
/// OSes, always return true (the safe side = don't delete) and perform no cleanup.
fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

/// Launch `typst watch <file> <tmp>-{p}.svg` and return (Source, child process).
fn spawn_typst_watch(typ: &Path) -> Result<(Source, Child)> {
    if which_typst().is_none() {
        bail!("typst is not on PATH. Typst preview requires typst.");
    }
    let stem = typ
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("vecview")
        .to_string();
    let dir = std::env::temp_dir();
    // The output path is process-specific (PID). Files don't collide even when the same document
    // is opened in multiple instances.
    let tag = std::process::id();
    // Clean up only leftovers from same-named documents whose process is dead (left behind after a
    // past crash, etc.). Files of other running instances are kept since their PID is alive. This
    // runs before watching starts, so no notification fires.
    sweep_dead_typst_pages(&dir, &stem);
    // Use the page-number template {p} so typst doesn't error on multi-page documents.
    let template = dir.join(format!("vecview-{stem}-{tag}-{{p}}.svg"));

    let child = Command::new("typst")
        .arg("watch")
        .arg(typ)
        .arg(&template)
        .arg("--format")
        .arg("svg")
        .stdout(Stdio::null())
        .stderr(Stdio::piped()) // Captured and surfaced in vv's status line (see the reader thread).
        .spawn()
        .context("failed to launch typst watch")?;

    Ok((Source::Typst { dir, stem, tag }, child))
}

/// Launch a live Markdown preview. Writes a temp typst wrapper that renders the `.md` via the
/// cmarker package, then runs `typst watch` on it. typst tracks the `read()` dependency, so editing
/// the `.md` recompiles and updates the preview, reusing the whole Typst pipeline (`Source::Typst`).
fn spawn_markdown_watch(md: &Path) -> Result<(Source, Child)> {
    if which_typst().is_none() {
        bail!("typst is not on PATH. Markdown preview renders via typst + the cmarker package.");
    }
    let stem = md
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("vecview")
        .to_string();
    let dir = std::env::temp_dir();
    let tag = std::process::id();
    sweep_dead_typst_pages(&dir, &stem);
    // The wrapper shares the page-file prefix so Source::cleanup() removes it on exit too. It is a
    // `.typ`, so owns() (which requires `.svg`) won't mistake it for a page.
    let wrapper = dir.join(format!("vecview-{stem}-{tag}-main.typ"));
    std::fs::write(&wrapper, cmarker_wrapper_src(md)?)
        .context("failed to write markdown wrapper")?;
    let template = dir.join(format!("vecview-{stem}-{tag}-{{p}}.svg"));

    // --root "/" so typst may read the .md (and absolute-path images) outside the temp dir.
    let child = Command::new("typst")
        .arg("watch")
        .arg("--root")
        .arg("/")
        .arg(&wrapper)
        .arg(&template)
        .arg("--format")
        .arg("svg")
        .stdout(Stdio::null())
        .stderr(Stdio::piped()) // Captured and surfaced in vv's status line (see the reader thread).
        .spawn()
        .context("failed to launch typst watch (markdown)")?;

    Ok((Source::Typst { dir, stem, tag }, child))
}

/// Export a Typst or Markdown document to PDF via `typst compile`. Markdown is wrapped with the
/// cmarker package (same as the live preview) and compiled with `--root /`. stderr is captured (not
/// inherited) so this is safe to call while the interactive display is active.
fn export_to_pdf(file: &Path, ext: &str, out: &Path) -> Result<()> {
    if which_typst().is_none() {
        bail!("typst is not on PATH. PDF export requires typst.");
    }
    match ext {
        "typ" => typst_compile_pdf(file, None, out),
        "md" | "markdown" => {
            let dir = std::env::temp_dir();
            let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("vv");
            let tag = std::process::id();
            let wrapper = dir.join(format!("vv-export-{stem}-{tag}.typ"));
            std::fs::write(&wrapper, cmarker_wrapper_src(file)?)
                .context("failed to write markdown wrapper")?;
            let r = typst_compile_pdf(&wrapper, Some("/"), out);
            let _ = std::fs::remove_file(&wrapper);
            r
        }
        other => bail!("PDF export supports only .typ and .md/.markdown (not .{other})"),
    }
}

/// Run `typst compile <main> <out> --format pdf` (optional `--root`), capturing stderr so it never
/// leaks onto the terminal; a compile error is surfaced via the returned `Err`.
fn typst_compile_pdf(main: &Path, root: Option<&str>, out: &Path) -> Result<()> {
    let mut cmd = Command::new("typst");
    cmd.arg("compile");
    if let Some(r) = root {
        cmd.arg("--root").arg(r);
    }
    let output = cmd
        .arg(main)
        .arg(out)
        .arg("--format")
        .arg("pdf")
        .output()
        .context("failed to launch typst compile (pdf)")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let detail = err
            .lines()
            .find(|l| l.to_lowercase().contains("error"))
            .unwrap_or_else(|| err.lines().next().unwrap_or(""))
            .trim();
        bail!("typst PDF compile failed: {detail}");
    }
    Ok(())
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn which_typst() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join("typst");
        candidate.is_file().then_some(candidate)
    })
}

/// Load the SVG and render/display the viewport for the current zoom/pan state.
fn render_and_display(
    svg_path: &Path,
    renderer: &Renderer,
    backend: &dyn OutputBackend,
    state: &mut ViewState,
    base: &mut Option<BaseFrame>,
) -> Result<()> {
    let doc = SvgDocument::open(
        svg_path
            .to_str()
            .ok_or_else(|| anyhow!("path is not UTF-8"))?,
    )?;
    let page = doc.render_page(0)?;
    let pw = page.width.max(1.0);
    let ph = page.height.max(1.0);

    // The output is always the pane (display area) size. Zoom is expressed by the size of the
    // viewport rect.
    let (out_w, out_h) = available_area(backend.name(), state.scale);

    // If there's a content-fit request, compute zoom/center from the content boundary and delegate
    // to the normal viewport computation.
    if let Some(fit) = state.pending_fit.take() {
        if let Some(bbox) = content_bbox(&page) {
            apply_fit(fit, bbox, pw, ph, out_w, out_h, state);
        }
    }

    // Compute the viewport from the center (page center if unset) (clamped within the page internally).
    let center = state.center.unwrap_or((pw / 2.0, ph / 2.0));
    let viewport = viewport_for(pw, ph, out_w, out_h, state.zoom, center);
    state.last_vw = viewport[2];
    state.last_vh = viewport[3];
    // Save the clamped viewport center so subsequent panning doesn't break at the edges.
    state.center = Some((viewport[0] + viewport[2] / 2.0, viewport[1] + viewport[3] / 2.0));
    state.last_viewport = Some(viewport);

    let mut rgba = renderer.render(&page, out_w, out_h, viewport)?;
    // While in copy mode, cache the base before applying the overlay (reused across subsequent caret moves).
    if state.copy.is_some() {
        *base = Some(BaseFrame { out_w, out_h, viewport, rgba: rgba.clone() });
    }
    if let Some(cm) = &state.copy {
        overlay_selection(&mut rgba, out_w, out_h, viewport, cm);
    }
    backend.display(&rgba, out_w, out_h)?;
    Ok(())
}

/// Determine the resolution (pixels) to rasterize at.
///
/// With tmux placeholder placement, the terminal doesn't report pixel dimensions (width/height=0),
/// leaving no choice but to rely on cell count × an estimated cell size (8x16). In environments
/// where the actual cell size is larger than the estimate (HiDPI, etc.), this stays low-resolution
/// and gets stretched and blurred on the terminal side. So, only for placeholder, rasterize at
/// higher resolution and let the terminal's downscaling sharpen it (supersampling). The factor is
/// VECVIEW_SCALE (`scale`, default 1=native, 1..=4). A high factor grows transfer size with the
/// square of the factor and can crash the terminal under continuous operation, so the default is
/// native. Direct placement (a=T) and the framebuffer display native pixels, so always keep them
/// at native scale.
fn available_area(backend_name: &str, scale: u32) -> (u32, u32) {
    if backend_name.starts_with("framebuffer") {
        if let Some(sz) = read_fb_virtual_size() {
            return sz;
        }
    }
    // Over-render only for placeholder, where the image is downscaled into cols×rows cells.
    let ss = if backend_name.contains("placeholder") {
        scale
    } else {
        1
    };
    // The terminal's pixel size (estimated from cell count if unavailable, with a fixed value as a
    // last resort). The estimated cell size can be overridden with VECVIEW_CELL_PX=WxH. Over
    // SSH+tmux etc., pixel dimensions don't propagate (width/height=0) and the actual cell size
    // differs from the estimate, so Sixel displays shrunken; tuning it once per environment lets
    // the pane be filled correctly thereafter.
    // Cell size: VECVIEW_CELL_PX → TIOCGWINSZ → a one-shot terminal query (covers tmux), else 8x16.
    let (cell_w, cell_h) = cell_px().unwrap_or((8, 16));
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.height > 0 {
            return (ws.width as u32, ws.height as u32);
        }
        if ws.columns > 0 && ws.rows > 0 {
            // With tmux passthrough sixel, when the image reaches the bottom of the pane (= the
            // bottom of the physical screen), the whole screen scrolls and even neighboring panes
            // break. Leave the bottom 1 row free so it doesn't reach the bottom edge.
            let rows = if backend_name.contains("passthrough") {
                (ws.rows as u32).saturating_sub(1).max(1)
            } else {
                ws.rows as u32
            };
            return (ws.columns as u32 * cell_w * ss, rows * cell_h * ss);
        }
    }
    (1280 * ss, 800 * ss)
}

/// Estimated cell size (width, height) for terminals that don't report pixel dimensions. Default
/// 8×16. Can be overridden with the `VECVIEW_CELL_PX="WxH"` environment variable (e.g. `10x17`).
/// Each value is clamped to 1..=128.
fn fallback_cell_px() -> (u32, u32) {
    std::env::var("VECVIEW_CELL_PX")
        .ok()
        .and_then(|s| parse_cell_px(&s))
        .unwrap_or((8, 16))
}

/// Whether the window the given tmux pane belongs to is currently active (in the foreground). None
/// if undeterminable. Used for the visibility check that clears the image when switching to another
/// window with tmux placeholder kitty.
fn pane_window_active(pane: &str) -> Option<bool> {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "-t", pane, "#{window_active}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim() == "1")
}

/// Add everything already queued on the channel to the received first message to form a burst.
/// Coalesces consecutive Reloads/keys into a single draw, and doesn't miss a Quit queued behind them.
fn drain_burst(first: Msg, rx: &mpsc::Receiver<Msg>) -> Vec<Msg> {
    let mut msgs = vec![first];
    while let Ok(m) = rx.try_recv() {
        msgs.push(m);
    }
    msgs
}

/// The minimum interval (ms) for image transfer during continuous input. Overridden by
/// `VECVIEW_MIN_FRAME_MS`, minimum 16. Smaller is smoother to follow but raises the risk of
/// outpacing the terminal and crashing it. The default is backend-dependent (`default`): tmux
/// passthrough (kitty placeholder / sixel) is heavy to transfer and piles up on the terminal side,
/// so be conservative; direct placement is native and light, so use a smaller value.
fn min_frame_ms(default: u64) -> u64 {
    std::env::var("VECVIEW_MIN_FRAME_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|v| v.max(16))
        .unwrap_or(default)
}

/// The periodic redraw interval (ms) for passthrough sixel. Overridden by `VECVIEW_REDRAW_MS`,
/// default 1000, minimum 100. Smaller restores faster after the image is cleared, but increases
/// the transfer volume of sixel resends accordingly.
fn redraw_interval_ms() -> u64 {
    std::env::var("VECVIEW_REDRAW_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|v| v.max(100))
        .unwrap_or(1000)
}

/// The visibility-polling interval (ms) for the tmux placeholder path. OFF by default: each tick
/// spawns `tmux display-message` to detect whether our window is active, and that query makes tmux
/// refresh the client (forcing the terminal to recomposite the large placeholder image), which pins
/// a CPU core even while idle. Enable with `VECVIEW_VIS_POLL_MS=<ms>` (minimum 100) only if the
/// lingering image when you switch tmux windows bothers you and your terminal tolerates the redraw.
fn vis_poll_ms() -> Option<u64> {
    match std::env::var("VECVIEW_VIS_POLL_MS") {
        Ok(s) => match s.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(v) => Some(v.max(100)),
            Err(_) => None,
        },
        Err(_) => None,
    }
}

/// Parse "WxH" (separator `x` or `X`) into (width, height). Each value is clamped to 1..=128.
fn parse_cell_px(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    Some((w.clamp(1, 128), h.clamp(1, 128)))
}

/// Decide the supersampling factor. Precedence: CLI argument, then the VECVIEW_SCALE environment
/// variable, then a default of 1. All are clamped to 1..=4. The default is 1 (native) because, on
/// the tmux placeholder path, a high factor grows the per-frame transfer size with the square of the
/// factor, making it easy for the terminal (Ghostty, etc.) to fail to keep up with image updates and
/// crash under continuous operation. Specify 2 or higher in environments that want sharpness and can
/// take it.
fn resolve_scale(arg: Option<u32>) -> u32 {
    arg.or_else(|| {
        std::env::var("VECVIEW_SCALE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
    })
    .unwrap_or(1)
    .clamp(1, 4)
}

/// Compute the bounding rect [x, y, w, h] of the content (ink) from all path vertices in the page
/// (including control points). Since pdftocairo's SVG has no full-page background rect, this is the
/// actual content boundary. None if there are no visible paths. Including control points makes it
/// slightly wider than the exact curve boundary, but it's sufficient for fitting purposes.
fn content_bbox(page: &Page) -> Option<[f32; 4]> {
    let (mut min_x, mut min_y) = (f32::INFINITY, f32::INFINITY);
    let (mut max_x, mut max_y) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    let mut acc = |[x, y]: [f32; 2]| {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    };
    for cmd in &page.commands {
        match cmd {
            DrawCommand::Path(p) => {
                for seg in &p.segments {
                    match *seg {
                        PathSegment::MoveTo(a) | PathSegment::LineTo(a) => acc(a),
                        PathSegment::QuadTo(c, a) => {
                            acc(c);
                            acc(a);
                        }
                        PathSegment::CubicTo(c1, c2, a) => {
                            acc(c1);
                            acc(c2);
                            acc(a);
                        }
                        PathSegment::Close => {}
                    }
                }
            }
            DrawCommand::Image(img) => {
                let [x, y, w, h] = img.rect;
                acc([x, y]);
                acc([x + w, y + h]);
            }
        }
    }
    (min_x.is_finite() && max_x > min_x && max_y > min_y)
        .then_some([min_x, min_y, max_x - min_x, max_y - min_y])
}

/// Set `state`'s zoom/center to fit the content boundary `bbox`. Width = full left-right,
/// Height = full top-bottom. zoom is computed as a ratio relative to the fit factor (s0=100%) and clamped to range.
fn apply_fit(fit: Fit, bbox: [f32; 4], pw: f32, ph: f32, out_w: u32, out_h: u32, state: &mut ViewState) {
    let [bx, by, bw, bh] = bbox;
    let bw = bw.max(1.0);
    let bh = bh.max(1.0);
    let s0 = (out_w as f32 / pw).min(out_h as f32 / ph); // The whole-page-fit (=100%) factor.
    let s = match fit {
        Fit::Width => out_w as f32 / bw,
        Fit::Height => out_h as f32 / bh,
    };
    let zoom = ((s / s0) * 100.0).round();
    state.zoom = (zoom as i64).clamp(ZOOM_MIN as i64, ZOOM_MAX as i64) as u32;
    state.center = Some((bx + bw / 2.0, by + bh / 2.0));
}

/// Print the sizes the terminal reports and the render resolution derived from them, then exit (for investigating resolution).
fn probe_and_exit(backend: Option<&str>, scale: u32) -> ! {
    let b = detect_backend(backend);
    println!("backend            = {}", b.name());
    println!("scale (SS factor)  = {scale}");
    println!("TMUX env           = {}", std::env::var_os("TMUX").is_some());
    match crossterm::terminal::window_size() {
        Ok(ws) => {
            println!(
                "window_size        = columns={} rows={} width(px)={} height(px)={}",
                ws.columns, ws.rows, ws.width, ws.height
            );
            if ws.columns > 0 && ws.rows > 0 && ws.width > 0 && ws.height > 0 {
                println!(
                    "cell size(px)      = {} x {}",
                    ws.width as u32 / ws.columns as u32,
                    ws.height as u32 / ws.rows as u32
                );
            } else {
                println!("cell size(px)      = unknown (pixel values are 0 -> falls back to 8x16 estimate)");
            }
        }
        Err(e) => println!("window_size        = error: {e}"),
    }
    let (cw, ch) = fallback_cell_px();
    println!("fallback cell(px)  = {cw} x {ch}  <- used only if detection fails");
    // Detect the real cell size (may query the terminal via CSI 16 t); needs raw mode so the reply
    // isn't echoed/line-buffered.
    let raw = crossterm::terminal::enable_raw_mode().is_ok();
    let detected = cell_px();
    if raw {
        crossterm::terminal::disable_raw_mode().ok();
    }
    match detected {
        Some((dw, dh)) => println!("detected cell(px)  = {dw} x {dh}  <- TIOCGWINSZ / CSI 16t query / VECVIEW_CELL_PX"),
        None => println!("detected cell(px)  = none (using fallback)"),
    }
    let (w, h) = available_area(b.name(), scale);
    println!("available_area(px) = {w} x {h}  <- rasterizing at this resolution");
    std::process::exit(0);
}

fn read_fb_virtual_size() -> Option<(u32, u32)> {
    let s = std::fs::read_to_string("/sys/class/graphics/fb0/virtual_size").ok()?;
    let (w, h) = s.trim().split_once(',')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Compute the in-page viewport rect [x, y, w, h] to display. At zoom=100 the whole page fits the
/// display area (fit); raising zoom shrinks the viewport and magnifies around the center `center`.
/// The viewport's aspect ratio is matched to the output (out_w/out_h) to prevent distortion.
fn viewport_for(
    pw: f32,
    ph: f32,
    out_w: u32,
    out_h: u32,
    zoom: u32,
    center: (f32, f32),
) -> [f32; 4] {
    let pw = pw.max(1.0);
    let ph = ph.max(1.0);
    let s0 = (out_w as f32 / pw).min(out_h as f32 / ph);
    let s = s0 * (zoom as f32 / 100.0);
    let vw = out_w as f32 / s;
    let vh = out_h as f32 / s;
    [
        clamp_origin(center.0, vw, pw),
        clamp_origin(center.1, vh, ph),
        vw,
        vh,
    ]
}

/// Decide the viewport origin on one axis. When the viewport is smaller than the page (zoomed in),
/// keep the origin within the page so the white outside the page isn't shown. When larger (at or
/// below fit), center it (letterbox).
fn clamp_origin(center: f32, v: f32, p: f32) -> f32 {
    if v >= p {
        (p - v) / 2.0
    } else {
        (center - v / 2.0).clamp(0.0, p - v)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_fit, content_bbox, parse_cell_px, parse_key, resolve_scale, viewport_for, Action,
        Fit, Keymap, ViewState,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::HashMap;
    use vecview_core::{DrawCommand, Page, PathData, PathSegment};

    fn key(code: KeyCode, ctrl: bool) -> KeyEvent {
        let m = if ctrl {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        KeyEvent::new(code, m)
    }

    #[test]
    fn parse_key_forms() {
        assert_eq!(parse_key("q"), Some((KeyCode::Char('q'), false)));
        assert_eq!(parse_key("+"), Some((KeyCode::Char('+'), false)));
        assert_eq!(parse_key("?"), Some((KeyCode::Char('?'), false)));
        assert_eq!(parse_key("ctrl+c"), Some((KeyCode::Char('c'), true)));
        assert_eq!(parse_key("left"), Some((KeyCode::Left, false)));
        assert_eq!(parse_key("space"), Some((KeyCode::Char(' '), false)));
        assert_eq!(parse_key("PageDown"), Some((KeyCode::PageDown, false)));
        assert_eq!(parse_key("esc"), Some((KeyCode::Esc, false)));
        assert_eq!(parse_key("foo"), None); // An unknown multi-character name is disallowed.
    }

    #[test]
    fn default_bindings_match_request() {
        // Defaults: arrows = pan, j/k = next/previous page, h/l = first/last page.
        let km = Keymap::build(&HashMap::new());
        assert!(matches!(km.action(&key(KeyCode::Up, false)), Some(Action::Pan(0.0, -1.0))));
        assert!(matches!(km.action(&key(KeyCode::Down, false)), Some(Action::Pan(0.0, 1.0))));
        assert!(matches!(km.action(&key(KeyCode::Left, false)), Some(Action::Pan(-1.0, 0.0))));
        assert!(matches!(km.action(&key(KeyCode::Right, false)), Some(Action::Pan(1.0, 0.0))));
        assert!(matches!(km.action(&key(KeyCode::Char('j'), false)), Some(Action::NextPage)));
        assert!(matches!(km.action(&key(KeyCode::Char('k'), false)), Some(Action::PrevPage)));
        assert!(matches!(km.action(&key(KeyCode::Char('h'), false)), Some(Action::FirstPage)));
        assert!(matches!(km.action(&key(KeyCode::Char('l'), false)), Some(Action::LastPage)));
        assert!(matches!(km.action(&key(KeyCode::Char('c'), true)), Some(Action::Quit)));
        // The old hjkl pan is unassigned by default (h/l are different actions, j/k are pages).
        assert!(matches!(km.action(&key(KeyCode::Char('j'), false)), Some(Action::NextPage)));
    }

    #[test]
    fn override_replaces_action_keys() {
        // Set next_page to space only. j becomes unassigned.
        let mut ov = HashMap::new();
        ov.insert("next_page".to_string(), vec!["space".to_string()]);
        let km = Keymap::build(&ov);
        assert!(matches!(km.action(&key(KeyCode::Char(' '), false)), Some(Action::NextPage)));
        assert!(km.action(&key(KeyCode::Char('j'), false)).is_none());
        // prev_page, which wasn't overridden, stays at the default k.
        assert!(matches!(km.action(&key(KeyCode::Char('k'), false)), Some(Action::PrevPage)));
    }

    fn path(segments: Vec<PathSegment>) -> DrawCommand {
        DrawCommand::Path(PathData {
            segments,
            fill: None,
            stroke: None,
        })
    }

    fn state() -> ViewState {
        ViewState {
            page: 0,
            zoom: 100,
            center: None,
            last_vw: 0.0,
            last_vh: 0.0,
            scale: 2,
            pending_fit: None,
            help: false,
            copy: None,
            last_viewport: None,
            status: None,
        }
    }

    fn glyph(ch: char, x: f32, y: f32) -> vecview_pdf::Glyph {
        vecview_pdf::Glyph {
            ch,
            rect: [x, y, 8.0, 10.0],
        }
    }

    #[test]
    fn copy_mode_groups_lines_and_drops_control_chars() {
        // Two lines (y=0 and y=20), with pdfium-derived \r\n (zero-rect control characters) in between.
        let glyphs = vec![
            glyph('a', 0.0, 0.0),
            glyph('b', 8.0, 0.0),
            vecview_pdf::Glyph { ch: '\r', rect: [0.0, 0.0, 0.0, 0.0] },
            vecview_pdf::Glyph { ch: '\n', rect: [0.0, 0.0, 0.0, 0.0] },
            glyph('c', 0.0, 20.0),
            glyph('d', 8.0, 20.0),
        ];
        let cm = super::CopyMode::new(glyphs).unwrap();
        // Control characters are excluded, and the 4 visible characters split into 2 lines.
        assert_eq!(cm.glyphs.len(), 4);
        assert_eq!(cm.lines.len(), 2);
        assert_eq!(cm.lines[0], vec![0, 1]);
        assert_eq!(cm.lines[1], vec![2, 3]);
    }

    #[test]
    fn copy_mode_selected_text_inserts_newline_on_line_change() {
        let glyphs = vec![
            glyph('a', 0.0, 0.0),
            glyph('b', 8.0, 0.0),
            glyph('c', 0.0, 20.0),
        ];
        let mut cm = super::CopyMode::new(glyphs).unwrap();
        cm.anchor = Some(0);
        cm.cursor = 2; // Select all.
        assert_eq!(cm.selected_text(), "ab\nc");
        // If unselected, only the single caret character.
        cm.anchor = None;
        cm.cursor = 1;
        assert_eq!(cm.selected_text(), "b");
    }

    #[test]
    fn copy_mode_vertical_move_keeps_column() {
        // Two lines, 3 characters each. On the upper line at the 2nd character, move down -> near the 2nd character of the lower line.
        let glyphs = vec![
            glyph('a', 0.0, 0.0),
            glyph('b', 8.0, 0.0),
            glyph('c', 16.0, 0.0),
            glyph('d', 0.0, 20.0),
            glyph('e', 8.0, 20.0),
            glyph('f', 16.0, 20.0),
        ];
        let mut cm = super::CopyMode::new(glyphs).unwrap();
        cm.cursor = 1; // 'b' (x≈8).
        cm.move_line(1);
        assert_eq!(cm.glyphs[cm.cursor].ch, 'e'); // Same column on the lower line.
        cm.move_line(-1);
        assert_eq!(cm.glyphs[cm.cursor].ch, 'b');
    }

    #[test]
    fn content_bbox_spans_all_vertices() {
        // The bounding rect covering all vertices (including control points) of two paths.
        let page = Page {
            width: 1000.0,
            height: 1000.0,
            commands: vec![
                path(vec![
                    PathSegment::MoveTo([200.0, 300.0]),
                    PathSegment::LineTo([400.0, 300.0]),
                ]),
                path(vec![
                    PathSegment::MoveTo([300.0, 400.0]),
                    PathSegment::CubicTo([350.0, 450.0], [700.0, 500.0], [600.0, 700.0]),
                ]),
            ],
        };
        // x:200..700, y:300..700 -> [200,300, 500,400].
        assert_eq!(content_bbox(&page), Some([200.0, 300.0, 500.0, 400.0]));
    }

    #[test]
    fn content_bbox_empty_is_none() {
        let page = Page {
            width: 100.0,
            height: 100.0,
            commands: vec![],
        };
        assert_eq!(content_bbox(&page), None);
    }

    #[test]
    fn fit_width_fills_output_width_and_centers_on_content() {
        // Page 1000x1000 into 1000x1000 output (s0=1). Content [200,300,600,400].
        // Width fit: s=out_w/bw=1000/600=1.667 -> zoom=167%, center = content center (500,500).
        let mut s = state();
        apply_fit(Fit::Width, [200.0, 300.0, 600.0, 400.0], 1000.0, 1000.0, 1000, 1000, &mut s);
        assert_eq!(s.zoom, 167);
        assert_eq!(s.center, Some((500.0, 500.0)));
    }

    #[test]
    fn fit_height_fills_output_height() {
        // Height fit: s=out_h/bh=1000/400=2.5 -> zoom=250%, center = content center.
        let mut s = state();
        apply_fit(Fit::Height, [200.0, 300.0, 600.0, 400.0], 1000.0, 1000.0, 1000, 1000, &mut s);
        assert_eq!(s.zoom, 250);
        assert_eq!(s.center, Some((500.0, 500.0)));
    }

    #[test]
    fn parse_cell_px_accepts_x_separator_and_clamps() {
        assert_eq!(parse_cell_px("10x17"), Some((10, 17)));
        assert_eq!(parse_cell_px(" 12 X 24 "), Some((12, 24)));
        assert_eq!(parse_cell_px("0x999"), Some((1, 128))); // clamped to 1..=128
        assert_eq!(parse_cell_px("8"), None);
        assert_eq!(parse_cell_px("axb"), None);
    }

    #[test]
    fn scale_precedence_arg_over_env_with_clamp() {
        // The argument takes top priority. Out-of-range values are clamped to 1..=4.
        assert_eq!(resolve_scale(Some(3)), 3);
        assert_eq!(resolve_scale(Some(9)), 4);
        assert_eq!(resolve_scale(Some(0)), 1);
    }

    #[test]
    fn fit_shows_whole_page_centered() {
        // A 2:1 page into 1000x1000. The fit factor is width-limited at s0=5, viewport = 200x200,
        // and center (100,50), giving [0, -50, 200, 200] (margins top and bottom).
        let vp = viewport_for(200.0, 100.0, 1000, 1000, 100, (100.0, 50.0));
        assert_eq!(vp, [0.0, -50.0, 200.0, 200.0]);
    }

    #[test]
    fn zoom_in_shrinks_viewport_around_center() {
        // At 200%, a square 100x100 page into 1000x1000. s0=10, s=20, viewport=50x50,
        // center (50,50) -> [25, 25, 50, 50].
        let vp = viewport_for(100.0, 100.0, 1000, 1000, 200, (50.0, 50.0));
        assert_eq!(vp, [25.0, 25.0, 50.0, 50.0]);
    }

    #[test]
    fn viewport_clamped_inside_page_at_edge() {
        // Even when panning to the edge, while zoomed in the viewport doesn't overflow outside the page (white).
        // 100x100, 200%, viewport 50x50, center at the bottom-right corner (100,100) -> origin clamped at [50,50].
        let vp = viewport_for(100.0, 100.0, 1000, 1000, 200, (100.0, 100.0));
        assert_eq!(vp, [50.0, 50.0, 50.0, 50.0]);
    }
}
