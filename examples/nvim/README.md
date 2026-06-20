# vv.nvim — preview inside nvim (experimental / WIP)

> ⚠️ **Experimental.** Basic display works, but placement and redraw
> synchronization under tmux aren't fully ironed out yet. If you want something
> stable for now, the recommended approach is to launch `vv doc.typ` in a
> separate tmux pane.

A plugin that shows a live preview of **SVG / Typst / PDF** inside Neovim (pure
Lua, **no third-party plugin dependencies**). It opens a floating window, uses
`vv --render` to produce a PNG at that window's size, and overlays it via the
**Kitty graphics protocol**. A `.typ` redraws on every save — so the live
preview for writing papers and documents **stays entirely inside nvim** (no
browser, no separate tmux pane).

Display is handled by a thin in-house terminal layer (`lua/vv/term.lua`) that
draws kitty graphics directly to the outer terminal. It doesn't depend on
image.nvim or the like, so you have full control over its behavior.

## Requirements

- `vv` on `PATH` (the vecview in this repo; `cargo install --path crates/vecview`)
- A **Kitty-graphics-capable terminal** (Ghostty / kitty). Inside tmux, set
  `set -g allow-passthrough on`
- Neovim 0.10+ (uses `vim.system`; verified on 0.11)
- `typst` for Typst and `libpdfium` for PDFs (same as vv's runtime dependencies)

> On Sixel-only terminals (without Kitty support), this nvim plugin currently
> can't display anything (vv itself and yazi do support sixel).

## Installation (lazy.nvim)

The plugin itself lives in the **subdirectory** `examples/nvim/vv.nvim/` of this
repository. lazy.nvim has no `rtp=` key for pointing at a subdirectory, so add it
to runtimepath in `config` before `require`-ing it:

```lua
{
  "barewalker/vecview",
  cmd = { "VV", "VVToggle", "VVClose", "VVNext", "VVPrev", "VVRefresh" },
  keys = {
    { "<leader>vv", "<cmd>VVToggle<cr>", desc = "vecview preview" },
    { "<leader>vn", "<cmd>VVNext<cr>",   desc = "vecview next page" },
    { "<leader>vp", "<cmd>VVPrev<cr>",   desc = "vecview prev page" },
  },
  opts = {
    cell_width = 10,   -- terminal cell pixel size (a hint for render resolution; affects sharpness only)
    cell_height = 20,
    width = 0.5,       -- float width (fraction of the full editor width)
    -- vv = "vv",      -- if you renamed the executable
  },
  config = function(plugin, opts)
    -- Add the subdirectory to runtimepath before setup (lazy has no rtp= key).
    vim.opt.rtp:append(plugin.dir .. "/examples/nvim/vv.nvim")
    require("vv").setup(opts)
  end,
}
```

> For local testing before publishing, use `dir = "/path/to/vecview/examples/nvim/vv.nvim"`
> instead (`dir` puts that directory itself on runtimepath, so the rtp append
> isn't needed).

To set it up manually, add `examples/nvim/vv.nvim` to runtimepath and call
`require("vv").setup({...})`.

## Commands

| Command | Action |
|---|---|
| `:VV [file]` | Open the preview (the current buffer's file if no argument) |
| `:VVToggle` | Toggle open/closed |
| `:VVClose` | Close |
| `:VVNext` / `:VVPrev` | Next / previous page |
| `:VVRefresh` | Manually redraw (see "Known limitations" below) |

Open a `.typ` and run `:VV` → the preview appears in a separate window. It
redraws every time you `:w` after editing.

## Configuration

| Key | Default | Description |
|---|---|---|
| `cell_width` / `cell_height` | `10` / `20` | Terminal cell pixel size. A hint for render resolution (kitty rescales to the window's cell count, so it needn't be exact) |
| `width` | `0.5` | Width of the float window (fraction of the full editor width) |
| `vv` | `"vv"` | The vv executable name |

## Known limitations (MVP)

- **Redraw sync is minimal.** The floating window has a fixed rectangle, so it
  doesn't need to follow scrolling, but after operations where nvim redraws the
  whole screen (popups, `:redraw!`, etc.) the image can be partially overwritten.
  In that case, redraw it with `:VVRefresh`. Full synchronization is future work.
- Image transfer uses direct data (chunked), so it **works even over SSH+tmux**,
  but for large PNGs the resend on every save is a bit heavy. Lowering
  `cell_width/height` makes it lighter (at the cost of quality).
- A Kitty-graphics-capable terminal is required (see above).
