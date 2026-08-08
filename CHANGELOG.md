# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-08-08

The headline of this release is that page flips and text selection stopped
blocking on rendering. Rasterization moved off the event loop entirely, and the
terminal now receives only the parts of the image that changed.

### Added

- **`bundled` feature**, which fetches libpdfium during the build and embeds it, so
  `cargo install vecview --features bundled` is all that's needed for PDF support.
  pdfium is Chromium's C++ library and is not distributed through the crate
  registry, so previously it had to be installed by hand. Off by default: the build
  reaches the network, which offline and air-gapped builds cannot do, and the binary
  grows from 15 MB to 28 MB.
- **Configuration file.** Every rendering and terminal setting now has a key in
  `config.toml`, so standing preferences no longer have to be exported from a
  shell rc. Looked up at `$VECVIEW_CONFIG`, `$XDG_CONFIG_HOME/vecview/config.toml`,
  then `~/.config/vecview/config.toml`. Precedence is CLI argument > environment
  variable > config file > default: the environment carries per-run context
  (SSH, tmux, a multiplexer each want a different cell size), so it outranks the
  file. Unknown sections and keys are reported rather than silently ignored.
- **Markdown preview**, rendered through Typst via cmarker, paginated to a paper
  size set by `render.md_page`.
- **PDF export** for Typst and Markdown sources, via `--pdf` or the `e` key.
- **herdr support**, using virtual kitty placements. herdr reports no pixel cell
  size to its panes, so the image is sized by the cell grid instead.
- **Terminal cell size auto-detection** through a `CSI 16t` query, which works
  where `TIOCGWINSZ` reports zeroes (notably inside tmux). Overridable with
  `terminal.cell_px` / `VECVIEW_CELL_PX` for environments that answer neither.
- **Supersampled anti-aliasing** (`render.aa_ss`, default 2). The scene is
  rendered at a multiple of the target size and downsampled, sharpening text and
  curve edges without enlarging the transferred image.
- **Page cache with neighbor prefetch**, so revisiting a page skips rasterizing
  it. Prefetch reaches two pages either side and runs only while input is idle.
- `VECVIEW_TIMING`, which reports per-stage timings (worker render, transfer,
  bands sent, startup) for diagnosing display latency.
- `contrib/install-vv.sh`, for building against an older glibc when the build and
  run environments differ (e.g. building in a container, running on the host).

### Changed

- **Page rasterization runs on a dedicated thread.** It costs ~50 ms for a page
  at 1792x1950, and over 100 ms for one carrying photographs, against ~15 ms to
  display a cached one. Doing it on the event loop meant every render was time
  spent not reading input, which showed up as a keypress landing mid-prefetch
  waiting for it. Confining pdfium to that one thread is also what keeps its
  single-threaded usage assumption true.
- **The image is transmitted in bands, and only changed bands are resent.** The
  placeholder image is split into 8 horizontal strips, each compared against the
  previous frame before being compressed. Moving the caret through a selection
  now sends one band (~141 KB, 4.5 ms) where it used to send the whole page
  (~2.7 MB, ~30 ms on a page with photographs).
- Selection highlighting merges adjacent glyphs on a line into one span before
  tinting, instead of tinting each glyph. The previous form left the gaps between
  characters unpainted.
- Typst's stderr is routed to the status line rather than written straight to the
  terminal, where it corrupted the image in split panes.
- Upgraded pdfium-render from 0.8 to 0.9, which is what `pdfium-bundled` builds
  against. Rendered output is byte-for-byte identical.
- When libpdfium is missing, the error now says where to get it and which paths were
  searched, instead of naming two paths and stopping.

### Fixed

- The display could stall after a page flip. A frame arriving for a page already
  flipped past was counted as drawn and cancelled the pending redraw for the page
  actually in view, leaving the screen stuck until the next keypress.
- Regenerating a watched PDF triggered an endless reload loop: pdfium holds the
  file open, so rendering bumped its atime and re-fired the watcher.
- Copy mode re-rasterized the page on entry, on every flip made while in it, and
  again on exit, because copy-mode frames were excluded from the cache. What is
  cached is the clean page image, which copy mode does not alter.
- Reading back and forth could evict the page still on screen from the cache.
- An idle vecview pinned a CPU core when its tmux window was hidden. The
  visibility poll that caused it is now off by default (`terminal.vis_poll_ms`).
- Typst documents whose assets sit at the project root failed to build a
  copy-mode text layer, because `--root` was not resolved the way the live
  preview resolves it.

## [0.1.0] - 2026-06-20

Initial release. Displays SVG, Typst, and PDF in the terminal at vector quality,
with live reload, zoom and pan, page navigation, and text selection. Output via
kitty graphics, Sixel, or the Linux framebuffer.

[0.2.0]: https://github.com/barewalker/vecview/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/barewalker/vecview/releases/tag/v0.1.0
