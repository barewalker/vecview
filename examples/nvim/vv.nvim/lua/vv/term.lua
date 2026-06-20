--- Terminal graphics layer: draw a PNG directly to the terminal via the Kitty graphics protocol (pure Lua, no dependencies).
---
--- nvim's :terminal (libvterm) cannot pass kitty graphics through, so we write raw bytes to the outer
--- terminal (fd 1) rather than nvim's grid. nvim 0.11 has no nvim_ui_send, so we use vim.uv's TTY
--- handle (or io.stdout if unavailable) -- the same approach as image.nvim.
--- Inside tmux, wrap in passthrough (ESC doubling + DCS wrapping).
---
--- Images are sent as "direct data transfer (t=d) split into chunks". Unlike file-path transfer (t=f),
--- this also works over SSH (where the terminal cannot read the remote's files).

local M = {}

local ESC = "\27"

-- Open fd 1 as a TTY handle once and reuse it.
local tty
local function out()
	if tty == nil then
		local ok, h = pcall(function()
			return vim.uv.new_tty(1, false)
		end)
		tty = (ok and h) or false
	end
	return tty or nil
end

local function write(bytes)
	local h = out()
	if h then
		pcall(function()
			h:write(bytes)
		end)
	else
		io.stdout:write(bytes)
	end
end

-- If inside tmux, wrap in passthrough (double the inner ESC and wrap it in \ePtmux;…\e\\).
local in_tmux = vim.env.TMUX ~= nil
local function wrap(seq)
	if not in_tmux then
		return seq
	end
	return ESC .. "Ptmux;" .. seq:gsub(ESC, ESC .. ESC) .. ESC .. "\\"
end

-- Inside tmux, passthrough cursor positioning bypasses pane translation and uses physical screen coordinates,
-- so we add this pane's top-left offset (pane_left/pane_top) to the nvim-relative coordinates to convert to physical coordinates
-- (same as image.nvim). $TMUX_PANE targets this pane for the query.
local function pane_offset()
	if not in_tmux then
		return 0, 0
	end
	local pane = vim.env.TMUX_PANE
	local args = { "tmux", "display-message", "-p" }
	if pane and pane ~= "" then
		vim.list_extend(args, { "-t", pane })
	end
	vim.list_extend(args, { "-F", "#{pane_left} #{pane_top}" })
	local out = vim.fn.system(args)
	local l, t = out:match("(%d+)%s+(%d+)")
	return tonumber(l) or 0, tonumber(t) or 0
end

--- Display a PNG file as image ID `id`, fitting it into cols×rows cells at screen cell (row,col) (1-based).
--- Direct data transfer (t=d, f=100=PNG). The terminal reads the dimensions from the PNG header.
function M.show(png_path, id, row, col, cols, rows)
	local f = io.open(png_path, "rb")
	if not f then
		return false
	end
	local data = f:read("*a")
	f:close()
	if not data or #data == 0 then
		return false
	end
	local payload = vim.base64.encode(data)

	-- Add the tmux pane offset to convert to physical screen coordinates (since passthrough uses physical coordinates).
	local off_l, off_t = pane_offset()
	row = row + off_t
	col = col + off_l

	-- Begin synchronized output -> save cursor -> move to the float's top-left (the image appears there once the transfer below completes).
	write(wrap(ESC .. "[?2026h" .. ESC .. "[s" .. ESC .. "[" .. row .. ";" .. col .. "H"))

	-- Split the base64 into 4096-byte chunks. Control keys go in the first chunk; m=1 means continue / m=0 means end.
	-- C=1 specifies that the cursor must not move. c/r scale to the float's cell counts.
	local CHUNK = 4096
	local n = #payload
	local i = 1
	local first = true
	while i <= n do
		local piece = payload:sub(i, i + CHUNK - 1)
		i = i + CHUNK
		local more = (i <= n) and 1 or 0
		local ctrl
		if first then
			ctrl = string.format("a=T,f=100,t=d,i=%d,q=2,C=1,c=%d,r=%d,m=%d", id, cols, rows, more)
			first = false
		else
			ctrl = "m=" .. more
		end
		write(wrap(ESC .. "_G" .. ctrl .. ";" .. piece .. ESC .. "\\"))
	end

	-- Restore cursor + end synchronized output.
	write(wrap(ESC .. "[u" .. ESC .. "[?2026l"))
	return true
end

--- Free image ID `id`'s image and placement (d=I = delete along with the data).
function M.clear(id)
	write(wrap(ESC .. "_Ga=d,d=I,i=" .. id .. ",q=2" .. ESC .. "\\"))
end

return M
