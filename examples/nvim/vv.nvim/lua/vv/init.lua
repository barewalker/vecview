--- vv.nvim — live preview of SVG / Typst / PDF inside nvim (pure Lua, no third-party dependencies).
---
--- Opens a floating window, renders a PNG at that window's size with `vv --render`, and overlays it via Kitty graphics.
--- `.typ` files are re-rendered on every save (BufWritePost), i.e. live preview. The display layer is term.lua
--- (drawing kitty graphics directly to the outer terminal) and does not depend on external plugins like image.nvim.
---
--- Prerequisites: `vv` on PATH, a terminal with Kitty graphics support (Ghostty / kitty). Under tmux, allow-passthrough on.

local term = require("vv.term")

local M = {}

local config = {
	-- Pixel dimensions of a terminal cell. Used as a guide for render resolution (kitty rescales to c×r cells,
	-- so it need not be exact; it only affects sharpness). Same idea as vv's VECVIEW_CELL_PX.
	cell_width = 10,
	cell_height = 20,
	width = 0.5, -- Float width (as a fraction of the full editor width). Shown tall on the right side.
	vv = "vv", -- vv executable name (on PATH).
}

local state = {
	win = nil,
	buf = nil,
	file = nil, -- absolute path
	page = 1,
	zoom = 100,
	png = nil,
	image_id = 0xB00B, -- Image ID unique to this plugin.
	rendering = false,
}

local SUPPORTED = { typ = true, svg = true, pdf = true }

local function is_open()
	return state.win ~= nil and vim.api.nvim_win_is_valid(state.win)
end

-- Float content rectangle: 1-based screen cell (row,col) and cell counts (cols,rows). Assumes border=none.
local function geom()
	local pos = vim.api.nvim_win_get_position(state.win)
	return pos[1] + 1, pos[2] + 1, vim.api.nvim_win_get_width(state.win), vim.api.nvim_win_get_height(state.win)
end

-- Render the current page to a PNG with vv --render and draw it into the float via Kitty.
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
				vim.notify("vv --render failed:\n" .. (res.stderr or ""), vim.log.levels.ERROR)
				return
			end
			if not is_open() then
				return
			end
			-- Clear the old frame before drawing the new one (prevents placements piling up under the same ID). Since
			-- updates are infrequent (once per save), a brief flicker during the swap is acceptable.
			term.clear(state.image_id)
			local row, col, c, r = geom()
			term.show(state.png, state.image_id, row, col, c, r)
		end)
	end)
end

local function setup_autocmds()
	local grp = vim.api.nvim_create_augroup("VvPreview", { clear = true })
	-- Re-render when the source is saved (Typst live preview).
	vim.api.nvim_create_autocmd("BufWritePost", {
		group = grp,
		callback = function(ev)
			if state.file and vim.fn.fnamemodify(ev.match, ":p") == state.file then
				M.render()
			end
		end,
	})
	-- Redraw on resize / layout change.
	vim.api.nvim_create_autocmd({ "VimResized", "WinResized" }, {
		group = grp,
		callback = function()
			if is_open() then
				M.render()
			end
		end,
	})
	-- Clear the image when the float is closed.
	vim.api.nvim_create_autocmd("WinClosed", {
		group = grp,
		callback = function(ev)
			if state.win and tonumber(ev.match) == state.win then
				term.clear(state.image_id)
				state.win = nil
			end
		end,
	})
	-- Do not leave the image on the terminal on exit.
	vim.api.nvim_create_autocmd("VimLeavePre", {
		group = grp,
		callback = function()
			term.clear(state.image_id)
		end,
	})
end

-- Open the preview (uses the current buffer's file if no argument is given).
function M.open(file)
	file = file
	if not file or file == "" then
		file = vim.api.nvim_buf_get_name(0)
	end
	if not file or file == "" then
		vim.notify("vv: no target file", vim.log.levels.WARN)
		return
	end
	file = vim.fn.fnamemodify(file, ":p")
	local ext = (file:match("%.([%w]+)$") or ""):lower()
	if not SUPPORTED[ext] then
		vim.notify(string.format("vv: unsupported file (.%s). svg / typ / pdf only.", ext), vim.log.levels.WARN)
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
		-- Remove the trailing '~' so empty cells don't interfere with the image.
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

-- Manually redraw when the image has been erased or displaced by other rendering.
function M.refresh()
	M.render()
end

function M.setup(opts)
	config = vim.tbl_deep_extend("force", config, opts or {})
	local cmd = vim.api.nvim_create_user_command
	cmd("VV", function(a)
		M.open(a.args ~= "" and a.args or nil)
	end, { nargs = "?", complete = "file", desc = "vecview: open preview" })
	cmd("VVClose", M.close, { desc = "vecview: close preview" })
	cmd("VVToggle", M.toggle, { desc = "vecview: toggle preview" })
	cmd("VVNext", M.next_page, { desc = "vecview: next page" })
	cmd("VVPrev", M.prev_page, { desc = "vecview: previous page" })
	cmd("VVRefresh", M.refresh, { desc = "vecview: redraw" })
end

return M
