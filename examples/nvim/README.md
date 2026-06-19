# vv.nvim — nvim 内プレビュー

Neovim の中に **SVG / Typst / PDF** のライブプレビューを出すプラグイン（純 Lua・**第三者プラグイン
依存なし**）。フローティング窓を開き、`vv --render` でその窓サイズの PNG を作って **Kitty graphics
protocol** で重ねる。`.typ` は保存のたびに再描画＝**論文・資料作成のライブプレビュー**が nvim 内で
完結する（ブラウザも tmux 別ペインも不要）。

表示は自前の薄い端末層（`lua/vv/term.lua`）が外側端末へ直接 kitty 描画する。image.nvim 等には
依存しないので、挙動を完全に自分で制御できる。

## 必要なもの

- PATH に `vv`（本リポジトリの vecview。`cargo install --path crates/vecview`）
- **Kitty graphics 対応端末**（Ghostty / kitty）。tmux 内なら `set -g allow-passthrough on`
- Neovim 0.10+（`vim.system` を使用。0.11 で確認）
- Typst には `typst`、PDF には `libpdfium`（vv の実行時依存と同じ）

> Sixel 専用端末（Kitty 非対応）では現状この nvim プラグインは表示できない（vv 本体や yazi は sixel 可）。

## インストール（lazy.nvim）

プラグイン本体はこのリポジトリの **サブディレクトリ** `examples/nvim/vv.nvim/` にある。lazy.nvim には
サブディレクトリを指定する `rtp=` キーが無いので、`config` で runtimepath に足してから `require` する:

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
    cell_width = 10,   -- 端末セルのピクセル寸法（描画解像度の目安。シャープさだけに効く）
    cell_height = 20,
    width = 0.5,       -- フロート幅（エディタ全幅に対する割合）
    -- vv = "vv",      -- 実行ファイル名を変えている場合
  },
  config = function(plugin, opts)
    -- サブディレクトリを runtimepath に追加してから setup（lazy には rtp= が無いため）。
    vim.opt.rtp:append(plugin.dir .. "/examples/nvim/vv.nvim")
    require("vv").setup(opts)
  end,
}
```

> 公開前のローカルテストでは、上記の代わりに `dir = "/path/to/vecview/examples/nvim/vv.nvim"` を
> 指定する（`dir` はそのディレクトリ自体を runtimepath に入れるので rtp 追加は不要）。

手動で置く場合は `examples/nvim/vv.nvim` を runtimepath に足し、`require("vv").setup({...})` を呼ぶ。

## コマンド

| コマンド | 動作 |
|---|---|
| `:VV [file]` | プレビューを開く（引数なしなら現在バッファのファイル） |
| `:VVToggle` | 開閉 |
| `:VVClose` | 閉じる |
| `:VVNext` / `:VVPrev` | ページ送り / 戻し |
| `:VVRefresh` | 手動で描き直す（下記の「既知の制約」参照） |

`.typ` を開いて `:VV` → 別窓にプレビュー。編集して `:w` するたびに再描画される。

## 設定

| キー | 既定 | 説明 |
|---|---|---|
| `cell_width` / `cell_height` | `10` / `20` | 端末セルのピクセル寸法。レンダリング解像度の目安（kitty が窓のセル数へ再スケールするので厳密でなくてよい） |
| `width` | `0.5` | フロート窓の横幅（エディタ全幅に対する割合） |
| `vv` | `"vv"` | vv 実行ファイル名 |

## 既知の制約（MVP）

- **再描画同期は最小限**。フローティング窓は矩形が固定なのでスクロール追従は不要だが、nvim が
  画面全体を描き直す操作（ポップアップ・`:redraw!` 等）の後に画像が一部上書きされることがある。
  その場合は `:VVRefresh` で描き直す。完全な同期は今後の課題。
- 画像転送は直接データ（チャンク分割）方式なので **SSH+tmux 越しでも動く**が、大きな PNG では
  保存毎の再転送がやや重い。`cell_width/height` を下げると軽くなる（画質と引き換え）。
- 端末は Kitty graphics 対応が前提（上記）。
