//! Linux Framebuffer バックエンド（`/dev/fb0`）。ネイティブ解像度で直接描画する。
//!
//! ベクター品質が最も活きる経路。`ioctl` で解像度・ビット深度・stride（`line_length`）と
//! ピクセルフォーマット（R/G/B のビット位置）を取得し、`mmap` でフレームバッファに
//! RGBA を変換して書き込む。
//!
//! 実機（bare TTY 上の `/dev/fb0`）でのみ表示確認できるため、ピクセル変換ロジックは
//! [`blit`] という純粋関数に切り出してユニットテスト可能にしている。

use anyhow::{anyhow, Result};
use vecview_core::OutputBackend;

/// フレームバッファのピクセル配置情報（`ioctl` 取得値から必要分のみ抽出）。
#[derive(Clone, Copy, Debug)]
pub struct FbInfo {
    pub xres: u32,
    pub yres: u32,
    pub bits_per_pixel: u32,
    /// 1行あたりのバイト数（stride）。`xres * bpp/8` とは限らない。
    pub line_length: u32,
    pub red_offset: u32,
    pub green_offset: u32,
    pub blue_offset: u32,
    pub transp_offset: u32,
    pub transp_length: u32,
}

/// RGBA 画像を `dst`（フレームバッファ相当のバイト列）へ左上基準で書き込む純粋関数。
/// stride とピクセルフォーマット（R/G/B/A のビット位置）を考慮する。32bpp / 24bpp 対応。
pub fn blit(dst: &mut [u8], rgba: &[u8], img_w: u32, img_h: u32, fb: &FbInfo) {
    let bytes_per_pixel = (fb.bits_per_pixel / 8) as usize;
    if bytes_per_pixel < 3 {
        return; // 16bpp 等は未対応（初回スコープ外）。
    }
    let copy_w = img_w.min(fb.xres) as usize;
    let copy_h = img_h.min(fb.yres) as usize;
    let stride = fb.line_length as usize;

    for y in 0..copy_h {
        for x in 0..copy_w {
            let src = (y * img_w as usize + x) * 4;
            let (r, g, b, a) = (rgba[src], rgba[src + 1], rgba[src + 2], rgba[src + 3]);
            let dst_off = y * stride + x * bytes_per_pixel;
            if dst_off + bytes_per_pixel > dst.len() {
                continue;
            }
            // 各成分をビットオフセット位置（/8 でバイト位置）へ配置。
            dst[dst_off + (fb.red_offset / 8) as usize] = r;
            dst[dst_off + (fb.green_offset / 8) as usize] = g;
            dst[dst_off + (fb.blue_offset / 8) as usize] = b;
            if bytes_per_pixel >= 4 && fb.transp_length > 0 {
                dst[dst_off + (fb.transp_offset / 8) as usize] = a;
            }
        }
    }
}

pub struct FramebufferBackend {
    path: String,
}

impl FramebufferBackend {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
        }
    }
}

impl OutputBackend for FramebufferBackend {
    fn name(&self) -> &str {
        "framebuffer"
    }

    fn is_supported(&self) -> bool {
        std::path::Path::new(&self.path).exists()
    }

    #[cfg(target_os = "linux")]
    fn display(&self, rgba: &[u8], width: u32, height: u32) -> Result<()> {
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| anyhow!("{} を開けません（video グループ/権限を確認）: {e}", self.path))?;

        let fb = unsafe { read_fb_info(file.as_raw_fd()) }?;

        let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file)? };
        blit(&mut mmap, rgba, width, height, &fb);
        mmap.flush()?;
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn display(&self, _rgba: &[u8], _width: u32, _height: u32) -> Result<()> {
        Err(anyhow!("framebuffer は Linux でのみ利用可能"))
    }
}

#[cfg(target_os = "linux")]
unsafe fn read_fb_info(fd: std::os::fd::RawFd) -> Result<FbInfo> {
    let mut var = std::mem::zeroed::<linux::fb_var_screeninfo>();
    let mut fix = std::mem::zeroed::<linux::fb_fix_screeninfo>();
    linux::fbioget_vscreeninfo(fd, &mut var).map_err(|e| anyhow!("FBIOGET_VSCREENINFO: {e}"))?;
    linux::fbioget_fscreeninfo(fd, &mut fix).map_err(|e| anyhow!("FBIOGET_FSCREENINFO: {e}"))?;
    Ok(FbInfo {
        xres: var.xres,
        yres: var.yres,
        bits_per_pixel: var.bits_per_pixel,
        line_length: fix.line_length,
        red_offset: var.red.offset,
        green_offset: var.green.offset,
        blue_offset: var.blue.offset,
        transp_offset: var.transp.offset,
        transp_length: var.transp.length,
    })
}

#[cfg(target_os = "linux")]
mod linux {
    //! `<linux/fb.h>` の構造体と ioctl 定義（必要分）。

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct fb_bitfield {
        pub offset: u32,
        pub length: u32,
        pub msb_right: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct fb_var_screeninfo {
        pub xres: u32,
        pub yres: u32,
        pub xres_virtual: u32,
        pub yres_virtual: u32,
        pub xoffset: u32,
        pub yoffset: u32,
        pub bits_per_pixel: u32,
        pub grayscale: u32,
        pub red: fb_bitfield,
        pub green: fb_bitfield,
        pub blue: fb_bitfield,
        pub transp: fb_bitfield,
        pub nonstd: u32,
        pub activate: u32,
        pub height: u32,
        pub width: u32,
        pub accel_flags: u32,
        pub pixclock: u32,
        pub left_margin: u32,
        pub right_margin: u32,
        pub upper_margin: u32,
        pub lower_margin: u32,
        pub hsync_len: u32,
        pub vsync_len: u32,
        pub sync: u32,
        pub vmode: u32,
        pub rotate: u32,
        pub colorspace: u32,
        pub reserved: [u32; 4],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct fb_fix_screeninfo {
        pub id: [u8; 16],
        pub smem_start: usize,
        pub smem_len: u32,
        pub type_: u32,
        pub type_aux: u32,
        pub visual: u32,
        pub xpanstep: u16,
        pub ypanstep: u16,
        pub ywrapstep: u16,
        pub line_length: u32,
        pub mmio_start: usize,
        pub mmio_len: u32,
        pub accel: u32,
        pub capabilities: u16,
        pub reserved: [u16; 2],
    }

    // FBIOGET_VSCREENINFO = 0x4600 / FBIOGET_FSCREENINFO = 0x4602（レガシー番号）。
    nix::ioctl_read_bad!(fbioget_vscreeninfo, 0x4600, fb_var_screeninfo);
    nix::ioctl_read_bad!(fbioget_fscreeninfo, 0x4602, fb_fix_screeninfo);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra_info(xres: u32, yres: u32, line_length: u32) -> FbInfo {
        // 一般的な BGRA32: B=offset0, G=8, R=16, A=24。
        FbInfo {
            xres,
            yres,
            bits_per_pixel: 32,
            line_length,
            red_offset: 16,
            green_offset: 8,
            blue_offset: 0,
            transp_offset: 24,
            transp_length: 8,
        }
    }

    #[test]
    fn blit_bgra_respects_offsets() {
        let fb = bgra_info(2, 2, 8); // stride == width*4
        let mut dst = vec![0u8; 16];
        // 1ピクセル目を RGBA(10,20,30,40)。
        let rgba = vec![10, 20, 30, 40, /* 残り3px */ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        blit(&mut dst, &rgba, 2, 2, &fb);
        // BGRA 並び: dst[0]=B(30), [1]=G(20), [2]=R(10), [3]=A(40)。
        assert_eq!(&dst[0..4], &[30, 20, 10, 40]);
    }

    #[test]
    fn blit_honors_stride() {
        // stride が幅より大きい場合（line_length=12 > 2px*4=8）、2行目は offset 12 から。
        let fb = bgra_info(2, 2, 12);
        let mut dst = vec![0u8; 24];
        let mut rgba = vec![0u8; 16];
        // (x=0,y=1) を赤(255,0,0,255)。インデックス = (y*width + x) * 4 = (1*2+0)*4 = 8。
        let src = 8usize;
        rgba[src] = 255;
        rgba[src + 3] = 255;
        blit(&mut dst, &rgba, 2, 2, &fb);
        // 2行目先頭は stride=12 の位置。R は blue offset0 ではなく red_offset16→ byte2。
        assert_eq!(dst[12 + 2], 255); // R
        assert_eq!(dst[12 + 3], 255); // A
    }

    #[test]
    fn blit_clips_oversize_image() {
        // 画像 4x4 をフレームバッファ 2x2 にクリップ。境界外書き込みでパニックしない。
        let fb = bgra_info(2, 2, 8);
        let mut dst = vec![0u8; 16];
        let rgba = vec![100u8; 4 * 4 * 4];
        blit(&mut dst, &rgba, 4, 4, &fb);
        assert_eq!(dst[0], 100);
    }
}
