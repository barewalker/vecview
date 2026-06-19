//! vecview CLI エントリポイント。
//!
//! `vecview <FILE>` で SVG / Typst / PDF をターミナル内にベクター表示する。Typst（`.typ`）は
//! 内部で `typst watch` を起動して SVG を生成し、その SVG を監視してライブ再描画する。
//! PDF（`.pdf`）は `pdfium` で表示解像度に直接ラスタライズし、元 PDF を監視して保存のたびに
//! 開き直す。いずれもブラウザ不要・ターミナル内完結のプレビューを実現する。
//!
//! 端末（TTY）で起動するとインタラクティブモードになり、キーでズーム・ページ送り・終了できる。

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
    /// マウス入力（テキスト選択用）。
    Mouse(MouseEvent),
}

/// 表示ソース。SVG/Typst はページ SVG ファイル、PDF は pdfium で直接描画する。
#[derive(Clone)]
enum Source {
    /// 単一 SVG ファイル。
    Svg(PathBuf),
    /// Typst（`typst watch` が `vecview-<stem>-<tag>-<p>.svg` をページごとに出力）。
    /// `tag` はプロセス固有（PID）。同じ文書を複数インスタンスで開いても出力パスが衝突せず、
    /// 互いのファイルを取り合ったり消し合ったりしないようにする。
    Typst { dir: PathBuf, stem: String, tag: u32 },
    /// PDF（pdfium で直接ラスタライズ。ファイルは持たず、ドキュメントは main 側で保持）。
    /// 元 PDF を監視し、保存のたび開き直す。
    Pdf { pdf: PathBuf },
}

impl Source {
    /// ページ `idx`（0始まり）の SVG パス（SVG/Typst のみ。PDF はファイルベースでないため未使用）。
    fn page_path(&self, idx: usize) -> PathBuf {
        match self {
            Source::Svg(p) => p.clone(),
            Source::Typst { dir, stem, tag } => {
                dir.join(format!("vecview-{stem}-{tag}-{}.svg", idx + 1))
            }
            Source::Pdf { pdf } => pdf.clone(),
        }
    }

    /// SVG=1、Typst=連番ファイル数。PDF は pdfium のページ数を使うため [`current_page_count`] 側で扱う。
    fn page_count(&self) -> usize {
        match self {
            Source::Svg(_) | Source::Pdf { .. } => 1,
            Source::Typst { dir, stem, tag } => typst_page_count(dir, stem, *tag),
        }
    }

    /// 監視すべきディレクトリ。Typst は生成先、PDF/SVG は元ファイルのあるディレクトリ。
    fn watch_dir(&self) -> PathBuf {
        let base = match self {
            Source::Svg(p) => p.parent().map(Path::to_path_buf),
            Source::Typst { dir, .. } => Some(dir.clone()),
            Source::Pdf { pdf } => pdf.parent().map(Path::to_path_buf),
        };
        base.filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// 終了時に自インスタンスが生成した一時ファイルを掃除する（Typst のみ）。PID 付きパスなので
    /// 他インスタンス（同じ文書を開いている別プロセス含む）のファイルは消さない。
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

    /// 変更パスがこのソースの監視対象（ページファイル、または元 PDF）か。
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

/// Typst の現在ページ数＝連番ページ SVG の存在数。mtime での判定はしない（コンパイル中の
/// ページ書き込みの一瞬を踏むと現行ページを古いと誤判定してページ数が落ち、表示が消える競合が
/// 起きるため）。版縮小で取り残された古いページの除去は、コンパイル完了後に
/// [`prune_stale_typst_pages`] が安全に削除する。
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

/// 版が縮んで取り残された自インスタンスの末尾ページ SVG を削除する。typst は1コンパイルで現行
/// ページを十数 ms 内に全て書くため、現行ページは最新 mtime の密なクラスタになる。末尾から見て、
/// 最新 mtime より明確に古い（`margin` 超）連続ぶんだけを取り残しとみなして消す。コンパイル完了後
/// （debounce 済み Reload 時）に呼ぶ前提なので、書き込み途中の現行ページを誤って消さない。
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
    // 末尾（高ページ番号）から、latest より margin 超古いファイルだけ削除し、新しいクラスタに
    // 達したら止める（先頭側の現行ページには触れない）。取り残しは常に末尾の連続ぶん。
    for (path, t) in entries.iter().rev() {
        let stale = latest.duration_since(*t).map(|d| d > margin).unwrap_or(false);
        if !stale {
            break;
        }
        let _ = std::fs::remove_file(path);
    }
}

/// 現在のページ数。PDF は開いている pdfium ドキュメントの値、SVG/Typst は [`Source::page_count`]。
fn current_page_count(source: &Source, pdf: Option<&vecview_pdf::Pdf>) -> usize {
    match source {
        Source::Pdf { .. } => pdf.map(|p| p.page_count()).unwrap_or(1),
        other => other.page_count(),
    }
}

#[derive(Parser, Debug)]
#[command(name = "vv", version, about = "vecview - ベクターグラフィクスをターミナルに表示する")]
struct Args {
    /// 表示するファイル（SVG / Typst .typ / PDF）。
    file: PathBuf,

    /// 初期ズーム倍率（%）。
    #[arg(short, long, default_value_t = 100)]
    zoom: u32,

    /// 出力バックエンド強制指定 [kitty|tmux|sixel|framebuffer]。環境変数 VECVIEW_BACKEND でも可。
    #[arg(short, long)]
    backend: Option<String>,

    /// スーパーサンプリング倍率（1..=4）。tmux 表示のシャープさと引き換えに転送量が倍率²で増え、
    /// 連続操作で端末が画像更新を捌けず落ちることがある。未指定なら環境変数 VECVIEW_SCALE、
    /// それも無ければ 1（等倍）。端末が耐えるなら 2 以上でシャープに。
    #[arg(short, long)]
    scale: Option<u32>,

    /// ヘッドレス描画モード。対話せず1ページを PNG に描いて終了する（yazi / nvim 連携の土台）。
    /// `--size` 必須。出力は `--output`（既定は stdout）。
    #[arg(long)]
    render: bool,

    /// ヘッドレス描画の出力ピクセルサイズ `幅x高`（例 `800x1000`）。`--render` 時に必須。
    #[arg(long)]
    size: Option<String>,

    /// ヘッドレス描画の出力先。PNG パス、または `-` で stdout。`--render` 時のみ有効（既定 `-`）。
    #[arg(short, long)]
    output: Option<String>,

    /// 描画するページ（1始まり）。`--render` 時のみ有効（既定 1）。
    #[arg(long, default_value_t = 1)]
    page: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let scale = resolve_scale(args.scale);
    // 出力バックエンドは CLI 引数 > 環境変数 VECVIEW_BACKEND（未指定なら自動検出）。
    let backend_choice = args
        .backend
        .clone()
        .or_else(|| std::env::var("VECVIEW_BACKEND").ok());

    // 診断モード：VECVIEW_PROBE=1 で端末が報告するサイズを表示して終了する（解像度調査用）。
    if std::env::var_os("VECVIEW_PROBE").is_some() {
        probe_and_exit(backend_choice.as_deref(), scale);
    }

    if !args.file.exists() {
        bail!("ファイルが見つかりません: {}", args.file.display());
    }

    // ヘッドレス描画モード（--render）：端末も対話もなしで1ページを PNG に描いて即終了する。
    // yazi の previewer や nvim プラグインが「指定サイズの画像を1枚作る」ために使う土台。
    if args.render {
        return render_headless(&args);
    }

    let ext = args
        .file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // PDF は pdfium で直接描画する（pdftocairo→SVG→usvg 経路はネスト <use> の transform
    // 二重適用バグで図がずれるため）。開いたドキュメントはここで保持する。
    let mut pdf_doc: Option<vecview_pdf::Pdf> = None;

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
        "pdf" => {
            let canonical = std::fs::canonicalize(&args.file).unwrap_or_else(|_| args.file.clone());
            pdf_doc = Some(vecview_pdf::Pdf::open(&canonical).context("PDF を開けません")?);
            (Source::Pdf { pdf: canonical }, None)
        }
        other => bail!("未対応の拡張子です: .{other}（svg / typ / pdf のみ対応）"),
    };

    let backend = detect_backend(backend_choice.as_deref());
    // GPU レンダラーは SVG/Typst のベクター描画にのみ使う。PDF は pdfium が描画するので初期化しない。
    let renderer = if matches!(source, Source::Pdf { .. }) {
        None
    } else {
        Some(Renderer::new().context("レンダラー初期化")?)
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
    // キーバインドを設定ファイル＋既定値から構築する。
    let keymap = Keymap::load();
    let help_key = keymap
        .help
        .iter()
        .find(|(n, _)| *n == "help")
        .and_then(|(_, k)| k.first().cloned())
        .unwrap_or_else(|| "?".to_string());
    eprintln!("操作: {help_key} でヘルプ表示（キーは {} で変更可）", config_path().map(|p| p.display().to_string()).unwrap_or_default());

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
        // テキスト選択のためマウスレポートを有効化する（終了時に無効化）。
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
        help: false,
        copy: None,
        last_viewport: None,
        status: None,
    };
    // 最後に描画した (ページ, mtime)。描画のたびに SVG を読むと atime が変わり notify が
    // 再発火する（自己トリガー）ため、同一ページで mtime 不変なら描画しない。
    let mut last_render: Option<(usize, SystemTime)> = None;
    // copy mode のキャレット移動で使い回す、選択オーバーレイ適用前のベース画像。
    let mut base_frame: Option<BaseFrame> = None;

    // 初回描画（.typ は生成待ちのため存在しないことがある）。
    render_current(
        &source,
        pdf_doc.as_ref(),
        &mut state,
        renderer.as_ref(),
        backend.as_ref(),
        &mut last_render,
        &mut base_frame,
    );

    // tmux passthrough sixel は tmux に消されるため、入力待ちをタイムアウトさせて定期的に
    // 直近フレームを再送し復元する。間隔は VECVIEW_REDRAW_MS（既定 1000ms、最小 100ms）。
    let refresh = if backend.wants_periodic_redraw() {
        Some(Duration::from_millis(redraw_interval_ms()))
    } else {
        None
    };

    // 画像転送のレート制限。キー長押し（ズーム/パン/キャレット）で毎フレーム大きな画像を
    // 端末へ流すと、生成側が消費側（特に tmux 経由の端末）を追い越して入力バッファが溢れ、
    // 端末ごとクラッシュする。連続入力中は最大 1/MIN_FRAME に間引き、入力が止んだら最終状態を
    // 1回描く（デバウンス）。間隔は VECVIEW_MIN_FRAME_MS（既定 80ms ≒ 12fps、最小 16ms）。
    // tmux passthrough 経由（placeholder / sixel）は転送が重く端末側に画像が溜まりやすいので
    // 既定をより保守的に。直接配置はネイティブで軽いため小さめでよい。
    let min_frame_default = if backend.name().contains("placeholder") || backend.name().contains("tmux") {
        200
    } else {
        80
    };
    let min_frame = Duration::from_millis(min_frame_ms(min_frame_default));
    let mut last_draw: Option<Instant> = None;
    let mut pending_full = false; // フル再描画（GPU 再ラスタライズ）が保留中。
    let mut pending_overlay = false; // overlay 軽量再描画が保留中。
    let mut last_sixel = Instant::now(); // sixel 定期再描画の前回時刻。

    // tmux placeholder kitty は別ウィンドウへ切り替えると、端末が画像を tmux のウィンドウ単位で
    // クリップしないため、画像が前面ウィンドウのペインに残ってしまう。自ウィンドウが可視かを
    // 定期ポーリングし、隠れたら画像を消し、戻ったら再描画する。$TMUX_PANE の window_active で判定。
    let kitty_ph = backend.name().contains("placeholder");
    let vis_pane = std::env::var("TMUX_PANE").ok();
    let vis_poll = Duration::from_millis(250);
    let mut last_vis_poll = Instant::now();
    let mut was_visible = true;

    loop {
        // 次の入力待ち時間：sixel 定期再描画・可視ポーリング・間引き中の保留描画の締切のうち最短。
        let wait = {
            let mut w = refresh;
            if pending_full || pending_overlay {
                let remaining = last_draw
                    .map(|t| min_frame.saturating_sub(Instant::now().duration_since(t)))
                    .unwrap_or(Duration::ZERO);
                w = Some(w.map_or(remaining, |x| x.min(remaining)));
            }
            if kitty_ph && vis_pane.is_some() {
                let remaining = vis_poll.saturating_sub(Instant::now().duration_since(last_vis_poll));
                w = Some(w.map_or(remaining, |x| x.min(remaining)));
            }
            w
        };

        // 入力を受信（待ち時間ありならタイムアウト）。タイムアウト時は msgs 空で描画判定へ進む。
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
        let mut dirty = false; // キー操作などで通常表示のフル再描画（GPU 再ラスタライズ）が必要。
        let mut overlay_only = false; // copy mode のキャレット/選択のみ変化＝ベース画像を使い回す軽量再描画。
        let mut help_changed = false; // ヘルプの表示/非表示が切り替わった。

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
                    // copy mode 中はキーをそちらで消費する（通常ナビと衝突させない）。
                    if let Some(mut cm) = state.copy.take() {
                        match handle_copy_key(&k, &mut cm) {
                            CopyOutcome::Yank => {
                                let text = cm.selected_text();
                                let n = text.chars().count();
                                copy_to_clipboard(&text);
                                state.status = Some(format!("copied {n} chars"));
                                overlay_only = true; // cm は戻さない＝抜ける。view 不変なので軽量再描画。
                            }
                            CopyOutcome::Exit => overlay_only = true,
                            CopyOutcome::Redraw => {
                                state.copy = Some(cm);
                                overlay_only = true; // キャレット/選択のみ変化＝ GPU 再描画しない。
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
                        // ヘルプ表示中はどのキーでも閉じる（その操作は消費する）。
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
                        Some(action) => {
                            apply_action(action, pages, &mut state);
                            dirty = true;
                        }
                        None => {}
                    }
                }
                Msg::Mouse(m) => {
                    match m.kind {
                        // 押下: copy mode でなければテキスト層を作って入り、最寄り文字にアンカー。
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
                        // ドラッグ: キャレットを伸ばす（アンカーは保持）。
                        MouseEventKind::Drag(MouseButton::Left) => {
                            if let Some(idx) = mouse_to_glyph(&state, m.column, m.row) {
                                if let Some(cm) = state.copy.as_mut() {
                                    cm.cursor = idx;
                                    overlay_only = true; // 選択のみ変化＝軽量再描画。
                                }
                            }
                        }
                        // 離す: 実際にドラッグした（範囲がある）ならコピーして抜ける。
                        // 単クリックはキャレットを置くだけで継続（その後キーボードで選択可）。
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
                                overlay_only = true; // view 不変＝軽量再描画。
                            }
                        }
                        // ホイール: ページめくり（下=次ページ、上=前ページ）。実際にページが
                        // 変わったときだけ再描画する。copy mode 中はグリフ座標が陳腐化するので抜ける。
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
            }
        }

        // Quit はバースト内の再変換・描画より優先（重い処理に入る前に抜ける）。
        if quit {
            break;
        }

        if reload {
            if let Source::Pdf { pdf } = &source {
                // 元 PDF が変わったので開き直す（監視対象＝元 PDF、描画は pdfium がメモリ上の
                // ドキュメントから行うので自己トリガーは起きない）。
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
                    // 書き込み途中の PDF を開くと一時的に失敗する。次の Reload で取り直す。
                    Err(e) => eprintln!("vecview: PDF 再オープンエラー: {e:#}"),
                }
            } else {
                // コンパイル完了後（debounce 済み）なので、版縮小で取り残された古い末尾ページを
                // ここで安全に削除する（書き込み途中ではないため現行ページは消さない）。
                if let Source::Typst { dir, stem, tag } = &source {
                    prune_stale_typst_pages(dir, stem, *tag);
                }
                // 版が縮んでページ番号が現行ページ数を超えていたらクランプ（PDF 側と同様）。
                let pc = current_page_count(&source, pdf_doc.as_ref());
                if state.page >= pc {
                    state.page = pc - 1;
                    state.center = None;
                }
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
            // 再読込で内容が変わったら copy mode のグリフ座標が陳腐化するので抜ける。
            if dirty {
                state.copy = None;
            }
        }

        // 今イテレーションで必要になった描画を保留フラグへ畳み込む（実描画はレート制限つきで後段）。
        if dirty {
            pending_full = true;
        }
        if overlay_only {
            pending_overlay = true;
        }
        // ヘルプを閉じた直後は画像を描き直す必要がある。
        if help_changed && !state.help {
            pending_full = true;
        }

        // 描画判定。ヘルプ表示中は画像描画を抑止し（点滅防止。閉じれば最新が出る）、
        // 画像描画は min_frame で間引いて端末を追い越さないようにする（フラッド＝クラッシュ防止）。
        if state.help {
            if help_changed {
                draw_help(backend.as_ref(), &keymap);
            }
            // 表示中は溜めない（閉じた時に help_changed で再セットされる）。
            pending_full = false;
            pending_overlay = false;
        } else if (pending_full || pending_overlay) && (!kitty_ph || was_visible) {
            // 自ウィンドウが隠れている間は描かない（前面ウィンドウへ画像が残るのを防ぐ）。
            // 保留は残し、戻って可視になったときに描く。
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
                    // copy mode の軽量再描画。キャッシュが無ければフル描画にフォールバック。
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
                // copy mode のステータス/操作ヒントを最下行に出す。
                draw_overlay_text(backend.as_ref(), &mut state);
                last_draw = Some(now);
                pending_full = false;
                pending_overlay = false;
            }
            // can_draw でなければ保留のまま。wait が締切で起こすので次イテレーションで描く。
        }

        // sixel 定期再描画（tmux に消された画像の復元）。描画レートとは独立に refresh 間隔で。
        if let Some(r) = refresh {
            let now = Instant::now();
            if !state.help && now.duration_since(last_sixel) >= r {
                let _ = backend.redraw();
                last_sixel = now;
            }
        }

        // 可視ポーリング（tmux placeholder のウィンドウ間残留対策）。隠れたら画像を消し、戻ったら再描画。
        if kitty_ph {
            if let Some(pane) = vis_pane.as_deref() {
                let now = Instant::now();
                if now.duration_since(last_vis_poll) >= vis_poll {
                    last_vis_poll = now;
                    if let Some(visible) = pane_window_active(pane) {
                        if !visible && was_visible {
                            // 別ウィンドウへ切替：転送済み画像を消して前面への残留を止める。
                            let _ = backend.clear();
                            was_visible = false;
                        } else if visible && !was_visible {
                            // 戻ってきた：再描画して画像を復元する。
                            was_visible = true;
                            pending_full = true;
                        }
                    }
                }
            }
        }
    }

    // 後始末：マウスキャプチャ無効化 + raw mode 解除 + 端末復帰 + typst 子プロセス停止。
    if interactive {
        crossterm::execute!(std::io::stdout(), DisableMouseCapture).ok();
        crossterm::terminal::disable_raw_mode().ok();
    }
    backend.leave().ok();
    if let Some(child) = child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // 自インスタンスが /tmp に出した一時ファイルを掃除（PID 付きなので他インスタンスは無傷）。
    source.cleanup();

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
    /// ヘルプ（ショートカット一覧）を表示中か。
    help: bool,
    /// テキスト選択（copy mode）。None なら通常表示。
    copy: Option<CopyMode>,
    /// 直近に描画したページ内ビューポート [x, y, w, h]。マウス座標→ページ座標の逆変換に使う。
    last_viewport: Option<[f32; 4]>,
    /// 直近のコピー結果メッセージ（ステータス行に1回だけ出す）。
    status: Option<String>,
}

/// テキスト選択（copy mode）の状態。`glyphs` は現ページのテキスト層（読み順）、
/// `lines` は可視行ごとのグリフ添字（各行は x 昇順）、`line_of` はグリフ→行番号。
/// `cursor` はキャレット位置（グリフ添字）、`anchor` は選択開始（None なら未選択）。
struct CopyMode {
    glyphs: Vec<vecview_pdf::Glyph>,
    lines: Vec<Vec<usize>>,
    line_of: Vec<usize>,
    cursor: usize,
    anchor: Option<usize>,
}

impl CopyMode {
    /// テキスト層からグリフを可視行へグルーピングして copy mode を作る。glyphs が空なら None。
    /// 制御文字（pdfium が行間に挟む `\r`/`\n` 等。矩形を持たない）は除外し、改行は行ジオメトリから
    /// 再構成する（[`selected_text`] が行変化に `\n` を挿入）。空白は語間に必要なので残す。
    fn new(glyphs: Vec<vecview_pdf::Glyph>) -> Option<Self> {
        let glyphs: Vec<vecview_pdf::Glyph> =
            glyphs.into_iter().filter(|g| !g.ch.is_control()).collect();
        if glyphs.is_empty() {
            return None;
        }
        // 読み順のまま、y中心が前行から半文字以上ずれたら改行とみなして行へ束ねる。
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

    /// グリフ中心 x。j/k の桁保持に使う。
    fn center_x(&self, idx: usize) -> f32 {
        let r = self.glyphs[idx].rect;
        r[0] + r[2] / 2.0
    }

    /// 上下の行へ、現在の x になるべく近いグリフへ移動する。
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

    /// 現在行の先頭/末尾グリフ。
    fn line_edge(&mut self, end: bool) {
        let line = &self.lines[self.line_of[self.cursor]];
        if let Some(&i) = if end { line.last() } else { line.first() } {
            self.cursor = i;
        }
    }

    /// 選択範囲 [start, end]（読み順, 両端含む）。未選択ならキャレット1文字。
    fn range(&self) -> (usize, usize) {
        match self.anchor {
            Some(a) => (a.min(self.cursor), a.max(self.cursor)),
            None => (self.cursor, self.cursor),
        }
    }

    /// 選択テキスト。行が変わる箇所に改行を挿入して連結する。
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

/// 本文（ink）境界へのフィット方向。
#[derive(Clone, Copy)]
enum Fit {
    /// 本文の左右いっぱいに合わせる（横方向にフィット、縦ははみ出し可・パンで送る）。
    Width,
    /// 本文の上下いっぱいに合わせる（縦方向にフィット）。
    Height,
}

/// キー操作。
#[derive(Clone, Copy)]
enum Action {
    Quit,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// ビューポートを (dx, dy) 方向に移動（符号のみ。量は last_vw/vh から算出）。
    Pan(f32, f32),
    NextPage,
    PrevPage,
    FirstPage,
    LastPage,
    /// 本文境界へフィット（左右/上下）。
    FitContent(Fit),
    /// ショートカット一覧の表示/非表示を切り替える。
    ToggleHelp,
    /// テキスト選択（copy mode）へ入る。
    EnterCopyMode,
}

/// ズーム倍率（%）。最小はフィット(100)、最大は16倍。
const ZOOM_MIN: u32 = 100;
const ZOOM_MAX: u32 = 1600;

/// 設定可能なアクションの一覧: (設定名, アクション, 既定キー)。設定名は config.toml の
/// `[keys]` のキー、ヘルプ表示順もこの順。既定値:
/// 拡大時の上下左右移動=矢印、ページ送り/戻り=j/k、先頭/最終ページ=h/l。
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
    ("help", Action::ToggleHelp, &["?"]),
    ("quit", Action::Quit, &["q", "esc", "ctrl+c"]),
];

/// キー（コード＋Ctrl 有無）→アクションの対応表と、ヘルプ表示用の有効バインドを保持する。
struct Keymap {
    lookup: std::collections::HashMap<(KeyCode, bool), Action>,
    /// (設定名, 実際のキー文字列) を ACTIONS 順に。ヘルプ表示・設定リファレンス用。
    help: Vec<(&'static str, Vec<String>)>,
}

impl Keymap {
    /// 既定値＋設定ファイルの上書きからキーマップを構築する。
    fn load() -> Self {
        Self::build(&read_key_overrides())
    }

    /// 既定値に `overrides`（設定名→キー文字列）を被せてキーマップを構築する。
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
                    None => eprintln!("vecview: 不明なキー指定 {spec:?}（{name}）"),
                }
            }
            help.push((*name, keys));
        }
        Keymap { lookup, help }
    }

    /// キーイベントに対応するアクションを返す。
    fn action(&self, k: &KeyEvent) -> Option<Action> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        self.lookup.get(&(k.code, ctrl)).copied()
    }
}

/// 設定ファイル `[keys]` から「設定名→キー文字列の並び」を読む。存在/解析失敗時は空（既定のみ）。
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
            eprintln!("vecview: 設定ファイル解析エラー ({}): {e}", path.display());
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

/// 設定ファイルのパス。優先順位: 環境変数 VECVIEW_CONFIG > $XDG_CONFIG_HOME/vecview/config.toml
/// > ~/.config/vecview/config.toml。
fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VECVIEW_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("vecview").join("config.toml"))
}

/// キー指定文字列を (KeyCode, Ctrl有無) に解析する。例: "q", "+", "left", "space", "ctrl+c"。
/// 名前付きキーは大小無視、1文字キーは記号・大小をそのまま使う。
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
            // 1文字キー（記号・英数字）。元の大小・記号をそのまま使う。
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
        // 本文 bbox は描画時にしか分からない（ページを読む必要がある）ため、要求だけ立てておき、
        // 描画時（render_pdf / render_and_display）に zoom/center へ反映する。
        Action::FitContent(fit) => state.pending_fit = Some(fit),
        // ヘルプ・copy mode はメインループ側で扱う（描画/入力経路が通常表示と異なるため）。
        Action::ToggleHelp => {}
        Action::EnterCopyMode => {}
    }
}

/// 現在のキーマップからショートカット一覧（ヘルプ表示）を生成する。設定名はそのまま
/// config.toml の `[keys]` のキーになるので、設定リファレンスも兼ねる。
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
    lines.push("    [keys] に  <action> = [\"key\", ...]  で再割り当て可".to_string());
    lines.push("  press any key to close".to_string());
    lines
}

/// ヘルプ画面を描く。画像を消してテキストを左上に並べる。
fn draw_help(backend: &dyn OutputBackend, keymap: &Keymap) {
    use std::io::Write;
    let _ = backend.clear();
    let mut out = std::io::stdout().lock();
    for (i, line) in help_lines(keymap).iter().enumerate() {
        // 行頭（ペイン相対）へ移動して出力。raw mode のため明示的に桁も指定する。
        let _ = write!(out, "\x1b[{};3H{line}", i + 2);
    }
    let _ = out.flush();
}

/// copy mode のキー1打の結果。
enum CopyOutcome {
    /// 再描画する（カーソル/選択が動いた）。
    Redraw,
    /// 選択をコピーして copy mode を抜ける。
    Yank,
    /// コピーせず copy mode を抜ける。
    Exit,
    /// 何もしない（未割り当てキー）。
    Ignore,
}

/// copy mode 中のキー処理。移動は vim/tmux 風（hjkl/矢印, 0/$, g/G）、space で選択開始、
/// enter/y でヤンク、esc/q で取り消し。
fn handle_copy_key(k: &KeyEvent, cm: &mut CopyMode) -> CopyOutcome {
    // raw mode では Ctrl-C はキーイベントになる。copy mode に閉じ込めないよう抜ける。
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return CopyOutcome::Exit;
    }
    let last = cm.glyphs.len() - 1;
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => CopyOutcome::Exit,
        KeyCode::Enter | KeyCode::Char('y') => CopyOutcome::Yank,
        // 選択開始/解除のトグル。
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

/// copy mode 突入時にテキスト層（読み順のグリフ）を作る。PDF は開いているドキュメント、
/// Typst は表示用 SVG と pt 寸法が一致する PDF を一時生成して読む。単体 SVG はテキスト層なし。
fn build_text_layer(
    source: &Source,
    typ_path: &Path,
    pdf: Option<&vecview_pdf::Pdf>,
    page: usize,
) -> Result<Vec<vecview_pdf::Glyph>> {
    match source {
        Source::Pdf { .. } => {
            let doc = pdf.ok_or_else(|| anyhow!("PDF が開かれていません"))?;
            doc.page_text(page)
        }
        Source::Typst { dir, stem, tag } => {
            // Typst SVG はグリフをパス化していて文字を持たないため、同じ .typ を PDF にも
            // コンパイルし、その文字＋座標を使う（pt 寸法は SVG と一致）。PID 付きで他インスタンスと衝突回避。
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
                .context("typst compile（テキスト用 PDF）の起動に失敗")?
                .success();
            if !ok {
                bail!("typst compile（テキスト用 PDF）が失敗しました");
            }
            let doc = vecview_pdf::Pdf::open(&pdf_path).context("テキスト用 PDF を開けません")?;
            let mut glyphs = doc.page_text(page)?;
            // 表示は SVG（usvg）で、usvg は SVG の pt 指定を 96dpi で px に正規化するため、表示・
            // ビューポートは「pt × 96/72」の単位になる。一方グリフ矩形は PDF の pt のまま。両者の
            // ページ寸法比で同じ単位へスケールしないと、選択ハイライトが下/右へ行くほどズレる。
            // 比はレンダラが実際に使う SVG 寸法と PDF pt 寸法から直接取る（usvg の DPI に非依存）。
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
        // 単体 SVG（Typst 由来含む）は <text> を持たないことが多く、テキスト層は作れない。
        Source::Svg(_) => Ok(Vec::new()),
    }
}

/// マウスのセル座標 (col,row) を最寄りグリフの添字へ変換する。直近のビューポートと端末セル数から
/// ページ座標を推定し、中心距離が最小のグリフを返す。
fn mouse_to_glyph(state: &ViewState, col: u16, row: u16) -> Option<usize> {
    let cm = state.copy.as_ref()?;
    let [vx, vy, vw, vh] = state.last_viewport?;
    let (cols, rows) = crossterm::terminal::size().ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    // セル中心の割合 → ページ座標。画像はペイン全体（cols×rows）を覆う前提。
    let px = vx + (col as f32 + 0.5) / cols as f32 * vw;
    let py = vy + (row as f32 + 0.5) / rows as f32 * vh;
    cm.glyphs
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| glyph_dist2(a.rect, px, py).total_cmp(&glyph_dist2(b.rect, px, py)))
        .map(|(i, _)| i)
}

/// 点 (px,py) からグリフ矩形中心までの距離の2乗。
fn glyph_dist2(r: [f32; 4], px: f32, py: f32) -> f32 {
    let cx = r[0] + r[2] / 2.0;
    let cy = r[1] + r[3] / 2.0;
    (cx - px) * (cx - px) + (cy - py) * (cy - py)
}

/// 選択テキストを OSC 52 でクリップボードへ送る。tmux 内では passthrough でラップする。
/// X11/Wayland 非依存・SSH 越しでもホスト側クリップボードに入る。
fn copy_to_clipboard(text: &str) {
    use base64::Engine;
    use std::io::Write;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{b64}\x07");
    let mut out = std::io::stdout().lock();
    if std::env::var_os("TMUX").is_some() {
        // tmux passthrough: 内側の ESC を二重化し \ePtmux;...\e\\ で包む。
        let inner = seq.replace('\x1b', "\x1b\x1b");
        let _ = write!(out, "\x1bPtmux;{inner}\x1b\\");
    } else {
        let _ = write!(out, "{seq}");
    }
    let _ = out.flush();
}

/// 選択ハイライトとキャレットを RGBA 画像へ直接ブレンドする。ページ座標→出力ピクセルは
/// viewport の逆変換で求める。
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
                continue; // 改行など矩形のない文字。
            }
            let x0 = (r[0] - vx) * sx;
            let y0 = (r[1] - vy) * sy;
            let x1 = (r[0] + r[2] - vx) * sx;
            let y1 = (r[1] + r[3] - vy) * sy;
            blend_rect(rgba, out_w, out_h, [x0, y0, x1, y1], [40, 120, 255], 0.38);
        }
    }
    // キャレット（縦線）。
    let cr = cm.glyphs[cm.cursor].rect;
    let cx = (cr[0] - vx) * sx;
    let cy0 = (cr[1] - vy) * sy;
    let ch = cr[3].max(8.0) * sy;
    let cw = (sx * 1.5).max(2.0);
    blend_rect(rgba, out_w, out_h, [cx, cy0, cx + cw, cy0 + ch], [255, 40, 40], 0.9);
}

/// 矩形 [x0,y0,x1,y1]（出力ピクセル）に `color` を係数 `a` でアルファブレンドする。
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

/// copy mode のステータス（直近のコピー結果）または操作ヒントを最下行に出す。
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

/// copy mode のキャレット移動時に GPU 再ラスタライズを避けるためのベース画像キャッシュ
/// （選択オーバーレイ適用前）。キーがすべて copy mode に消費され view が変わらない copy mode
/// 中だけ使い回す。複雑な図を毎キー再描画すると GPU を酷使し端末ごと巻き込んで落ちうるため。
struct BaseFrame {
    out_w: u32,
    out_h: u32,
    viewport: [f32; 4],
    rgba: Vec<u8>,
}

/// copy mode のキャレット/選択のみ変化したときの軽量再描画。直近のベース画像を使い回し、
/// 選択オーバーレイだけ重ねて表示する（GPU 再描画・SVG 再読込なし）。キャッシュが無ければ
/// false を返し、呼び出し側はフル描画にフォールバックする。
fn redraw_overlay(backend: &dyn OutputBackend, state: &ViewState, base: &Option<BaseFrame>) -> bool {
    let Some(bf) = base else { return false };
    let mut rgba = bf.rgba.clone();
    if let Some(cm) = &state.copy {
        overlay_selection(&mut rgba, bf.out_w, bf.out_h, bf.viewport, cm);
    }
    backend.display(&rgba, bf.out_w, bf.out_h).is_ok()
}

/// 現在ページを描画して表示する。PDF は pdfium、SVG/Typst は GPU レンダラー。
/// 失敗（ファイル未生成等）時は静かにスキップ。
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
                eprintln!("vecview: 描画エラー: {e:#}");
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
                Err(e) => eprintln!("vecview: 描画エラー: {e:#}"),
            }
        }
    }
}

/// PDF の現在ページを、ズーム/パン状態のビューポートで pdfium にラスタライズさせ表示する。
fn render_pdf(
    pdf: &vecview_pdf::Pdf,
    backend: &dyn OutputBackend,
    state: &mut ViewState,
    base: &mut Option<BaseFrame>,
) -> Result<()> {
    let (pw, ph) = pdf.page_size(state.page)?;
    let (pw, ph) = (pw.max(1.0), ph.max(1.0));

    // 出力は常にペイン（表示領域）サイズ。ズームはビューポート矩形の大小で表現する。
    let (out_w, out_h) = available_area(backend.name(), state.scale);

    // 本文フィット要求があれば、本文境界からズーム/中心を算出して通常のビューポート計算へ委譲。
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
    // pdfium はビットマップ全体（ページ範囲外の letterbox 含む）を白の clear_color で塗るため、
    // 縦長ページを横長ペインへフィットすると左右が白帯になり、ページの白と見分けがつかず「余分な
    // 左右余白」に見える。SVG/Typst レンダラと同じ暗色でページ外を塗り直し、ページ境界を可視にする。
    fill_letterbox(&mut rgba, out_w, out_h, viewport, pw, ph);
    // copy mode 中はオーバーレイ適用前のベースをキャッシュ（以降のキャレット移動で使い回す）。
    if state.copy.is_some() {
        *base = Some(BaseFrame { out_w, out_h, viewport, rgba: rgba.clone() });
    }
    if let Some(cm) = &state.copy {
        overlay_selection(&mut rgba, out_w, out_h, viewport, cm);
    }
    backend.display(&rgba, out_w, out_h)?;
    Ok(())
}

/// ページ範囲外（letterbox）を暗色で塗る。pdfium はページ範囲外も clear_color の白で塗ってしまう
/// ため、SVG レンダラ（`LETTERBOX` = 0.10 グレー相当）に合わせてページ境界を見えるようにする。
/// ページ矩形 [0,0,pw,ph]（pt）をビューポート→出力ピクセルへ順変換し、その外側だけ塗り替える。
/// 拡大中（ページが出力を覆う）は矩形が出力全体を含むので何もしない。
fn fill_letterbox(rgba: &mut [u8], out_w: u32, out_h: u32, viewport: [f32; 4], pw: f32, ph: f32) {
    let [vx, vy, vw, vh] = viewport;
    let sx = out_w as f32 / vw.max(1.0);
    let sy = out_h as f32 / vh.max(1.0);
    let x0 = ((0.0 - vx) * sx).round();
    let y0 = ((0.0 - vy) * sy).round();
    let x1 = ((pw - vx) * sx).round();
    let y1 = ((ph - vy) * sy).round();
    const C: u8 = 26; // LETTERBOX(0.10) を 8bit へ（0.10*255≒26）。
    for y in 0..out_h {
        let inside_y = (y as f32) >= y0 && (y as f32) < y1;
        for x in 0..out_w {
            if inside_y && (x as f32) >= x0 && (x as f32) < x1 {
                continue; // ページ内はそのまま。
            }
            let idx = ((y * out_w + x) * 4) as usize;
            rgba[idx] = C;
            rgba[idx + 1] = C;
            rgba[idx + 2] = C;
            rgba[idx + 3] = 255;
        }
    }
}

/// ヘッドレス描画（`--render`）：端末も対話もなしで1ページを RGBA へ描き、PNG として出力する。
/// 描画経路は対話モードと同じ（PDF=pdfium、SVG/Typst=wgpu）で、出力は `--size` の実ピクセル。
fn render_headless(args: &Args) -> Result<()> {
    let (out_w, out_h) = match args.size.as_deref() {
        Some(s) => parse_size(s)
            .ok_or_else(|| anyhow!("--size の形式が不正です（例: 800x1000）: {s}"))?,
        None => bail!("--render には --size が必要です（例: --size 800x1000）"),
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
            let renderer = Renderer::new().context("レンダラー初期化")?;
            render_svg_file(&args.file, &renderer, out_w, out_h, zoom)?
        }
        "typ" => render_typ_headless(&args.file, page_idx, out_w, out_h, zoom)?,
        other => bail!("未対応の拡張子です: .{other}（svg / typ / pdf のみ対応）"),
    };
    write_png(&rgba, out_w, out_h, output)
}

/// ヘッドレスで PDF の1ページをビューポート（ページ中央・指定ズーム）でラスタライズする。
fn render_pdf_headless(file: &Path, page: usize, out_w: u32, out_h: u32, zoom: u32) -> Result<Vec<u8>> {
    let pdf = vecview_pdf::Pdf::open(file).context("PDF を開けません")?;
    let pages = pdf.page_count();
    if page >= pages {
        bail!("ページ範囲外: {}（総 {} ページ）", page + 1, pages);
    }
    let (pw, ph) = pdf.page_size(page)?;
    let (pw, ph) = (pw.max(1.0), ph.max(1.0));
    let viewport = viewport_for(pw, ph, out_w, out_h, zoom, (pw / 2.0, ph / 2.0));
    let mut rgba = pdf.render(page, viewport, out_w, out_h)?;
    fill_letterbox(&mut rgba, out_w, out_h, viewport, pw, ph);
    Ok(rgba)
}

/// ヘッドレスで Typst を単発コンパイル（`typst compile`）して SVG を生成し、要求ページを描く。
/// 出力は一時ディレクトリに `vv-render-<stem>-<pid>-{p}.svg` として作り、描画後に掃除する。
fn render_typ_headless(file: &Path, page: usize, out_w: u32, out_h: u32, zoom: u32) -> Result<Vec<u8>> {
    if which_typst().is_none() {
        bail!("typst が PATH にありません。Typst の描画には typst が必要です。");
    }
    let dir = std::env::temp_dir();
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("vv");
    let tag = std::process::id();
    let template = dir.join(format!("vv-render-{stem}-{tag}-{{p}}.svg"));
    let ok = Command::new("typst")
        .arg("compile")
        .arg(file)
        .arg(&template)
        .arg("--format")
        .arg("svg")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit()) // コンパイルエラーは表示する。
        .status()
        .context("typst compile の起動に失敗")?
        .success();
    // 自分が出した一時 SVG（全ページ分）を消す。
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
        bail!("typst compile が失敗しました");
    }
    let page_svg = dir.join(format!("vv-render-{stem}-{tag}-{}.svg", page + 1));
    if !page_svg.exists() {
        cleanup();
        bail!("ページ範囲外、または SVG が生成されませんでした: ページ {}", page + 1);
    }
    let renderer = Renderer::new().context("レンダラー初期化")?;
    let res = render_svg_file(&page_svg, &renderer, out_w, out_h, zoom);
    cleanup();
    res
}

/// SVG ファイルを開き、ページ中央・指定ズームのビューポートで RGBA へ描く（ヘッドレス共通）。
fn render_svg_file(path: &Path, renderer: &Renderer, out_w: u32, out_h: u32, zoom: u32) -> Result<Vec<u8>> {
    let doc = SvgDocument::open(
        path.to_str()
            .ok_or_else(|| anyhow!("パスが UTF-8 でありません"))?,
    )?;
    let page = doc.render_page(0)?;
    let pw = page.width.max(1.0);
    let ph = page.height.max(1.0);
    let viewport = viewport_for(pw, ph, out_w, out_h, zoom, (pw / 2.0, ph / 2.0));
    renderer.render(&page, out_w, out_h, viewport)
}

/// RGBA8（out_w×out_h）を PNG として `output` へ書く。`output` が `-` なら stdout。
fn write_png(rgba: &[u8], w: u32, h: u32, output: &str) -> Result<()> {
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder};
    use std::io::Write;
    let encode = |writer: &mut dyn std::io::Write| -> Result<()> {
        PngEncoder::new(writer)
            .write_image(rgba, w, h, ExtendedColorType::Rgba8)
            .map_err(|e| anyhow!("PNG エンコード失敗: {e}"))
    };
    if output == "-" {
        let mut out = std::io::stdout().lock();
        encode(&mut out)?;
        out.flush()?;
    } else {
        let f = std::fs::File::create(output)
            .with_context(|| format!("出力ファイルを作成できません: {output}"))?;
        let mut w = std::io::BufWriter::new(f);
        encode(&mut w)?;
        w.flush()?;
    }
    Ok(())
}

/// `"幅x高"`（区切りは `x`/`X`）を (幅, 高) ピクセルへ解析する。0 は不可、上限 16384 にクランプ。
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w.min(16384), h.min(16384)))
}

/// `dir` 内の `vecview-<stem>-<pid>-…` のうち、`<pid>` のプロセスがもう生きていないもの（＝過去に
/// クラッシュ等で掃除されずに残ったファイル）を削除する。稼働中の他インスタンスのファイルは
/// PID が生きているので消さない。プロセス生存判定は Linux の /proc を使い、それ以外の OS では何もしない
/// （`/tmp` の再起動クリアに任せる）。
fn sweep_dead_typst_pages(dir: &Path, stem: &str) {
    let prefix = format!("vecview-{stem}-");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // "vecview-<stem>-" を剥がし、続く数字列（PID）を取り出す。
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

/// PID のプロセスが生存しているか。Linux は `/proc/<pid>` の有無で判定。他 OS は常に true を返し
/// （安全側＝消さない）、掃除を行わない。
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
    // 出力パスはプロセス固有（PID）。同じ文書を複数インスタンスで開いてもファイルが衝突しない。
    let tag = std::process::id();
    // 過去にクラッシュ等で残った（プロセスが死んでいる）同名文書の取り残しだけ掃除する。
    // 稼働中の他インスタンスのファイルは PID が生きているので消さない。監視開始前なので通知は出ない。
    sweep_dead_typst_pages(&dir, &stem);
    // 複数ページ文書でも typst がエラーにならないようページ番号テンプレート {p} を使う。
    let template = dir.join(format!("vecview-{stem}-{tag}-{{p}}.svg"));

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

    Ok((Source::Typst { dir, stem, tag }, child))
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
    base: &mut Option<BaseFrame>,
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
    state.last_viewport = Some(viewport);

    let mut rgba = renderer.render(&page, out_w, out_h, viewport)?;
    // copy mode 中はオーバーレイ適用前のベースをキャッシュ（以降のキャレット移動で使い回す）。
    if state.copy.is_some() {
        *base = Some(BaseFrame { out_w, out_h, viewport, rgba: rgba.clone() });
    }
    if let Some(cm) = &state.copy {
        overlay_selection(&mut rgba, out_w, out_h, viewport, cm);
    }
    backend.display(&rgba, out_w, out_h)?;
    Ok(())
}

/// ラスタ化する解像度（ピクセル）を求める。
///
/// tmux プレースホルダ配置では端末がピクセル寸法を報告せず（width/height=0）、セル数 ×
/// 概算セルサイズ(8x16) に頼るしかない。実セルサイズが概算より大きい環境（HiDPI 等）では
/// この低解像度のまま端末側で引き伸ばされてボケる。そこでプレースホルダ時のみ高解像度で
/// ラスタ化し、端末側の縮小でシャープにする（スーパーサンプリング）。倍率は VECVIEW_SCALE
/// （`scale`、既定1=等倍、1..=4）。高倍率は転送量が倍率²で増え連続操作で端末が落ちうるため既定は
/// 等倍。直接配置(a=T)やフレームバッファはネイティブ画素表示なので常に等倍に保つ。
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
    // 概算セルサイズは VECVIEW_CELL_PX=幅x高 で上書き可能。SSH+tmux 等ではピクセル寸法が
    // 伝播せず（width/height=0）、実セルサイズが概算とズレて Sixel が縮小表示されるため、
    // 環境ごとに 1 度合わせれば以後ペインを正しく埋められる。
    let (cell_w, cell_h) = fallback_cell_px();
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.height > 0 {
            return (ws.width as u32, ws.height as u32);
        }
        if ws.columns > 0 && ws.rows > 0 {
            // tmux passthrough sixel は画像がペイン下端（=物理画面下端）に達すると画面全体が
            // スクロールし、隣ペインまで崩れる。最下 1 行ぶん空けて下端に届かないようにする。
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

/// ピクセル寸法を報告しない端末向けの概算セルサイズ（幅, 高）。既定 8×16。
/// 環境変数 `VECVIEW_CELL_PX="幅x高"`（例 `10x17`）で上書きできる。各値は 1..=128 にクランプ。
fn fallback_cell_px() -> (u32, u32) {
    std::env::var("VECVIEW_CELL_PX")
        .ok()
        .and_then(|s| parse_cell_px(&s))
        .unwrap_or((8, 16))
}

/// 指定 tmux ペインの属するウィンドウが現在アクティブ（前面表示中）か。判定不能なら None。
/// tmux placeholder kitty で、別ウィンドウへ切り替わったとき画像を消すための可視判定に使う。
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

/// 受信した先頭メッセージに、チャネルに既に積まれている分をまとめて足してバーストにする。
/// 連続する Reload/キーを1回の描画へ集約し、Quit が後ろに積まれても取りこぼさない。
fn drain_burst(first: Msg, rx: &mpsc::Receiver<Msg>) -> Vec<Msg> {
    let mut msgs = vec![first];
    while let Ok(m) = rx.try_recv() {
        msgs.push(m);
    }
    msgs
}

/// 連続入力中の画像転送の最小間隔（ms）。`VECVIEW_MIN_FRAME_MS` で上書き、最小 16。短いほど
/// 追従が滑らかだが、端末を追い越してクラッシュさせるリスクが上がる。既定はバックエンド依存
/// （`default`）：tmux passthrough（kitty placeholder / sixel）は転送が重く端末側に溜まりやすい
/// ため保守的に、直接配置はネイティブで軽いため小さめにする。
fn min_frame_ms(default: u64) -> u64 {
    std::env::var("VECVIEW_MIN_FRAME_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|v| v.max(16))
        .unwrap_or(default)
}

/// passthrough sixel の定期再描画間隔（ms）。`VECVIEW_REDRAW_MS` で上書き、既定 1000、最小 100。
/// 短いほど消えてから復元するまでが速いが、その分 sixel 再送の転送量が増える。
fn redraw_interval_ms() -> u64 {
    std::env::var("VECVIEW_REDRAW_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|v| v.max(100))
        .unwrap_or(1000)
}

/// "幅x高"（区切りは `x` または `X`）を (幅, 高) に解析。各値は 1..=128 にクランプ。
fn parse_cell_px(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    Some((w.clamp(1, 128), h.clamp(1, 128)))
}

/// スーパーサンプリング倍率を決める。優先順位は CLI 引数 > 環境変数 VECVIEW_SCALE > 既定1。
/// いずれも 1..=4 にクランプする。既定を 1（等倍）にしているのは、tmux placeholder 経路で
/// 高倍率にすると 1 フレームの転送量が倍率²で増え、連続操作で端末（Ghostty 等）が画像更新を
/// 捌ききれずクラッシュしやすくなるため。シャープさが欲しく端末が耐える環境では 2 以上を指定する。
fn resolve_scale(arg: Option<u32>) -> u32 {
    arg.or_else(|| {
        std::env::var("VECVIEW_SCALE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
    })
    .unwrap_or(1)
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
    let (cw, ch) = fallback_cell_px();
    println!("fallback cell(px)  = {cw} x {ch}  ← VECVIEW_CELL_PX で上書き可（ピクセル0時に使用）");
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
        assert_eq!(parse_key("foo"), None); // 複数文字の未知名は不可。
    }

    #[test]
    fn default_bindings_match_request() {
        // 既定: 矢印=パン、j/k=ページ送り/戻り、h/l=先頭/最終ページ。
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
        // 旧パン hjkl は既定では割り当てなし（h/l は別アクション、j/k はページ）。
        assert!(matches!(km.action(&key(KeyCode::Char('j'), false)), Some(Action::NextPage)));
    }

    #[test]
    fn override_replaces_action_keys() {
        // next_page を space のみへ。j は未割り当てになる。
        let mut ov = HashMap::new();
        ov.insert("next_page".to_string(), vec!["space".to_string()]);
        let km = Keymap::build(&ov);
        assert!(matches!(km.action(&key(KeyCode::Char(' '), false)), Some(Action::NextPage)));
        assert!(km.action(&key(KeyCode::Char('j'), false)).is_none());
        // 上書きしていない prev_page は既定の k のまま。
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
        // 2行（y=0 と y=20）。間に pdfium 由来の \r\n（矩形ゼロの制御文字）を挟む。
        let glyphs = vec![
            glyph('a', 0.0, 0.0),
            glyph('b', 8.0, 0.0),
            vecview_pdf::Glyph { ch: '\r', rect: [0.0, 0.0, 0.0, 0.0] },
            vecview_pdf::Glyph { ch: '\n', rect: [0.0, 0.0, 0.0, 0.0] },
            glyph('c', 0.0, 20.0),
            glyph('d', 8.0, 20.0),
        ];
        let cm = super::CopyMode::new(glyphs).unwrap();
        // 制御文字は除外され、可視4文字が2行に分かれる。
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
        cm.cursor = 2; // 全選択。
        assert_eq!(cm.selected_text(), "ab\nc");
        // 未選択ならキャレット1文字のみ。
        cm.anchor = None;
        cm.cursor = 1;
        assert_eq!(cm.selected_text(), "b");
    }

    #[test]
    fn copy_mode_vertical_move_keeps_column() {
        // 2行、各3文字。上の行で2文字目にいて下へ→下の行の2文字目付近へ。
        let glyphs = vec![
            glyph('a', 0.0, 0.0),
            glyph('b', 8.0, 0.0),
            glyph('c', 16.0, 0.0),
            glyph('d', 0.0, 20.0),
            glyph('e', 8.0, 20.0),
            glyph('f', 16.0, 20.0),
        ];
        let mut cm = super::CopyMode::new(glyphs).unwrap();
        cm.cursor = 1; // 'b'（x≈8）。
        cm.move_line(1);
        assert_eq!(cm.glyphs[cm.cursor].ch, 'e'); // 下行の同桁。
        cm.move_line(-1);
        assert_eq!(cm.glyphs[cm.cursor].ch, 'b');
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
    fn parse_cell_px_accepts_x_separator_and_clamps() {
        assert_eq!(parse_cell_px("10x17"), Some((10, 17)));
        assert_eq!(parse_cell_px(" 12 X 24 "), Some((12, 24)));
        assert_eq!(parse_cell_px("0x999"), Some((1, 128))); // 1..=128 にクランプ
        assert_eq!(parse_cell_px("8"), None);
        assert_eq!(parse_cell_px("axb"), None);
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
