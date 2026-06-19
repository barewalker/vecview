--- 端末グラフィクス層：Kitty graphics protocol で PNG を端末へ直接描く（純 Lua・依存なし）。
---
--- nvim の :terminal(libvterm) は kitty graphics を通せないため、nvim のグリッドではなく
--- 外側端末（fd 1）へ生バイトを書く。nvim 0.11 には nvim_ui_send が無いので vim.uv の TTY
--- ハンドル（無ければ io.stdout）を使う ── これは image.nvim と同じ方式。
--- tmux 内では passthrough（ESC 二重化 + DCS 包み）でラップする。
---
--- 画像は「直接データ転送（t=d）をチャンク分割」で送る。ファイルパス転送（t=f）と違い、
--- SSH 越し（端末が remote のファイルを読めない）でも動く。

local M = {}

local ESC = "\27"

-- fd 1 を TTY ハンドルとして一度だけ開いて使い回す。
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

-- tmux 内なら passthrough でラップ（内側 ESC を二重化し \ePtmux;…\e\\ で包む）。
local in_tmux = vim.env.TMUX ~= nil
local function wrap(seq)
	if not in_tmux then
		return seq
	end
	return ESC .. "Ptmux;" .. seq:gsub(ESC, ESC .. ESC) .. ESC .. "\\"
end

--- PNG ファイルを画像 ID `id` として、スクリーンセル (row,col)（1始まり）へ cols×rows セルに
--- 収めて表示する。直接データ転送（t=d, f=100=PNG）。寸法は PNG ヘッダから端末が読む。
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

	-- 同期出力開始 → カーソル保存 → フロート左上へ移動（この後の転送完了時にそこへ表示される）。
	write(wrap(ESC .. "[?2026h" .. ESC .. "[s" .. ESC .. "[" .. row .. ";" .. col .. "H"))

	-- base64 を 4096 バイトごとに分割。最初のチャンクに制御キー、m=1 継続 / m=0 終端。
	-- C=1 はカーソルを動かさない指定。c/r でフロートのセル数へスケールさせる。
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

	-- カーソル復帰 + 同期出力終了。
	write(wrap(ESC .. "[u" .. ESC .. "[?2026l"))
	return true
end

--- 画像 ID `id` の画像と placement を解放する（d=I=データごと削除）。
function M.clear(id)
	write(wrap(ESC .. "_Ga=d,d=I,i=" .. id .. ",q=2" .. ESC .. "\\"))
end

return M
