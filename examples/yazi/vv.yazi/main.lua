--- vv.yazi — a yazi previewer that previews SVG / Typst / PDF via vecview (`vv --render`).
---
--- `vv --render <file> --size WxH --page N -o <cache>.png` renders one page to a PNG,
--- which is then displayed in the preview pane with `ya.image_show`. Since yazi has no native
--- Typst support, that is the main value here. Rendering uses the same path as vv itself (PDF=pdfium, SVG/Typst=wgpu).
---
--- Requires: `vv` (the vecview from this repo) on PATH. yazi 26 or newer (uses `ya.mgr_emit`).

local M = {}

-- Rendering resolution (px). Matches yazi's maximum preview dimensions, falling back to defaults if unavailable.
-- Render large and let `ya.image_show` scale it down to fit the pane.
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

	-- If there is no cache, use vv to render one page (skip+1) to a PNG. `vv` launches the binary on PATH
	-- directly, so it does not go through the cellpx-correction shell function (--render is terminal-independent and doesn't need it).
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

		-- If vv fails (vv not installed, typst missing, render failure, etc.), silently display nothing.
		-- yazi 26 has no ya.preview_widgets and its error-drawing API differs, so we draw nothing here.
		if not output or not output.status.success then
			return
		end
	end

	ya.image_show(cache, job.area)
end

function M:seek(job)
	-- Scroll through multi-page documents (PDF / Typst). Adjust skip by the scroll amount and peek again.
	local h = cx.active.current.hovered
	if h and h.url == job.file.url then
		ya.mgr_emit("peek", {
			math.max(0, cx.active.preview.skip + job.units),
			only_if = job.file.url,
		})
	end
end

return M
