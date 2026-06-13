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
use vecview_core::{Document, DrawCommand, OutputBackend, Page, PathSegment};
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

    /// スーパーサンプリング倍率（1..=4）。tmux 表示のシャープさと引き換えに転送量が増える。
    /// 未指定なら環境変数 VECVIEW_SCALE、それも無ければ 2。
    #[arg(short, long)]
    scale: Option<u32>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let scale = resolve_scale(args.scale);

    // 診断モード：VECVIEW_PROBE=1 で端末が報告するサイズを表示して終了する（解像度調査用）。
    if std::env::var_os("VECVIEW_PROBE").is_some() {
        probe_and_exit(args.backend.as_deref(), scale);
    }

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
    eprintln!(
        "操作: +/- ズーム  0 リセット  w 左右フィット  v 上下フィット  \
         n/Space/PgDn 次頁  p/PgUp 前頁  hjkl/矢印 パン  q 終了"
    );

    // ファイル監視（親ディレクトリを NonRecursive で監視し atomic rename を取りこぼさない）。
    // 対象ソースのページファイル以外の変更（temp_dir のノイズ等）は無視する。
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
        scale,
        pending_fit: None,
    };
    // 最後に描画した (ページ, mtime)。描画のたびに SVG を読むと atime が変わり notify が
    // 再発火する（自己トリガー）ため、同一ページで mtime 不変なら描画しない。
    let mut last_render: Option<(usize, SystemTime)> = None;

    // 初回描画（.typ は生成待ちのため存在しないことがある）。
    render_current(&source, &mut state, &renderer, backend.as_ref(), &mut last_render);

    while let Ok(first) = rx.recv() {
        // バーストをまとめて取り出す。連続する Reload は1回の描画に集約し、Quit/キーが
        // Reload の後ろに積まれても取りこぼさない（高頻度の再変換で反応不能・点滅になるのを防ぐ）。
        let mut msgs = vec![first];
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }

        let mut quit = false;
        let mut reload = false;
        let mut dirty = false; // キー操作などで再描画が必要。

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
                    match key_action(&k) {
                        Some(Action::Quit) => {
                            quit = true;
                            break;
                        }
                        Some(action) => {
                            apply_action(action, &source, &mut state);
                            dirty = true;
                        }
                        None => {}
                    }
                }
                Msg::Reload => reload = true,
            }
        }

        // Quit はバースト内の再変換・描画より優先（重い処理に入る前に抜ける）。
        if quit {
            break;
        }

        if reload {
            if matches!(source, Source::Pdf { .. }) {
                // PDF は元ファイルが変わったので全ページを再変換する（監視対象＝元 PDF、
                // 描画対象＝temp の SVG なので自己トリガーは起きない）。
                match source.reconvert() {
                    Ok(()) => {
                        let pc = source.page_count();
                        if state.page >= pc {
                            state.page = pc - 1;
                            state.center = None;
                        }
                        dirty = true;
                    }
                    // 書き込み途中の PDF を読むと一時的に失敗する。次の Reload で取り直す。
                    Err(e) => eprintln!("vecview: PDF 再変換エラー: {e:#}"),
                }
            } else {
                // SVG/Typst は描画が atime を変えて notify が再発火する（自己トリガー）。
                // 同一ページで mtime 不変なら描画しない。
                let path = source.page_path(state.page);
                let current = mtime_of(&path);
                let unchanged = current.is_none()
                    || (last_render.map(|(p, _)| p) == Some(state.page)
                        && current == last_render.map(|(_, m)| m));
                if !unchanged {
                    dirty = true;
                }
            }
        }

        if dirty {
            render_current(&source, &mut state, &renderer, backend.as_ref(), &mut last_render);
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
    /// スーパーサンプリング倍率（1..=4）。描画解像度に掛かる。
    scale: u32,
    /// 次回描画時に本文境界へフィットさせる要求（描画時に本文 bbox を見て zoom/center へ反映）。
    pending_fit: Option<Fit>,
}

/// 本文（ink）境界へのフィット方向。
#[derive(Clone, Copy)]
enum Fit {
    /// 本文の左右いっぱいに合わせる（横方向にフィット、縦ははみ出し可・パンで送る）。
    Width,
    /// 本文の上下いっぱいに合わせる（縦方向にフィット）。
    Height,
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
    /// 本文境界へフィット（左右/上下）。
    FitContent(Fit),
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
        // 本文フィット。w=左右いっぱい、v=上下いっぱい。
        KeyCode::Char('w') => Some(Action::FitContent(Fit::Width)),
        KeyCode::Char('v') => Some(Action::FitContent(Fit::Height)),
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
        // 本文 bbox は描画時にしか分からない（ページの SVG を読む必要がある）ため、要求だけ
        // 立てておき、render_and_display で zoom/center に反映する。
        Action::FitContent(fit) => state.pending_fit = Some(fit),
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
    let (out_w, out_h) = available_area(backend.name(), state.scale);

    // 本文フィット要求があれば、本文境界からズーム/中心を算出して通常のビューポート計算へ委譲。
    if let Some(fit) = state.pending_fit.take() {
        if let Some(bbox) = content_bbox(&page) {
            apply_fit(fit, bbox, pw, ph, out_w, out_h, state);
        }
    }

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

/// ラスタ化する解像度（ピクセル）を求める。
///
/// tmux プレースホルダ配置では端末がピクセル寸法を報告せず（width/height=0）、セル数 ×
/// 概算セルサイズ(8x16) に頼るしかない。実セルサイズが概算より大きい環境（HiDPI 等）では
/// この低解像度のまま端末側で引き伸ばされてボケる。そこでプレースホルダ時のみ高解像度で
/// ラスタ化し、端末側の縮小でシャープにする（スーパーサンプリング）。倍率は VECVIEW_SCALE
/// （`scale`、既定2、1..=4）。直接配置(a=T)やフレームバッファはネイティブ画素表示なので等倍に保つ。
fn available_area(backend_name: &str, scale: u32) -> (u32, u32) {
    if backend_name.starts_with("framebuffer") {
        if let Some(sz) = read_fb_virtual_size() {
            return sz;
        }
    }
    // 画像が cols×rows セルへ縮小配置されるプレースホルダ時のみ過剰描画してよい。
    let ss = if backend_name.contains("placeholder") {
        scale
    } else {
        1
    };
    // 端末のピクセルサイズ（取得できなければセル数から概算、最後は固定値）。
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.height > 0 {
            return (ws.width as u32, ws.height as u32);
        }
        if ws.columns > 0 && ws.rows > 0 {
            return (ws.columns as u32 * 8 * ss, ws.rows as u32 * 16 * ss);
        }
    }
    (1280 * ss, 800 * ss)
}

/// スーパーサンプリング倍率を決める。優先順位は CLI 引数 > 環境変数 VECVIEW_SCALE > 既定2。
/// いずれも 1..=4 にクランプする。
fn resolve_scale(arg: Option<u32>) -> u32 {
    arg.or_else(|| {
        std::env::var("VECVIEW_SCALE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
    })
    .unwrap_or(2)
    .clamp(1, 4)
}

/// ページ内の全パス頂点（制御点含む）から本文（ink）の外接矩形 [x, y, w, h] を求める。
/// pdftocairo の SVG は全面背景矩形を持たないため、これが実際の本文境界になる。可視パスが
/// 無ければ None。制御点を含むため厳密な曲線境界よりわずかに広いが、フィット用途では十分。
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

/// 本文境界 `bbox` に合わせて `state` の zoom/center を設定する。Width=左右いっぱい、
/// Height=上下いっぱい。zoom はフィット倍率(s0=100%)に対する比として求め、範囲にクランプ。
fn apply_fit(fit: Fit, bbox: [f32; 4], pw: f32, ph: f32, out_w: u32, out_h: u32, state: &mut ViewState) {
    let [bx, by, bw, bh] = bbox;
    let bw = bw.max(1.0);
    let bh = bh.max(1.0);
    let s0 = (out_w as f32 / pw).min(out_h as f32 / ph); // ページ全体フィット(=100%)の倍率。
    let s = match fit {
        Fit::Width => out_w as f32 / bw,
        Fit::Height => out_h as f32 / bh,
    };
    let zoom = ((s / s0) * 100.0).round();
    state.zoom = (zoom as i64).clamp(ZOOM_MIN as i64, ZOOM_MAX as i64) as u32;
    state.center = Some((bx + bw / 2.0, by + bh / 2.0));
}

/// 端末が報告するサイズと、そこから算出する描画解像度を表示して終了する（解像度調査用）。
fn probe_and_exit(backend: Option<&str>, scale: u32) -> ! {
    let b = detect_backend(backend);
    println!("backend            = {}", b.name());
    println!("scale (SS倍率)     = {scale}");
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
                println!("cell size(px)      = 不明（ピクセル値が0 → 8x16 概算に落ちる）");
            }
        }
        Err(e) => println!("window_size        = エラー: {e}"),
    }
    let (w, h) = available_area(b.name(), scale);
    println!("available_area(px) = {w} x {h}  ← この解像度でラスタ化している");
    std::process::exit(0);
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
    use super::{apply_fit, content_bbox, resolve_scale, viewport_for, Fit, ViewState};
    use vecview_core::{DrawCommand, Page, PathData, PathSegment};

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
        }
    }

    #[test]
    fn content_bbox_spans_all_vertices() {
        // 2本のパスの全頂点（制御点含む）を覆う外接矩形。
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
        // x:200..700, y:300..700 → [200,300, 500,400]。
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
        // ページ 1000x1000 を 1000x1000 出力へ（s0=1）。本文 [200,300,600,400]。
        // 左右フィット: s=out_w/bw=1000/600=1.667 → zoom=167%、中心=本文中心(500,500)。
        let mut s = state();
        apply_fit(Fit::Width, [200.0, 300.0, 600.0, 400.0], 1000.0, 1000.0, 1000, 1000, &mut s);
        assert_eq!(s.zoom, 167);
        assert_eq!(s.center, Some((500.0, 500.0)));
    }

    #[test]
    fn fit_height_fills_output_height() {
        // 上下フィット: s=out_h/bh=1000/400=2.5 → zoom=250%、中心=本文中心。
        let mut s = state();
        apply_fit(Fit::Height, [200.0, 300.0, 600.0, 400.0], 1000.0, 1000.0, 1000, 1000, &mut s);
        assert_eq!(s.zoom, 250);
        assert_eq!(s.center, Some((500.0, 500.0)));
    }

    #[test]
    fn scale_precedence_arg_over_env_with_clamp() {
        // 引数が最優先。範囲外は 1..=4 にクランプ。
        assert_eq!(resolve_scale(Some(3)), 3);
        assert_eq!(resolve_scale(Some(9)), 4);
        assert_eq!(resolve_scale(Some(0)), 1);
    }

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
