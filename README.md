# vecview

A CLI tool that displays vector graphics (SVG / Typst / PDF) in your terminal
**at full vector quality, without rasterizing**. SVG and Typst are tessellated
with `lyon` and rendered by `wgpu` (GPU), with anti-aliasing performed freshly
at the current display resolution every time — so zooming in never degrades the
image. PDFs are rasterized directly by `pdfium`.

The primary goal is to **keep the live preview of Typst documents you edit in
nvim entirely inside the terminal**. No browser required — every time you save,
the preview in your terminal updates.

## Installation

Install with cargo straight from GitHub:

```bash
cargo install --git https://github.com/barewalker/vecview
```

Or build from a local clone:

```bash
cargo install --path crates/vecview
# or: cargo build --release  →  target/release/vv
```

Prebuilt binaries (built against an older glibc for broad portability) are attached
to each [GitHub Release](https://github.com/barewalker/vecview/releases) — download
`vv`, `chmod +x vv`, and put it on your `PATH` if you'd rather not build it yourself.

The installed command is **`vv`** (the project's full name is vecview). Note that the
runtime dependencies below still need to be present regardless of how you install.

### Runtime dependencies

| Dependency | Purpose | Notes |
|---|---|---|
| **libpdfium** (`libpdfium.so` / `.dylib` / `.dll`) | PDF rendering and the text layer for Typst/PDF | Required. Linked and loaded by `pdfium-render` |
| **typst** | Live preview of `.typ` files | Only needed when passing a `.typ`. Must be on `PATH` |
| **Vulkan driver** (Mesa RADV/ANV, etc.) | GPU rendering of SVG/Typst | Required for headless wgpu rendering |

Prebuilt libpdfium binaries are available from
[bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries).
Place the library somewhere on your library search path (`LD_LIBRARY_PATH`, etc.).

## Usage

```bash
vv <FILE>            # SVG / Typst (.typ) / PDF
vv doc.typ           # spawns `typst watch` internally and live-redraws on every save
vv paper.pdf         # display a PDF (watches the file for changes and redraws)
vv diagram.svg       # display an SVG (also usable as a general-purpose SVG viewer)

# Options
vv doc.typ -z 150            # initial zoom 150%
vv doc.typ -s 2              # supersampling factor (default 1)
vv doc.typ -b sixel          # force a backend [kitty|tmux|sixel|framebuffer]
```

### Headless rendering (`--render`)

A mode that renders a single page to a PNG and exits, with no terminal and no
interaction. It's the foundation for editor/file-manager integrations (the yazi
previewer and the nvim plugin) that just need to "produce one image at a given
size." The render path is the same as normal display (PDF = pdfium,
SVG/Typst = wgpu).

```bash
vv --render doc.typ --size 800x1000 -o preview.png   # output page 1 as PNG
vv --render paper.pdf --size 700x900 --page 3 -o -    # send page 3 to stdout
```

| Flag | Description |
|---|---|
| `--render` | Enable headless render mode |
| `--size WxH` | Output pixel size (required) |
| `--page N` | Page to render (1-based, default 1) |
| `-o, --output` | Output PNG path. `-` for stdout (default `-`) |
| `-z, --zoom` | Zoom % (shared with normal display) |

### Key bindings (interactive mode, when launched on a TTY)

| Key | Action |
|---|---|
| `+` / `=` | Zoom in |
| `-` | Zoom out |
| `0` | Reset zoom (fit to view) |
| `w` / `v` | Fit content horizontally / vertically |
| Arrows | Pan (move the visible region when zoomed in) |
| `j` / `Space` / `PageDown` | Next page |
| `k` / `PageUp` / `Backspace` | Previous page |
| `h` / `l` | First / last page |
| Mouse wheel | Page navigation (down = next / up = previous) |
| `y` | Enter text selection (copy mode) |
| `?` | Show help |
| `q` / `Esc` / `Ctrl-C` | Quit |

Keys can be remapped under `[keys]` in `config.toml` (the `?` help shows the path).

Zoom is anchored to the whole-page fit view (100%) and lets you **magnify part of
the page**. When zoomed in, rather than scaling up a cached image, the viewport
is **re-tessellated and re-rendered at that resolution**, so the image never
degrades no matter how far you zoom in (for PDFs, pdfium re-rasterizes the
viewport equivalently). Use the arrows to pan the magnified region. Multi-page
documents can be browsed with the page-navigation keys.

#### Text selection and copy (copy mode)

Press `y` (or drag with the mouse) to select text and copy it to the clipboard.
Because the output is an image, **the terminal's native text selection does not
work**, so vecview provides a dedicated copy mode.

| Key | Action |
|---|---|
| `h` `j` `k` `l` / arrows | Move the caret (by character / line) |
| `0` / `$` | Start / end of line |
| `g` / `G` | Start / end of document |
| `Space` | Begin / clear selection |
| `Enter` / `y` | Copy and exit |
| `Esc` / `q` | Cancel |
| Mouse drag | Select a range; releasing copies it |

Copy uses **OSC 52**, so it's independent of X11/Wayland and lands in the host's
clipboard even over SSH or tmux (tmux requires `allow-passthrough on`).

Supported formats: **PDF** has a text layer natively. **Typst** is displayed as
SVG (vector quality), and when you enter copy mode the same `.typ` is also
compiled to PDF in the background, whose glyphs and coordinates drive the
selection (Typst's SVG turns glyphs into paths and carries no text). A standalone
`.svg` has no text layer.

### Editor / file-manager integration (yazi / nvim)

Integration plugins built on top of `vv --render` (headless PNG output) are
bundled in this repository.

- **[examples/nvim](examples/nvim/)** — `vv.nvim`. Shows a live preview of
  SVG / Typst / PDF inside Neovim (pure Lua, no third-party dependencies, Kitty
  graphics). Display a `.typ` in a separate window with `:VV` and have it redraw
  on every save. **Writing papers and documents stays entirely inside nvim.**
- **[examples/yazi](examples/yazi/)** — `vv.yazi`. A previewer that shows
  SVG / Typst / PDF in yazi's preview pane. Especially useful because **Typst
  can't be viewed in yazi alone**.

You don't have to use either plugin — you can just launch `vv doc.typ` in a
separate tmux pane and place it side by side (the preview updates every time you
save `doc.typ` in nvim; no plugin needed).

## Output backends

| Backend | Target terminals | Notes |
|---|---|---|
| `kitty` | Ghostty / kitty / WezTerm | Kitty Graphics Protocol (direct RGBA transfer) |
| `kitty (tmux placeholder)` | The above terminals inside tmux | Unicode placeholders + DCS passthrough to position correctly within the pane |
| `sixel` | Sixel-capable terminals (Windows Terminal / xterm / foot / mlterm, etc.) | For terminals without Kitty support. Reduced to 256 colors |
| `sixel (tmux passthrough / native)` | Sixel terminals inside tmux | Native if tmux supports sixel, otherwise passthrough |
| `framebuffer` | Linux bare TTY / embedded | Draws directly to `/dev/fb0`. Vector quality shines at native resolution |

The backend is chosen automatically at startup from environment variables and TTY
state. You can also force it with `--backend` (or `VECVIEW_BACKEND`).

### Using it under tmux

To pass Kitty / Sixel graphics through tmux, enable passthrough:

```tmux
set -g allow-passthrough on
```

To use tmux's native Sixel support, make tmux aware that your terminal supports
sixel:

```tmux
set -as terminal-features '*:sixel'
```

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `VECVIEW_BACKEND` | auto-detect | Force a backend `[kitty\|tmux\|sixel\|framebuffer]` |
| `VECVIEW_SCALE` | `1` | Supersampling factor (1..=4). Also settable with `-s`. Higher is sharper but transfer size grows with the square of the factor |
| `VECVIEW_CELL_PX` | `8x16` | Approximate cell size `WxH` when the terminal doesn't report pixel dimensions (SSH+tmux, etc.). Adjust if the display shrinks or overflows |
| `VECVIEW_MIN_FRAME_MS` | `200` (over tmux) / `80` (direct) | Minimum interval (ms) between image transfers during continuous input. Smaller is smoother but becomes unstable if it outruns the terminal |
| `VECVIEW_REDRAW_MS` | `1000` | Resend interval (ms) to restore tmux passthrough sixel after a tmux redraw |
| `VECVIEW_SIXEL_NATIVE` | off | `1` to attempt tmux native sixel (requires sixel in `client_termfeatures`) |
| `VECVIEW_PROBE` | off | `1` to print the size reported by the terminal and the render resolution, then exit (for resolution debugging) |

> Note: over tmux, the terminal protocol means some terminals handle
> high-frequency image updates poorly with Kitty / Sixel. If rapid zoom/pan
> garbles the display or destabilizes the terminal, keeping `VECVIEW_SCALE=1`
> (the default) plus a larger `VECVIEW_MIN_FRAME_MS` (e.g. `300`) stabilizes it.

### Using the framebuffer

- Run it on a bare TTY (a console such as `Ctrl+Alt+F3`). It can't display inside
  a GUI session because the compositor owns the screen.
- Read/write permission on `/dev/fb0` is required (e.g. the `video` group).

## Architecture

```
.typ ──(typst watch)──┐
                      ├─> SVG ──usvg──> vector tree ──lyon──> mesh
.svg ─────────────────┘                                       │
                                                              ▼
                            wgpu (offscreen · MSAA · display resolution)──> RGBA
.pdf ──pdfium──(viewport re-rasterize)────────────────────────> RGBA
                                                              │
                          ┌──────────────────────┬────────────┴─────────┐
                          ▼                       ▼                      ▼
                Kitty Graphics Protocol         Sixel            direct draw to /dev/fb0
```

Crate layout (Cargo workspace):

| Crate | Role |
|---|---|
| `vecview` | CLI entry point. Spawns `typst watch`, watches files, runs the live redraw loop |
| `vecview-core` | Format-agnostic abstractions (`Document` / `OutputBackend` / `Page` / `PathData`) |
| `vecview-svg` | Parses SVG with `usvg` and converts it to a `Page` (preserving curve information) |
| `vecview-renderer` | `lyon` tessellation + headless `wgpu` rendering + RGBA readback |
| `vecview-pdf` | Direct PDF rasterization and text-layer extraction via `pdfium` |
| `vecview-output` | Backend detection and the Kitty / Sixel / Framebuffer implementations |

## Building and testing

```bash
cargo build
cargo test                 # smoke tests for blit conversion, aspect calc, GPU rendering, etc.
cargo clippy --all-targets
```

Headless GPU rendering requires a Vulkan driver (Mesa RADV/ANV, etc.).

## Status and roadmap

Implemented: display of SVG / Typst / PDF, live redraw on file changes, Kitty
(+ tmux placeholder) / Sixel (+ tmux) / Framebuffer output, high-resolution
vector-quality rendering, interactive zoom and multi-page navigation, text
selection and copy, correct placement within a tmux pane, and handling of window
switching and multiple instances.

Not yet supported / known limitations: faithful rendering of gradients and clip
paths, verification of framebuffer output on real hardware, and the stability of
high-frequency image updates on some terminals (mitigated by the environment
variables above).

## License

Apache License 2.0 — see [LICENSE](LICENSE) for details. Copyright 2026 Mitsuaki Takeuchi.
