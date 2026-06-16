//! Kitty Graphics Protocol バックエンド。RGBA8 を `f=32` で直接転送する。
//!
//! 2つの配置モードを持つ：
//! - **直接配置**（非 tmux）：`a=T` でカーソル位置に画像を直接表示する。
//! - **Unicode プレースホルダ**（tmux）：画像を仮想配置（`U=1`）として転送し、`U+10EEEE`
//!   プレースホルダ文字に行・列 diacritics を付けた「テキストセル」を描画する。tmux は
//!   これを通常のテキストセルとして追跡するため、画像がペイン境界内に正しく収まる。
//!   `a=T` 直接配置は tmux がグラフィクスのペイン配置を理解せず常にウィンドウ左上に
//!   出てしまうため、tmux 内ではこちらを使う。tmux 側 `set -g allow-passthrough on` が必要。

use std::io::Write;

use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use vecview_core::OutputBackend;

/// RGBA を zlib（RFC 1950）で圧縮する。kitty graphics の `o=z` で送る前段。圧縮レベルは
/// 速度優先（白背景中心のページは低レベルでも十分縮む）。Vec への書き込みは失敗しない。
fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

/// base64 ペイロードの1チャンク最大長（Kitty 推奨の 4096 バイト）。
const CHUNK: usize = 4096;

/// Unicode プレースホルダ文字 U+10EEEE。
const PLACEHOLDER: char = '\u{10EEEE}';

pub struct KittyBackend {
    /// tmux passthrough（＋プレースホルダ配置）を使うか。
    tmux: bool,
    /// この vecview インスタンス固有の画像 ID。Kitty の画像 ID は端末でグローバルなので、
    /// 複数ウィンドウで複数の vecview が動くと固定 ID では互いに上書き・誤削除し合う。PID から
    /// 一意な 24bit 値を作り、プレースホルダの前景色（24bit トゥルーカラー）で符号化する。
    image_id: u32,
}

impl KittyBackend {
    pub fn new(tmux: bool) -> Self {
        // PID を 24bit に収めて一意 ID とする（0 は避ける）。24bit なのでプレースホルダの
        // 最上位バイト用 diacritic は不要。並行する別インスタンスと衝突する確率は実質ゼロ。
        let image_id = (std::process::id() & 0x00FF_FFFF).max(1);
        Self { tmux, image_id }
    }

    /// 自インスタンスの画像を削除する APC 本体。tmux placeholder では他インスタンスの画像を
    /// 巻き込まないよう自分の ID だけ消す（d=I,i=...）。非 tmux 直接配置は単一端末でグローバル
    /// 衝突しないため全削除でよい（自動採番 ID を使い ID を持たないため）。
    fn delete_body(&self) -> Vec<u8> {
        if self.tmux {
            format!("_Ga=d,d=I,i={}", self.image_id).into_bytes()
        } else {
            b"_Ga=d".to_vec()
        }
    }

    /// APC グラフィクスシーケンスを（必要なら tmux ラップして）書き出す。
    /// `body` は `_G...` 部分（先頭 ESC と終端 ST を含まない中身）。
    fn write_apc(&self, out: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
        if self.tmux {
            out.write_all(b"\x1bPtmux;")?;
            // 内側 APC シーケンス全体（ESC 含む）の ESC を二重化して埋め込む。
            let mut seq = Vec::with_capacity(body.len() + 4);
            seq.extend_from_slice(b"\x1b");
            seq.extend_from_slice(body);
            seq.extend_from_slice(b"\x1b\\");
            for &b in &seq {
                if b == 0x1b {
                    out.write_all(b"\x1b\x1b")?;
                } else {
                    out.write_all(&[b])?;
                }
            }
            out.write_all(b"\x1b\\")?;
        } else {
            out.write_all(b"\x1b")?;
            out.write_all(body)?;
            out.write_all(b"\x1b\\")?;
        }
        Ok(())
    }

    /// 画像データを 4096 バイト区切りで転送する。`first_control` は先頭チャンクに付ける
    /// 制御キー列（例: `a=T,f=32,s=W,v=H`）。
    fn transmit(&self, out: &mut impl Write, rgba: &[u8], first_control: &str) -> std::io::Result<()> {
        // RGBA は大半が同色（白背景）になりがちなので zlib 圧縮（o=z）が劇的に効く。
        // s/v（画素寸法）は非圧縮サイズのまま。tmux passthrough 経由の転送量を大きく削減する。
        let payload = STANDARD.encode(zlib_compress(rgba));
        let bytes = payload.as_bytes();
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK).collect();
        let last = chunks.len().saturating_sub(1);
        for (i, chunk) in chunks.iter().enumerate() {
            let more = if i == last { 0 } else { 1 };
            let mut body = Vec::new();
            if i == 0 {
                write!(body, "_G{first_control},o=z,m={more};")?;
            } else {
                write!(body, "_Gm={more};")?;
            }
            body.extend_from_slice(chunk);
            self.write_apc(out, &body)?;
        }
        Ok(())
    }

    /// 直接配置（非 tmux）：前画像を削除→クリア→カーソル位置に表示。
    fn display_direct(&self, out: &mut impl Write, rgba: &[u8], w: u32, h: u32) -> std::io::Result<()> {
        let del = self.delete_body();
        self.write_apc(out, &del)?;
        out.write_all(b"\x1b[2J\x1b[H")?;
        self.transmit(out, rgba, &format!("a=T,q=2,f=32,s={w},v={h}"))?;
        Ok(())
    }

    /// Unicode プレースホルダ配置（tmux）：画像を仮想配置として転送し、プレースホルダ
    /// セルを描画する。セルは通常テキストなので tmux がペイン内に正しく配置する。
    fn display_placeholder(&self, out: &mut impl Write, rgba: &[u8], w: u32, h: u32) -> std::io::Result<()> {
        let (cols, rows) = cell_footprint(w, h);

        // 画面（ペイン）をクリアし左上へ。これで前フレームのプレースホルダセルも消える。
        out.write_all(b"\x1b[2J\x1b[H")?;

        // 前フレームの自分の画像とその placement を削除して解放する（d=I,i=自ID）。2J はテキスト
        // しか消さず Kitty 画像/placement は残るため、削除しないと再描画のたびに端末側へ画像が
        // 溜まり続け、copy mode のキャレット連打などでメモリが膨張して端末が落ちる。自 ID だけ
        // 消すので、別ウィンドウの別 vecview の画像は巻き込まない。
        let del = self.delete_body();
        self.write_apc(out, &del)?;

        // 仮想配置（U=1）として転送。c/r が画像の占有セル数。i は自インスタンス固有 ID。
        let id = self.image_id;
        self.transmit(
            out,
            rgba,
            &format!("a=T,q=2,U=1,i={id},f=32,s={w},v={h},c={cols},r={rows}"),
        )?;

        // 前景色で画像 ID を 24bit トゥルーカラーとして符号化（id = r<<16 | g<<8 | b）。
        let (r, g, b) = ((id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
        write!(out, "\x1b[38;2;{r};{g};{b}m")?;
        let mut buf = [0u8; 4];
        for y in 0..rows {
            // 行頭へ（tmux ではペイン相対のカーソル移動）。
            write!(out, "\x1b[{};1H", y + 1)?;
            for x in 0..cols {
                out.write_all(PLACEHOLDER.encode_utf8(&mut buf).as_bytes())?;
                out.write_all(diacritic(y).encode_utf8(&mut buf).as_bytes())?;
                out.write_all(diacritic(x).encode_utf8(&mut buf).as_bytes())?;
            }
        }
        out.write_all(b"\x1b[0m")?;
        Ok(())
    }
}

impl OutputBackend for KittyBackend {
    fn name(&self) -> &str {
        if self.tmux {
            "kitty (tmux placeholder)"
        } else {
            "kitty"
        }
    }

    fn is_supported(&self) -> bool {
        true
    }

    fn enter(&self) -> Result<()> {
        // 代替スクリーンへ切替 + カーソル非表示。終了時に元の画面が復元され、描画内容は残らない。
        let mut out = std::io::stdout().lock();
        out.write_all(b"\x1b[?1049h\x1b[?25l")?;
        out.flush()?;
        Ok(())
    }

    fn leave(&self) -> Result<()> {
        let mut out = std::io::stdout().lock();
        // 転送済みの自分の画像を削除してから、カーソル表示・代替スクリーン解除。自 ID だけ消すので
        // 別ウィンドウの別 vecview の画像は残る。
        let del = self.delete_body();
        self.write_apc(&mut out, &del)?;
        out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l")?;
        out.flush()?;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        // 画面クリアだけでは Kitty 画像が残るため、先に自分の画像を削除する（他インスタンスは温存）。
        let mut out = std::io::stdout().lock();
        let del = self.delete_body();
        self.write_apc(&mut out, &del)?;
        out.write_all(b"\x1b[2J\x1b[H")?;
        out.flush()?;
        Ok(())
    }

    fn display(&self, rgba: &[u8], width: u32, height: u32) -> Result<()> {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if self.tmux {
            self.display_placeholder(&mut out, rgba, width, height)?;
        } else {
            self.display_direct(&mut out, rgba, width, height)?;
        }
        out.flush()?;
        Ok(())
    }
}

/// 画像が占有すべきセル数（列・行）を端末のセルサイズから求める。diacritics 表のサイズと
/// 端末のセル数で上限を切る。
fn cell_footprint(w: u32, h: u32) -> (u32, u32) {
    let (cell_w, cell_h, max_cols, max_rows) = match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 => (
            (ws.width as u32 / ws.columns as u32).max(1),
            (ws.height as u32 / ws.rows as u32).max(1),
            ws.columns as u32,
            ws.rows as u32,
        ),
        Ok(ws) if ws.columns > 0 && ws.rows > 0 => (8, 16, ws.columns as u32, ws.rows as u32),
        _ => (8, 16, 80, 24),
    };
    let cols = (w / cell_w)
        .max(1)
        .min(max_cols)
        .min(DIACRITICS.len() as u32);
    let rows = (h / cell_h)
        .max(1)
        .min(max_rows)
        .min(DIACRITICS.len() as u32);
    (cols, rows)
}

/// n 番目の row/column diacritic を返す（範囲外は末尾にクランプ）。
fn diacritic(n: u32) -> char {
    let idx = (n as usize).min(DIACRITICS.len() - 1);
    char::from_u32(DIACRITICS[idx]).unwrap_or('\u{0305}')
}

/// Kitty の row/column diacritics 表（`gen/rowcolumn-diacritics.txt`）。N 番目の diacritic が
/// 値 N（行番号・列番号）を表す。
const DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];
