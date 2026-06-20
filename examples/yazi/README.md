# vv.yazi — yazi previewer

A previewer plugin that renders **SVG / Typst / PDF** with `vv --render` and
displays it in [yazi](https://yazi-rs.github.io/)'s preview pane. The main value
is **Typst (`.typ`), which yazi can't preview natively**.

Rendering goes through the same path as vv itself (PDF = pdfium,
SVG/Typst = wgpu), so the preview looks identical to the main display.

## Requirements

- `vv` on `PATH` (the vecview in this repo; `cargo install --path crates/vecview`)
- yazi **26 or later** (uses `ya.mgr_emit`)
- `typst` for Typst previews and `libpdfium` for PDFs (same as vv's runtime
  dependencies)

## Installation

Place the plugin itself in yazi's plugin directory:

```bash
mkdir -p ~/.config/yazi/plugins
cp -r examples/yazi/vv.yazi ~/.config/yazi/plugins/
```

Add a previewer rule under `[plugin]` in `~/.config/yazi/yazi.toml` (use
`prepend_previewers` to take priority over the defaults):

```toml
[plugin]
prepend_previewers = [
    { url = "*.typ", run = "vv" },              # Typst (unsupported by yazi → the main reason to use this)
    { url = "*.svg", run = "vv" },              # SVG (optional)
    { mime = "application/pdf", run = "vv" },   # PDF (optional; yazi's default may be faster)
]
# Because SVG has an image mime type, yazi's standard image preloader tries to
# convert svg→PNG with the external `resvg` (you get a "Failed to start resvg"
# error if resvg isn't installed). Make the preloader a noop to route everything
# through vv.
prepend_preloaders = [
    { url = "*.svg", run = "noop" },
]
```

If `.typ` alone is enough, you can drop the svg/pdf lines.

> If you'd rather not handle SVG with vv and just use **yazi's standard svg
> preview**, remove the svg line and the preloaders above and install `resvg`
> instead (`cargo install resvg`). resvg is based on usvg — the same rendering
> engine as vv — so the quality is equivalent.

## On the tmux + Ghostty crash (important)

**When you use Ghostty over tmux, image previews can crash Ghostty itself. This
is a known bug on Ghostty's side, not in vecview**, and it happens with yazi's
standard images (png/jpg) and with native kitty images in general (it does not
happen natively, without tmux). The reported conditions:

- Occurs with tmux's **`mouse on`** (**does not occur with `mouse off`**)
- Occurs when the window is **larger than ~90x40 cells** (maximized)
- Occurs when the image is **over ~100KB**

References: ghostty-org/ghostty discussions
[#11909](https://github.com/ghostty-org/ghostty/discussions/11909) /
[#4266](https://github.com/ghostty-org/ghostty/discussions/4266) /
[#9197](https://github.com/ghostty-org/ghostty/discussions/9197)

Workarounds:

- **Turn off tmux mouse** (`set -g mouse off`, or toggle it with `prefix+m`) —
  the most reliable
- Don't maximize the window (with a smaller window it's less likely even with
  `mouse on`)
- Update Ghostty (this area is being actively fixed)
- For heavy use, run yazi outside tmux (in a native terminal)

Lowering `[preview] image_delay` (0–100ms) or `max_width`/`max_height` helps
somewhat, but it doesn't fully fix the bug above.

## Notes

- **The first render is slow** (`typst compile` + wgpu initialization). yazi
  caches per page, so the same file is instant on the second view onward.
- Image quality depends on the terminal protocol. On Ghostty / kitty, setting
  `[preview] preview_protocol = "kitty"` in `~/.config/yazi/yazi.toml` gives the
  best quality (sixel is reduced to 256 colors).
- For multi-page documents, scrolling in the preview turns the page.
