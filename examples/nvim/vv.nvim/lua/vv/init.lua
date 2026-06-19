--- vv.nvim — nvim 内に SVG / Typst / PDF のライブプレビューを出す（純 Lua・第三者依存なし）。
---
--- フローティング窓を開き、`vv --render` でその窓サイズの PNG を作って Kitty graphics で重ねる。
--- `.typ` は保存（BufWritePost）のたびに再描画＝ライブプレビュー。表示層は term.lua（外側端末へ
--- 直接 kitty 描画）で、image.nvim 等の外部プラグインには依存しない。
---
--- 前提: PATH に `vv`、端末が Kitty graphics 対応（Ghostty / kitty）。tmux なら allow-passthrough on。

local term = require("vv.term")

local M = {}

local config = {
	-- 端末セルのピクセル寸法。描画解像度の目安に使う（kitty が c×r セルへ再スケールするので
	-- 厳密でなくてよい＝シャープさだけに効く）。vv の VECVIEW_CELL_PX と同じ考え方。
	cell_width = 10,
	cell_height = 20,
	width = 0.5, -- フロート幅（エディタ全幅に対する割合）。右側に縦長で出す。
	vv = "vv", -- vv 実行ファイル名（PATH 上）。
}

local state = {
	win = nil,
	buf = nil,
	file = nil, -- 絶対パス
	page = 1,
	zoom = 100,
	png = nil,
	image_id = 0xB00B, -- このプラグイン固有の画像 ID。
	rendering = false,
}

local SUPPORTED = { typ = true, svg = true, pdf = true }

local function is_open()
	return state.win ~= nil and vim.api.nvim_win_is_valid(state.win)
end

-- フロート内容矩形：スクリーン1始まりセル (row,col) と セル数 (cols,rows)。border=none 前提。
local function geom()
	local pos = vim.api.nvim_win_get_position(state.win)
	return pos[1] + 1, pos[2] + 1, vim.api.nvim_win_get_width(state.win), vim.api.nvim_win_get_height(state.win)
end

-- 現在ページを vv --render で PNG 化し、フロートへ Kitty 描画する。
function M.render()
	if not is_open() or not state.file or state.rendering then
		return
	end
	local _, _, cols, rows = geom()
	local pw = cols * config.cell_width
	local ph = rows * config.cell_height
	state.png = state.png or (vim.fn.tempname() .. ".png")
	state.rendering = true

	vim.system({
		config.vv,
		"--render",
		state.file,
		"--size",
		string.format("%dx%d", pw, ph),
		"--page",
		tostring(state.page),
		"--zoom",
		tostring(state.zoom),
		"-o",
		state.png,
	}, { text = true }, function(res)
		vim.schedule(function()
			state.rendering = false
			if res.code ~= 0 then
				vim.notify("vv --render 失敗:\n" .. (res.stderr or ""), vim.log.levels.ERROR)
				return
			end
			if not is_open() then
				return
			end
			-- 旧フレームを消してから新フレームを描く（同 ID の placement 累積を防ぐ）。保存毎の
			-- 低頻度更新なので一瞬の切り替えは許容。
			term.clear(state.image_id)
			local row, col, c, r = geom()
			term.show(state.png, state.image_id, row, col, c, r)
		end)
	end)
end

local function setup_autocmds()
	local grp = vim.api.nvim_create_augroup("VvPreview", { clear = true })
	-- ソース保存で再描画（Typst ライブプレビュー）。
	vim.api.nvim_create_autocmd("BufWritePost", {
		group = grp,
		callback = function(ev)
			if state.file and vim.fn.fnamemodify(ev.match, ":p") == state.file then
				M.render()
			end
		end,
	})
	-- リサイズ/レイアウト変化で描き直す。
	vim.api.nvim_create_autocmd({ "VimResized", "WinResized" }, {
		group = grp,
		callback = function()
			if is_open() then
				M.render()
			end
		end,
	})
	-- フロートが閉じたら画像を消す。
	vim.api.nvim_create_autocmd("WinClosed", {
		group = grp,
		callback = function(ev)
			if state.win and tonumber(ev.match) == state.win then
				term.clear(state.image_id)
				state.win = nil
			end
		end,
	})
	-- 終了時に画像を端末へ残さない。
	vim.api.nvim_create_autocmd("VimLeavePre", {
		group = grp,
		callback = function()
			term.clear(state.image_id)
		end,
	})
end

-- プレビューを開く（引数なしなら現在バッファのファイル）。
function M.open(file)
	file = file
	if not file or file == "" then
		file = vim.api.nvim_buf_get_name(0)
	end
	if not file or file == "" then
		vim.notify("vv: 対象ファイルがありません", vim.log.levels.WARN)
		return
	end
	file = vim.fn.fnamemodify(file, ":p")
	local ext = (file:match("%.([%w]+)$") or ""):lower()
	if not SUPPORTED[ext] then
		vim.notify(string.format("vv: 非対応のファイル (.%s)。svg / typ / pdf のみ。", ext), vim.log.levels.WARN)
		return
	end

	state.file = file
	state.page = 1
	state.zoom = 100

	if not is_open() then
		state.buf = vim.api.nvim_create_buf(false, true)
		vim.bo[state.buf].bufhidden = "wipe"
		local total_w, total_h = vim.o.columns, vim.o.lines
		local width = math.max(10, math.floor(total_w * config.width))
		local height = math.max(3, total_h - 2)
		state.win = vim.api.nvim_open_win(state.buf, false, {
			relative = "editor",
			row = 0,
			col = total_w - width,
			width = width,
			height = height,
			style = "minimal",
			border = "none",
			focusable = false,
		})
		-- 末尾 '~' を消し、空セルが画像を邪魔しないようにする。
		vim.wo[state.win].fillchars = "eob: "
		setup_autocmds()
	end
	M.render()
end

function M.close()
	term.clear(state.image_id)
	if is_open() then
		vim.api.nvim_win_close(state.win, true)
	end
	state.win = nil
	state.file = nil
end

function M.toggle()
	if is_open() then
		M.close()
	else
		M.open()
	end
end

function M.next_page()
	state.page = state.page + 1
	M.render()
end

function M.prev_page()
	if state.page > 1 then
		state.page = state.page - 1
		M.render()
	end
end

function M.zoom_in()
	state.zoom = math.min(1600, math.floor(state.zoom * 3 / 2))
	M.render()
end

function M.zoom_out()
	state.zoom = math.max(100, math.floor(state.zoom * 2 / 3))
	M.render()
end

-- 画像が他の描画で消された/ずれたときに手動で描き直す。
function M.refresh()
	M.render()
end

function M.setup(opts)
	config = vim.tbl_deep_extend("force", config, opts or {})
	local cmd = vim.api.nvim_create_user_command
	cmd("VV", function(a)
		M.open(a.args ~= "" and a.args or nil)
	end, { nargs = "?", complete = "file", desc = "vecview: プレビューを開く" })
	cmd("VVClose", M.close, { desc = "vecview: プレビューを閉じる" })
	cmd("VVToggle", M.toggle, { desc = "vecview: プレビュー開閉" })
	cmd("VVNext", M.next_page, { desc = "vecview: 次ページ" })
	cmd("VVPrev", M.prev_page, { desc = "vecview: 前ページ" })
	cmd("VVRefresh", M.refresh, { desc = "vecview: 再描画" })
end

return M
