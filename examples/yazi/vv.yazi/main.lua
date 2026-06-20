--- vv.yazi — SVG / Typst / PDF を vecview（`vv --render`）でプレビューする yazi previewer。
---
--- `vv --render <file> --size WxH --page N -o <cache>.png` で1ページを PNG に描き、
--- それを `ya.image_show` でプレビューペインに表示する。Typst は yazi がネイティブ対応しないため
--- ここが主な価値。描画は vv 本体と同じ経路（PDF=pdfium、SVG/Typst=wgpu）。
---
--- 必要: PATH に `vv`（本リポジトリの vecview）。yazi 26 以降（`ya.mgr_emit` を使用）。

local M = {}

-- レンダリング解像度（px）。yazi のプレビュー最大寸法に合わせ、取得できなければ既定値。
-- 大きめに描いて `ya.image_show` がペインへ縮小フィットする。
local function render_size()
	local w, h = 1000, 1400
	pcall(function()
		if rt and rt.preview then
			if type(rt.preview.max_width) == "number" and rt.preview.max_width > 0 then
				w = rt.preview.max_width
			end
			if type(rt.preview.max_height) == "number" and rt.preview.max_height > 0 then
				h = rt.preview.max_height
			end
		end
	end)
	return string.format("%dx%d", w, h)
end

function M:peek(job)
	local cache = ya.file_cache(job)
	if not cache then
		return
	end

	-- キャッシュが無ければ vv で1ページ（skip+1）を PNG 化する。`vv` は PATH のバイナリを
	-- 直接起動するので、cellpx 補正用シェル関数は経由しない（--render は端末非依存で不要）。
	if not fs.cha(cache) then
		local output = Command("vv")
			:arg({
				"--render",
				tostring(job.file.url),
				"--size",
				render_size(),
				"--page",
				tostring(job.skip + 1),
				"-o",
				tostring(cache),
			})
			:stdout(Command.PIPED)
			:stderr(Command.PIPED)
			:output()

		-- vv 失敗時（vv 未導入・typst 不在・描画不能など）は静かに何も表示しない。
		-- yazi 26 では ya.preview_widgets が無くエラー描画 API が異なるため、ここでは描かない。
		if not output or not output.status.success then
			return
		end
	end

	ya.image_show(cache, job.area)
end

function M:seek(job)
	-- 複数ページ（PDF / Typst）のスクロール送り。skip をスクロール量で増減して再 peek する。
	local h = cx.active.current.hovered
	if h and h.url == job.file.url then
		ya.mgr_emit("peek", {
			math.max(0, cx.active.preview.skip + job.units),
			only_if = job.file.url,
		})
	end
end

return M
