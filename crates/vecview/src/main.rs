//! vecview CLI エントリポイント。
//!
//! `vecview <FILE>` で SVG / Typst / PDF をターミナル内にベクター表示する。Typst（`.typ`）は
//! 内部で `typst watch` を起動して SVG を生成し、その SVG を監視してライブ再描画する。
//! PDF（`.pdf`）は `pdftocairo` でページごとに SVG へ変換し、元 PDF を監視して保存のたびに
//! 再変換する。いずれもブラウザ不要・ターミナル内完結のベクタープレビューを実現する。
//!
//! 端末（TTY）で起動するとインタラクティブモードになり、キーでズーム・ページ送り・終了できる。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use vecview_core::{Document, OutputBackend};
use vecview_output::detect_backend;
use vecview_renderer::Renderer;
use vecview_svg::SvgDocument;

/// 再描画ループへのメッセージ。
enum Msg {
    /// 監視対象ファイルが変更された。
    Reload,
    /// 終了要求（Ctrl-C 等）。
    Quit,
    /// キー入力。
    Key(KeyEvent),
}

/// 表示ソース。ページごとに 1 つの SVG を持つ。
#[derive(Clone)]
enum Source {
    /// 単一 SVG ファイル。
    Svg(PathBuf),
    /// Typst（`typst watch` が `vecview-<stem>-<p>.svg` をページごとに出力）。
    Typst { dir: PathBuf, stem: String },
    /// PDF（`pdftocairo` が `vecview-<stem>-<p>.svg` をページごとに生成。元 PDF を監視し
    /// 保存のたび全ページ再変換する）。
    Pdf {
        pdf: PathBuf,
        dir: PathBuf,
        stem: String,
    },
}

impl Source {
    /// ページ `idx`（0始まり）の SVG パス。
    fn page_path(&self, idx: usize) -> PathBuf {
        match self {
            Source::Svg(p) => p.clone(),
            Source::Typst { dir, stem } | Source::Pdf { dir, stem, .. } => {
                dir.join(format!("vecview-{stem}-{}.svg", idx + 1))
            }
        }
    }

    /// 現在存在するページ数（Typst/PDF は連番ファイルを数える）。最低 1。
    fn page_count(&self) -> usize {
        match self {
            Source::Svg(_) => 1,
            Source::Typst { dir, stem } | Source::Pdf { dir, stem, .. } => {
                let mut n = 0;
                while dir.join(format!("vecview-{stem}-{}.svg", n + 1)).exists() {
                    n += 1;
                }
                n.max(1)
            }
        }
    }

    /// 監視すべきディレクトリ。Typst は生成先、PDF/SVG は元ファイルのあるディレクトリ。
    fn watch_dir(&self) -> PathBuf {
        let base = match self {
            Source::Svg(p) => p.parent().map(Path::to_path_buf),
            Source::Typst { dir, .. } => Some(dir.clone()),
            Source::Pdf { pdf, .. } => pdf.parent().map(Path::to_path_buf),
        };
        base.filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// 変更パスがこのソースの監視対象（ページファイル、または元 PDF）か。
    fn owns(&self, path: &Path) -> bool {
        match self {
            Source::Svg(p) => path == p,
            Source::Pdf { pdf, .. } => path == pdf,
            Source::Typst { dir, stem } => {
                path.parent() == Some(dir.as_path())
                    && path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(|n| n.starts_with(&format!("vecview-{stem}-")) && n.ends_with(".svg"))
                        .unwrap_or(false)
            }
        }
    }

    /// 監視対象が変化したときの再生成。PDF は元ファイルを全ページ再変換する。
    fn reconvert(&self) -> Result<()> {
        if let Source::Pdf { pdf, dir, stem } = self {
            vecview_pdf::convert_to_svgs(pdf, dir, stem)?;
        }
        Ok(())
    }
}

#[derive(Parser, Debug)]
#[command(name = "vecview", version, about = "ベクターグラフィクスをターミナルに表示する")]
struct Args {
    /// 表示するファイル（SVG / Typst .typ / PDF）。
    file: PathBuf,

    /// 初期ズーム倍率（%）。
    #[arg(short, long, default_value_t = 100)]
    zoom: u32,

    /// 出力バックエンド強制指定 [kitty|tmux|framebuffer]。
    #[arg(short, long)]
    backend: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.file.exists() {
        bail!("ファイルが見つかりません: {}", args.file.display());
    }

    let ext = args
        .file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Typst は typst watch を起動して SVG を生成。SVG はそのまま監視。
    let (source, mut child) = match ext.as_str() {
        "typ" => {
            let (source, child) = spawn_typst_watch(&args.file)?;
            (source, Some(child))
        }
        "svg" => {
            let canonical = std::fs::canonicalize(&args.file).unwrap_or_else(|_| args.file.clone());
            (Source::Svg(canonical), None)
        }
        // PDF は起動時に全ページを SVG 化（typst のような常駐ウォッチャは不要。元 PDF の
        // 変更は下のファイル監視で検知して再変換する）。
        "pdf" => {
            let canonical = std::fs::canonicalize(&args.file).unwrap_or_else(|_| args.file.clone());
            let stem = canonical
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("vecview")
                .to_string();
            let dir = std::env::temp_dir();
            vecview_pdf::convert_to_svgs(&canonical, &dir, &stem).context("PDF の変換")?;
            (Source::Pdf { pdf: canonical, dir, stem }, None)
        }
        other => bail!("未対応の拡張子です: .{other}（svg / typ / pdf のみ対応）"),
    };

    let backend = detect_backend(args.backend.as_deref());
    let renderer = Renderer::new().context("レンダラー初期化")?;
    eprintln!(
        "vecview: backend={} | GPU={} | {}",
        backend.name(),
        renderer.adapter_info,
        source.watch_dir().display()
    );
    eprintln!("操作: +/- ズーム  0 リセット  j/Space 次  k 前  q 終了");

    // ファイル監視（親ディレクトリを NonRecursive で監視し atomic rename を取りこぼさない）。
    // 対象ソースのページファイル以外の変更（temp_dir のノイズ等）は無視する。
    let (tx, rx) = mpsc::channel::<Msg>();
    let watch_tx = tx.clone();
    let owns_source = source.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(120),
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
    .context("Ctrl-C ハンドラ設定")?;

    // TTY ならインタラクティブ（raw mode + キー入力スレッド）。
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdout());
    if interactive {
        crossterm::terminal::enable_raw_mode().ok();
        let key_tx = tx.clone();
        std::thread::spawn(move || loop {
            match crossterm::event::read() {
                Ok(Event::Key(k)) => {
                    if key_tx.send(Msg::Key(k)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        });
    }

    // 代替スクリーンへ切替（終了時に端末状態を復元し、描画した画像を残さない）。
    backend.enter().ok();

    let mut state = ViewState {
        page: 0,
        zoom: args.zoom.clamp(ZOOM_MIN, ZOOM_MAX),
        center: None,
        last_vw: 0.0,
        last_vh: 0.0,
    };
    // 最後に描画した (ページ, mtime)。描画のたびに SVG を読むと atime が変わり notify が
    // 再発火する（自己トリガー）ため、同一ページで mtime 不変なら描画しない。
    let mut last_render: Option<(usize, SystemTime)> = None;

    // 初回描画（.typ は生成待ちのため存在しないことがある）。
    render_current(&source, &mut state, &renderer, backend.as_ref(), &mut last_render);

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Quit => break,
            Msg::Key(k) => {
                if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }
                match key_action(&k) {
                    Some(Action::Quit) => break,
                    Some(action) => {
                        apply_action(action, &source, &mut state);
                        render_current(
                            &source,
                            &mut state,
                            &renderer,
                            backend.as_ref(),
                            &mut last_render,
                        );
                    }
                    None => {}
                }
            }
            Msg::Reload => {
                // PDF は元ファイルが変わったので全ページを再変換してから描画する。
                // 監視対象（元 PDF）と描画対象（temp の SVG）が別なので自己トリガーは起きない。
                if matches!(source, Source::Pdf { .. }) {
                    if let Err(e) = source.reconvert() {
                        eprintln!("vecview: PDF 再変換エラー: {e:#}");
                        continue;
                    }
                    let pc = source.page_count();
                    if state.page >= pc {
                        state.page = pc - 1;
                        state.center = None;
                    }
                    render_current(&source, &mut state, &renderer, backend.as_ref(), &mut last_render);
                    continue;
                }
                let path = source.page_path(state.page);
                let current = mtime_of(&path);
                if current.is_none() || (last_render.map(|(p, _)| p) == Some(state.page)
                    && current == last_render.map(|(_, m)| m))
                {
                    continue;
                }
                render_current(&source, &mut state, &renderer, backend.as_ref(), &mut last_render);
            }
        }
    }

    // 後始末：raw mode 解除 + 端末復帰 + typst 子プロセス停止。
    if interactive {
        crossterm::terminal::disable_raw_mode().ok();
    }
    backend.leave().ok();
    if let Some(child) = child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }

    Ok(())
}

/// 現在の表示状態。zoom はフィット倍率に対する%。center はビューポート中心（ページ座標、
/// None ならページ中央）。last_vw/vh は直近のビューポート寸法（パン量の基準）。
struct ViewState {
    page: usize,
    zoom: u32,
    center: Option<(f32, f32)>,
    last_vw: f32,
    last_vh: f32,
}

/// キー操作。
enum Action {
    Quit,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// ビューポートを (dx, dy) 方向に移動（符号のみ。量は last_vw/vh から算出）。
    Pan(f32, f32),
    NextPage,
    PrevPage,
}

/// ズーム倍率（%）。最小はフィット(100)、最大は16倍。
const ZOOM_MIN: u32 = 100;
const ZOOM_MAX: u32 = 1600;

fn key_action(k: &KeyEvent) -> Option<Action> {
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        return Some(Action::Quit);
    }
    match k.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
        KeyCode::Char('+') | KeyCode::Char('=') => Some(Action::ZoomIn),
        KeyCode::Char('-') | KeyCode::Char('_') => Some(Action::ZoomOut),
        KeyCode::Char('0') => Some(Action::ZoomReset),
        // パン（vim hjkl ＋ 矢印）。
        KeyCode::Char('h') | KeyCode::Left => Some(Action::Pan(-1.0, 0.0)),
        KeyCode::Char('l') | KeyCode::Right => Some(Action::Pan(1.0, 0.0)),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::Pan(0.0, -1.0)),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::Pan(0.0, 1.0)),
        // ページ送り。
        KeyCode::Char('n') | KeyCode::Char(' ') | KeyCode::PageDown => Some(Action::NextPage),
        KeyCode::Char('p') | KeyCode::Backspace | KeyCode::PageUp => Some(Action::PrevPage),
        _ => None,
    }
}

fn apply_action(action: Action, source: &Source, state: &mut ViewState) {
    match action {
        Action::Quit => {}
        // 乗算式（約1.5倍ずつ）。フィット(100%)が最小、それ以下は白地が広がるだけなので不可。
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
            if state.page + 1 < source.page_count() {
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
    }
}

/// 現在ページを描画して表示する。失敗（ファイル未生成等）時は静かにスキップ。
fn render_current(
    source: &Source,
    state: &mut ViewState,
    renderer: &Renderer,
    backend: &dyn OutputBackend,
    last_render: &mut Option<(usize, SystemTime)>,
) {
    let path = source.page_path(state.page);
    if !path.exists() {
        return;
    }
    match render_and_display(&path, renderer, backend, state) {
        Ok(()) => {
            *last_render = mtime_of(&path).map(|m| (state.page, m));
        }
        Err(e) => eprintln!("vecview: 描画エラー: {e:#}"),
    }
}

/// `typst watch <file> <tmp>-{p}.svg` を起動し、(Source, 子プロセス) を返す。
fn spawn_typst_watch(typ: &Path) -> Result<(Source, Child)> {
    if which_typst().is_none() {
        bail!("typst が PATH にありません。Typst プレビューには typst が必要です。");
    }
    let stem = typ
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("vecview")
        .to_string();
    let dir = std::env::temp_dir();
    // 複数ページ文書でも typst がエラーにならないようページ番号テンプレート {p} を使う。
    let template = dir.join(format!("vecview-{stem}-{{p}}.svg"));

    let child = Command::new("typst")
        .arg("watch")
        .arg(typ)
        .arg(&template)
        .arg("--format")
        .arg("svg")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit()) // typst のコンパイルエラーを表示。
        .spawn()
        .context("typst watch の起動に失敗")?;

    Ok((Source::Typst { dir, stem }, child))
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

/// SVG を読み込み、現在のズーム/パン状態に応じたビューポートを描画して表示する。
fn render_and_display(
    svg_path: &Path,
    renderer: &Renderer,
    backend: &dyn OutputBackend,
    state: &mut ViewState,
) -> Result<()> {
    let doc = SvgDocument::open(
        svg_path
            .to_str()
            .ok_or_else(|| anyhow!("パスが UTF-8 でありません"))?,
    )?;
    let page = doc.render_page(0)?;
    let pw = page.width.max(1.0);
    let ph = page.height.max(1.0);

    // 出力は常にペイン（表示領域）サイズ。ズームはビューポート矩形の大小で表現する。
    let (out_w, out_h) = available_area(backend.name());

    // 中心（未設定ならページ中央）からビューポートを計算（内部でページ内にクランプ）。
    let center = state.center.unwrap_or((pw / 2.0, ph / 2.0));
    let viewport = viewport_for(pw, ph, out_w, out_h, state.zoom, center);
    state.last_vw = viewport[2];
    state.last_vh = viewport[3];
    // クランプ後のビューポート中心を保存し、以降のパンが端で破綻しないようにする。
    state.center = Some((viewport[0] + viewport[2] / 2.0, viewport[1] + viewport[3] / 2.0));

    let rgba = renderer.render(&page, out_w, out_h, viewport)?;
    backend.display(&rgba, out_w, out_h)?;
    Ok(())
}

/// バックエンドの表示可能領域（ピクセル）を求める。
fn available_area(backend_name: &str) -> (u32, u32) {
    if backend_name.starts_with("framebuffer") {
        if let Some(sz) = read_fb_virtual_size() {
            return sz;
        }
    }
    // 端末のピクセルサイズ（取得できなければセル数から概算、最後は固定値）。
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.height > 0 {
            return (ws.width as u32, ws.height as u32);
        }
        if ws.columns > 0 && ws.rows > 0 {
            return (ws.columns as u32 * 8, ws.rows as u32 * 16);
        }
    }
    (1280, 800)
}

fn read_fb_virtual_size() -> Option<(u32, u32)> {
    let s = std::fs::read_to_string("/sys/class/graphics/fb0/virtual_size").ok()?;
    let (w, h) = s.trim().split_once(',')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// 表示するページ内ビューポート矩形 [x, y, w, h] を求める。zoom=100 でページ全体が
/// 表示領域に収まり（フィット）、zoom を上げるとビューポートが小さくなり中心 `center`
/// 周辺を拡大する。ビューポートのアスペクト比は出力（out_w/out_h）に一致させ歪みを防ぐ。
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

/// 1軸のビューポート原点を決める。ビューポートがページより小さい（拡大中）ときは原点を
/// ページ内に収めてページ外の白を見せない。大きい（フィット以下）ときは中央寄せ（letterbox）。
fn clamp_origin(center: f32, v: f32, p: f32) -> f32 {
    if v >= p {
        (p - v) / 2.0
    } else {
        (center - v / 2.0).clamp(0.0, p - v)
    }
}

#[cfg(test)]
mod tests {
    use super::viewport_for;

    #[test]
    fn fit_shows_whole_page_centered() {
        // 2:1 ページを 1000x1000 へ。フィット倍率は幅律速 s0=5、ビューポート = 200x200、
        // 中心(100,50) なので [0, -50, 200, 200]（上下に余白）。
        let vp = viewport_for(200.0, 100.0, 1000, 1000, 100, (100.0, 50.0));
        assert_eq!(vp, [0.0, -50.0, 200.0, 200.0]);
    }

    #[test]
    fn zoom_in_shrinks_viewport_around_center() {
        // 200% で正方ページ 100x100 を 1000x1000 に。s0=10, s=20, ビューポート=50x50、
        // 中心(50,50) → [25, 25, 50, 50]。
        let vp = viewport_for(100.0, 100.0, 1000, 1000, 200, (50.0, 50.0));
        assert_eq!(vp, [25.0, 25.0, 50.0, 50.0]);
    }

    #[test]
    fn viewport_clamped_inside_page_at_edge() {
        // 端へパンしても、拡大中はビューポートがページ外（白）へはみ出さない。
        // 100x100, 200%, ビューポート 50x50、中心を右下角(100,100)に → 原点は [50,50] でクランプ。
        let vp = viewport_for(100.0, 100.0, 1000, 1000, 200, (100.0, 100.0));
        assert_eq!(vp, [50.0, 50.0, 50.0, 50.0]);
    }
}
