//! 自绘小工具（设置窗口用）：32 位 DIB 后台缓冲 + 抗锯齿圆角矩形、渐变、柔和投影、描边、虚线、线段 / 圆弧（画图标）。
//! 文字用 GDI 直接画到同一块缓冲上（窗口不透明，ClearType 可用）。
//!
//! 坐标：调用方给逻辑像素（96 DPI 下的 px），Painter 乘 s 换成物理像素；距离场按物理像素算，边缘 1 像素抗锯齿。
//! 颜色在 sRGB 里直接混合（和浏览器的 CSS 一样），所以照着设计稿的 HTML 取值就能对上。
//! 画像素前先 GdiFlush，GDI 文字和像素绘制可以任意交错（后画的盖住先画的）。

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection,
    CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetTextExtentPoint32W, GetTextFaceW,
    GetTextMetricsW, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW,
    TRANSPARENT, TextOutW,
};
use windows::Win32::Foundation::COLORREF;
use windows::core::PCWSTR;

// ───────────────────────── 颜色 ─────────────────────────

/// sRGB 颜色，分量 0..1（a = 不透明度）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// 0xRRGGBB → 不透明颜色。
pub const fn hex(v: u32) -> Rgba {
    Rgba { r: ((v >> 16) & 255) as f32 / 255.0, g: ((v >> 8) & 255) as f32 / 255.0, b: (v & 255) as f32 / 255.0, a: 1.0 }
}

/// 0xRRGGBB + 不透明度。
pub const fn hexa(v: u32, a: f32) -> Rgba {
    let c = hex(v);
    Rgba { a, ..c }
}

impl Rgba {
    pub fn alpha(self, a: f32) -> Rgba {
        Rgba { a: self.a * a, ..self }
    }
    /// 线性插值（含透明度）。
    pub fn lerp(self, o: Rgba, t: f32) -> Rgba {
        Rgba {
            r: self.r + (o.r - self.r) * t,
            g: self.g + (o.g - self.g) * t,
            b: self.b + (o.b - self.b) * t,
            a: self.a + (o.a - self.a) * t,
        }
    }
    pub fn colorref(self) -> COLORREF {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        COLORREF(q(self.r) | q(self.g) << 8 | q(self.b) << 16)
    }
}

// ───────────────────────── 缓冲 ─────────────────────────

/// 一块 32 位自顶向下的 DIB，选进自己的内存 DC。
pub struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    pub w: i32,
    pub h: i32,
}

impl Surface {
    pub fn new(w: i32, h: i32) -> Option<Surface> {
        let (w, h) = (w.max(1), h.max(1));
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = match CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) if !bits.is_null() => b,
                Ok(b) => {
                    let _ = DeleteObject(b.into());
                    let _ = DeleteDC(dc);
                    return None;
                }
                Err(_) => {
                    let _ = DeleteDC(dc);
                    return None;
                }
            };
            let old = SelectObject(dc, bmp.into());
            SetBkMode(dc, TRANSPARENT);
            Some(Surface { dc, bmp, old, bits: bits as *mut u32, w, h })
        }
    }

    pub fn blit(&self, dst: HDC) {
        unsafe {
            let _ = GdiFlush();
            let _ = BitBlt(dst, 0, 0, self.w, self.h, Some(self.dc), 0, 0, SRCCOPY);
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize)
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// ───────────────────────── 字体 ─────────────────────────

/// GDI 字体 + 行高（逻辑像素）。
pub struct Font {
    pub h: HFONT,
    /// 字符单元高度（ascent + descent），逻辑像素
    pub cell: f32,
}

impl Font {
    /// faces：按顺序找第一个装了的字体；px：字号（逻辑像素，CSS 的 font-size）；s：DPI 缩放。
    pub fn new(faces: &[&str], px: f32, weight: i32, s: f32) -> Font {
        let mut chosen = None;
        for (i, face) in faces.iter().enumerate() {
            let f = create_font(face, px, weight, s);
            if i + 1 == faces.len() || face_is(f, face) {
                chosen = Some(f);
                break;
            }
            unsafe {
                let _ = DeleteObject(f.into());
            }
        }
        let h = chosen.unwrap_or_else(|| create_font("Microsoft YaHei UI", px, weight, s));
        let cell = unsafe {
            let dc = CreateCompatibleDC(None);
            let old = SelectObject(dc, h.into());
            let mut tm = TEXTMETRICW::default();
            let _ = GetTextMetricsW(dc, &mut tm);
            SelectObject(dc, old);
            let _ = DeleteDC(dc);
            tm.tmHeight as f32 / s
        };
        Font { h, cell }
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.h.into());
        }
    }
}

fn create_font(face: &str, px: f32, weight: i32, s: f32) -> HFONT {
    let w: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFontW(
            -((px * s).round() as i32).max(1),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            PCWSTR(w.as_ptr()),
        )
    }
}

/// 字体映射后的实际字体名是不是 face（没装时 GDI 会悄悄换成别的）。
fn face_is(f: HFONT, face: &str) -> bool {
    unsafe {
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, f.into());
        let mut buf = [0u16; 64];
        let n = GetTextFaceW(dc, Some(&mut buf)).max(0) as usize;
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let got = String::from_utf16_lossy(&buf[..n.min(64)]);
        let got = got.trim_end_matches('\0');
        got.eq_ignore_ascii_case(face)
    }
}

/// 文字宽度（逻辑像素）。dc 任意（只用来量）。
pub fn text_w(dc: HDC, font: &Font, s: f32, text: &str) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let w: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        let old = SelectObject(dc, font.h.into());
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &w, &mut sz);
        SelectObject(dc, old);
        sz.cx as f32 / s
    }
}

// ───────────────────────── 绘制 ─────────────────────────

/// 一条路径段（图标用），坐标为逻辑像素。
#[derive(Clone, Copy)]
pub enum Seg {
    Line(f32, f32, f32, f32),
    /// 圆心、半径、起止角（弧度，y 向下，顺时针为正）
    Arc(f32, f32, f32, f32, f32),
}

pub struct Painter<'a> {
    surf: &'a mut Surface,
    /// 逻辑像素 → 物理像素
    pub s: f32,
    /// 全局不透明度（拖动中的屏幕 94%）
    pub alpha: f32,
}

/// shade 的“内部块”。
enum Hole<'g> {
    None,
    Skip(Option<[usize; 4]>),
    /// 整块铺色，颜色由 g(物理 x, 物理 y) 给出
    Fill(Option<[usize; 4]>, &'g dyn Fn(f32, f32) -> Rgba),
    /// 整块铺纯色
    Solid(Option<[usize; 4]>, Rgba),
}

/// 圆角矩形（物理像素：中心、半宽高、圆角）里距边至少 m 的那块矩形（像素下标 [x0, y0, x1, y1)）。
fn inner(cx: f32, cy: f32, hw: f32, hh: f32, r: f32, m: f32) -> Option<[usize; 4]> {
    let my = m.max(r);
    let x0 = (cx - hw + m - 0.5).ceil().max(0.0);
    let x1 = (cx + hw - m - 0.5).floor() + 1.0;
    let y0 = (cy - hh + my - 0.5).ceil().max(0.0);
    let y1 = (cy + hh - my - 0.5).floor() + 1.0;
    if x1 > x0 && y1 > y0 { Some([x0 as usize, y0 as usize, x1 as usize, y1 as usize]) } else { None }
}

#[inline]
fn pack(c: Rgba) -> u32 {
    0xFF00_0000 | ((c.r * 255.0 + 0.5) as u32) << 16 | ((c.g * 255.0 + 0.5) as u32) << 8 | (c.b * 255.0 + 0.5) as u32
}

#[inline]
fn blend(p: &mut u32, c: Rgba, a: f32) {
    if a <= 0.0 {
        return;
    }
    if a >= 0.999 {
        *p = pack(c);
        return;
    }
    let d = *p;
    let dr = ((d >> 16) & 255) as f32;
    let dg = ((d >> 8) & 255) as f32;
    let db = (d & 255) as f32;
    let r = dr + (c.r * 255.0 - dr) * a;
    let g = dg + (c.g * 255.0 - dg) * a;
    let b = db + (c.b * 255.0 - db) * a;
    *p = 0xFF00_0000 | ((r + 0.5) as u32) << 16 | ((g + 0.5) as u32) << 8 | (b + 0.5) as u32;
}

/// 圆角矩形有向距离（中心 c，半宽高 hw/hh，圆角 r），外正内负。
#[inline]
pub fn sd_rrect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let r = r.min(hw).min(hh).max(0.0);
    let qx = (x - cx).abs() - hw + r;
    let qy = (y - cy).abs() - hh + r;
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// 误差函数补（Abramowitz–Stegun 7.1.26）。
#[inline]
fn erfc(x: f32) -> f32 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let y = t * (0.254_829_6 + t * (-0.284_496_74 + t * (1.421_413_8 + t * (-1.453_152 + t * 1.061_405_4)))) * (-z * z).exp();
    if x >= 0.0 { y } else { 2.0 - y }
}

/// 点到线段的距离。
#[inline]
fn d_seg(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let l2 = dx * dx + dy * dy;
    let t = if l2 > 0.0 { (((px - x0) * dx + (py - y0) * dy) / l2).clamp(0.0, 1.0) } else { 0.0 };
    let (ex, ey) = (px - x0 - dx * t, py - y0 - dy * t);
    (ex * ex + ey * ey).sqrt()
}

/// 点到圆弧（a0 → a1 顺时针）的距离。
#[inline]
fn d_arc(px: f32, py: f32, cx: f32, cy: f32, r: f32, a0: f32, a1: f32) -> f32 {
    use std::f32::consts::TAU;
    let (dx, dy) = (px - cx, py - cy);
    let ang = dy.atan2(dx);
    let span = (a1 - a0).rem_euclid(TAU);
    let rel = (ang - a0).rem_euclid(TAU);
    if rel <= span {
        ((dx * dx + dy * dy).sqrt() - r).abs()
    } else {
        let e0 = (cx + r * a0.cos(), cy + r * a0.sin());
        let e1 = (cx + r * a1.cos(), cy + r * a1.sin());
        let d0 = ((px - e0.0).powi(2) + (py - e0.1).powi(2)).sqrt();
        let d1 = ((px - e1.0).powi(2) + (py - e1.1).powi(2)).sqrt();
        d0.min(d1)
    }
}

impl<'a> Painter<'a> {
    pub fn new(surf: &'a mut Surface, s: f32) -> Painter<'a> {
        Painter { surf, s, alpha: 1.0 }
    }

    /// 逻辑包围盒 → 物理像素范围（已夹到缓冲内）。
    fn span(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> (usize, usize, usize, usize) {
        let s = self.s;
        let cl = |v: f32, m: i32| (v.max(0.0) as i32).min(m) as usize;
        (cl((x0 * s).floor(), self.surf.w), cl((y0 * s).floor(), self.surf.h), cl((x1 * s).ceil(), self.surf.w), cl((y1 * s).ceil(), self.surf.h))
    }

    /// 通用着色：在逻辑包围盒内对每个物理像素中心调用 f(物理 x, 物理 y) → (颜色, 覆盖率)。
    /// hole：已知完全在形状内部的一块（不用逐像素算距离场）——跳过，或直接按 g 的颜色整块铺。大形状省掉绝大部分计算。
    fn shade(&mut self, bx0: f32, by0: f32, bx1: f32, by1: f32, hole: Hole, f: impl Fn(f32, f32) -> Option<(Rgba, f32)>) {
        let (x0, y0, x1, y1) = self.span(bx0, by0, bx1, by1);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let w = self.surf.w as usize;
        let ga = self.alpha;
        let (hr, fill, solid) = match hole {
            Hole::None => (None, None, None),
            Hole::Skip(r) => (r, None, None),
            Hole::Fill(r, g) => (r, Some(g), None),
            Hole::Solid(r, c) => (r, None, Some(c)),
        };
        let opaque = solid.map(|c| (c, c.a * ga >= 0.999, pack(c)));
        let px = self.surf.pixels();
        for y in y0..y1 {
            let row = &mut px[y * w..y * w + w];
            let fy = y as f32 + 0.5;
            let (h0, h1) = match hr {
                Some([hx0, hy0, hx1, hy1]) if y >= hy0 && y < hy1 => (hx0.clamp(x0, x1), hx1.clamp(x0, x1).max(hx0.clamp(x0, x1))),
                _ => (x1, x1),
            };
            for x in (x0..h0).chain(h1..x1) {
                if let Some((c, cov)) = f(x as f32 + 0.5, fy) {
                    blend(&mut row[x], c, c.a * cov * ga);
                }
            }
            if let Some((c, op, v)) = opaque {
                if op {
                    row[h0..h1].fill(v);
                } else {
                    for p in &mut row[h0..h1] {
                        blend(p, c, c.a * ga);
                    }
                }
            }
            if let Some(g) = fill {
                for (x, p) in row.iter_mut().enumerate().take(h1).skip(h0) {
                    let c = g(x as f32 + 0.5, fy);
                    blend(p, c, c.a * ga);
                }
            }
        }
    }

    /// 整块铺色（不抗锯齿，用于背景）。
    pub fn clear(&mut self, c: Rgba) {
        let v = pack(c);
        self.surf.pixels().fill(v);
    }

    /// 竖向线性渐变的矩形（top → bottom，不透明）。颜色差很小时 8 位会出台阶，加 4x4 有序抖动（和浏览器一样）。
    pub fn vgrad(&mut self, x: f32, y: f32, w: f32, h: f32, top: Rgba, bottom: Rgba) {
        const BAYER: [f32; 16] = [0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0];
        let s = self.s;
        let (py0, ph) = (y * s, (h * s).max(1.0));
        let (x0, y0, x1, y1) = self.span(x, y, x + w, y + h);
        let sw = self.surf.w as usize;
        let px = self.surf.pixels();
        for yy in y0..y1 {
            let c = top.lerp(bottom, ((yy as f32 + 0.5 - py0) / ph).clamp(0.0, 1.0));
            // 这一行只有 4 种值（按 x & 3）
            let pat: [u32; 4] = std::array::from_fn(|i| {
                let d = (BAYER[(yy & 3) * 4 + i] + 0.5) / 16.0;
                let q = |v: f32| ((v * 255.0 + d).floor().clamp(0.0, 255.0)) as u32;
                0xFF00_0000 | q(c.r) << 16 | q(c.g) << 8 | q(c.b)
            });
            for (xx, p) in px[yy * sw..yy * sw + sw].iter_mut().enumerate().take(x1).skip(x0) {
                *p = pat[xx & 3];
            }
        }
    }

    /// 填充圆角矩形。
    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        let s = self.s;
        let (cx, cy, hw, hh, rr) = ((x + w / 2.0) * s, (y + h / 2.0) * s, w * s / 2.0, h * s / 2.0, r * s);
        self.shade(x - 1.0, y - 1.0, x + w + 1.0, y + h + 1.0, Hole::Solid(inner(cx, cy, hw, hh, rr, 0.5), c), |px, py| {
            let cov = (0.5 - sd_rrect(px, py, cx, cy, hw, hh, rr)).clamp(0.0, 1.0);
            if cov <= 0.0 { None } else { Some((c, cov)) }
        });
    }

    /// 填充圆角矩形，颜色由 f(逻辑 x, 逻辑 y) 给出（渐变）。
    pub fn rrect_with(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, f: impl Fn(f32, f32) -> Rgba) {
        let s = self.s;
        let (cx, cy, hw, hh, rr) = ((x + w / 2.0) * s, (y + h / 2.0) * s, w * s / 2.0, h * s / 2.0, r * s);
        let g = |px: f32, py: f32| f(px / s, py / s);
        self.shade(x - 1.0, y - 1.0, x + w + 1.0, y + h + 1.0, Hole::Fill(inner(cx, cy, hw, hh, rr, 0.5), &g), |px, py| {
            let d = sd_rrect(px, py, cx, cy, hw, hh, rr);
            let cov = (0.5 - d).clamp(0.0, 1.0);
            if cov <= 0.0 { None } else { Some((g(px, py), cov)) }
        });
    }

    /// 圆角矩形的环：覆盖距边 d0..d1 的区域（逻辑像素，外正内负）。如 CSS border 1px = (-1, 0)，外圈 1.5px = (0, 1.5)。
    pub fn ring(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, d0: f32, d1: f32, c: Rgba) {
        let s = self.s;
        let (cx, cy, hw, hh, rr) = ((x + w / 2.0) * s, (y + h / 2.0) * s, w * s / 2.0, h * s / 2.0, r * s);
        let (a, b) = (d0 * s, d1 * s);
        let m = d1.max(0.0) + 1.0;
        let hole = inner(cx, cy, hw, hh, rr, (-a).max(0.0) + 1.0);
        self.shade(x - m, y - m, x + w + m, y + h + m, Hole::Skip(hole), |px, py| {
            let d = sd_rrect(px, py, cx, cy, hw, hh, rr);
            let cov = ((d - a + 0.5).clamp(0.0, 1.0)).min((b - d + 0.5).clamp(0.0, 1.0));
            if cov <= 0.0 { None } else { Some((c, cov)) }
        });
    }

    /// CSS box-shadow（外投影）：偏移 dx/dy、模糊半径 blur（σ = blur/2）、扩展 spread。只画在盒子外面（同 CSS）。
    pub fn shadow(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, dx: f32, dy: f32, blur: f32, spread: f32, c: Rgba) {
        let s = self.s;
        let sig = (blur / 2.0 * s).max(0.3);
        let (bx, by, bw, bh) = (x + dx - spread, y + dy - spread, w + 2.0 * spread, h + 2.0 * spread);
        if bw <= 0.0 || bh <= 0.0 {
            return;
        }
        let (scx, scy, shw, shh, sr) = ((bx + bw / 2.0) * s, (by + bh / 2.0) * s, bw * s / 2.0, bh * s / 2.0, (r + spread).max(0.0) * s);
        let (cx, cy, hw, hh, rr) = ((x + w / 2.0) * s, (y + h / 2.0) * s, w * s / 2.0, h * s / 2.0, r * s);
        let m = blur * 1.6 + 1.0;
        if sig >= sr * 0.8 {
            // 模糊远大于圆角：按“高斯模糊的矩形 = 横向 × 纵向”算，每行每列各算一次误差函数
            let (x0, y0, x1, y1) = self.span(bx - m, by - m, bx + bw + m, by + bh + m);
            let k = 1.0 / (sig * std::f32::consts::SQRT_2);
            let edge = |p: f32, lo: f32, hi: f32| 0.5 * (erfc((lo - p) * k) - erfc((hi - p) * k));
            let fx: Vec<f32> = (x0..x1).map(|x| edge(x as f32 + 0.5, scx - shw, scx + shw)).collect();
            let fy: Vec<f32> = (y0..y1).map(|y| edge(y as f32 + 0.5, scy - shh, scy + shh)).collect();
            let (bl, bt, br, bb) = (cx - hw - 1.0, cy - hh - 1.0, cx + hw + 1.0, cy + hh + 1.0);
            self.shade(bx - m, by - m, bx + bw + m, by + bh + m, Hole::Skip(inner(cx, cy, hw, hh, rr, 0.5)), |px, py| {
                let cov = fx[px as usize - x0] * fy[py as usize - y0];
                if cov < 0.002 {
                    return None;
                }
                let inside = if px > bl && px < br && py > bt && py < bb { (0.5 - sd_rrect(px, py, cx, cy, hw, hh, rr)).clamp(0.0, 1.0) } else { 0.0 };
                Some((c, cov * (1.0 - inside)))
            });
            return;
        }
        self.shade(bx - m, by - m, bx + bw + m, by + bh + m, Hole::Skip(inner(cx, cy, hw, hh, rr, 0.5)), |px, py| {
            let inside = (0.5 - sd_rrect(px, py, cx, cy, hw, hh, rr)).clamp(0.0, 1.0);
            if inside >= 1.0 {
                return None;
            }
            let d = sd_rrect(px, py, scx, scy, shw, shh, sr);
            let cov = 0.5 * erfc(d / (sig * std::f32::consts::SQRT_2));
            Some((c, cov * (1.0 - inside)))
        });
    }

    /// 柔光（不裁掉盒子内部），如 CSS box-shadow 0 0 8px 发光。
    pub fn glow(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, blur: f32, c: Rgba) {
        let s = self.s;
        let sig = (blur / 2.0 * s).max(0.3);
        let (cx, cy, hw, hh, rr) = ((x + w / 2.0) * s, (y + h / 2.0) * s, w * s / 2.0, h * s / 2.0, r * s);
        let m = blur * 1.6 + 1.0;
        self.shade(x - m, y - m, x + w + m, y + h + m, Hole::None, |px, py| {
            let d = sd_rrect(px, py, cx, cy, hw, hh, rr);
            Some((c, 0.5 * erfc(d / (sig * std::f32::consts::SQRT_2))))
        });
    }

    /// 虚线边框（画在盒子里面，宽 width），沿周长 dash 实 gap 空；周长按整数个周期均分，接缝处不断。
    pub fn dashed(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, width: f32, dash: f32, gap: f32, c: Rgba) {
        use std::f32::consts::FRAC_PI_2;
        let s = self.s;
        let (cx, cy, hw, hh) = ((x + w / 2.0) * s, (y + h / 2.0) * s, w * s / 2.0, h * s / 2.0);
        let r = (r * s).min(hw).min(hh);
        let lw = width * s;
        // 中线所在的圆角矩形
        let (mhw, mhh, mr) = (hw - lw / 2.0, hh - lw / 2.0, (r - lw / 2.0).max(0.0));
        let (sx, sy) = ((mhw - mr) * 2.0, (mhh - mr) * 2.0);
        let arc = mr * FRAC_PI_2;
        let per = 2.0 * (sx + sy) + 4.0 * arc;
        let period0 = (dash + gap) * s;
        let n = (per / period0).round().max(1.0);
        let period = per / n;
        let on = period * dash / (dash + gap);
        self.shade(x - 1.0, y - 1.0, x + w + 1.0, y + h + 1.0, Hole::Skip(inner(cx, cy, hw, hh, r, lw + 1.0)), |px, py| {
            let d = sd_rrect(px, py, cx, cy, hw, hh, r);
            let band = ((d + lw + 0.5).clamp(0.0, 1.0)).min((0.5 - d).clamp(0.0, 1.0));
            if band <= 0.0 {
                return None;
            }
            // 沿周长的位置（从上边左端起顺时针）
            let (qx, qy) = (px - cx, py - cy);
            let (ex, ey) = (mhw - mr, mhh - mr);
            let t = if qx.abs() <= ex {
                if qy < 0.0 { qx + ex } else { sx + arc + sy + arc + (ex - qx) }
            } else if qy.abs() <= ey {
                if qx > 0.0 { sx + arc + (qy + ey) } else { 2.0 * sx + sy + 3.0 * arc + (ey - qy) }
            } else {
                let (ccx, ccy) = (qx.signum() * ex, qy.signum() * ey);
                let a = (qy - ccy).atan2(qx - ccx); // -π..π
                match (qx > 0.0, qy > 0.0) {
                    (true, false) => sx + mr * (a + FRAC_PI_2),
                    (true, true) => sx + arc + sy + mr * a,
                    (false, true) => 2.0 * sx + 2.0 * arc + sy + mr * (a - FRAC_PI_2),
                    (false, false) => 2.0 * sx + 2.0 * sy + 3.0 * arc + mr * (a + std::f32::consts::PI),
                }
            };
            let u = t.rem_euclid(period);
            let cov = if u <= on { (u.min(on - u) + 0.5).clamp(0.0, 1.0) } else { (0.5 - (u - on).min(period - u)).clamp(0.0, 1.0) };
            Some((c, band * cov))
        });
    }

    /// 描一条路径（线段 + 圆弧，圆头），宽 width（逻辑像素）。整条路径一次算覆盖率，交接处不会叠深。
    pub fn stroke(&mut self, segs: &[Seg], width: f32, c: Rgba) {
        if segs.is_empty() {
            return;
        }
        let s = self.s;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for g in segs {
            let (a, b, cc, d) = match *g {
                Seg::Line(ax, ay, bx, by) => (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)),
                Seg::Arc(cx, cy, r, _, _) => (cx - r, cy - r, cx + r, cy + r),
            };
            x0 = x0.min(a);
            y0 = y0.min(b);
            x1 = x1.max(cc);
            y1 = y1.max(d);
        }
        let m = width + 1.0;
        let hw = width * s / 2.0;
        let segs_p: Vec<Seg> = segs
            .iter()
            .map(|g| match *g {
                Seg::Line(a, b, cc, d) => Seg::Line(a * s, b * s, cc * s, d * s),
                Seg::Arc(cx, cy, r, a0, a1) => Seg::Arc(cx * s, cy * s, r * s, a0, a1),
            })
            .collect();
        self.shade(x0 - m, y0 - m, x1 + m, y1 + m, Hole::None, |px, py| {
            let mut d = f32::MAX;
            for g in &segs_p {
                let e = match *g {
                    Seg::Line(a, b, cc, dd) => d_seg(px, py, a, b, cc, dd),
                    Seg::Arc(cx, cy, r, a0, a1) => d_arc(px, py, cx, cy, r, a0, a1),
                };
                d = d.min(e);
            }
            let cov = (hw - d + 0.5).clamp(0.0, 1.0);
            if cov <= 0.0 { None } else { Some((c, cov)) }
        });
    }

    /// 折线（圆头圆角）。
    pub fn polyline(&mut self, pts: &[(f32, f32)], width: f32, c: Rgba) {
        let segs: Vec<Seg> = pts.windows(2).map(|p| Seg::Line(p[0].0, p[0].1, p[1].0, p[1].1)).collect();
        self.stroke(&segs, width, c);
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        self.rrect(cx - r, cy - r, 2.0 * r, 2.0 * r, r, c);
    }

    /// GDI 文字：左上角在 (x, y)（逻辑像素，字符单元顶部）。
    pub fn text(&mut self, x: f32, y: f32, text: &str, font: &Font, c: Rgba) {
        if text.is_empty() {
            return;
        }
        let w: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let dc = self.surf.dc;
            let old = SelectObject(dc, font.h.into());
            SetTextColor(dc, c.colorref());
            let _ = TextOutW(dc, (x * self.s).round() as i32, (y * self.s).round() as i32, &w);
            SelectObject(dc, old);
        }
    }

    /// 文字竖直居中在 cy 上，返回宽度（逻辑像素）。
    pub fn text_mid(&mut self, x: f32, cy: f32, text: &str, font: &Font, c: Rgba) -> f32 {
        self.text(x, cy - font.cell / 2.0, text, font, c);
        self.measure(font, text)
    }

    pub fn measure(&self, font: &Font, text: &str) -> f32 {
        text_w(self.surf.dc, font, self.s, text)
    }
}

/// CSS cubic-bezier(x1, y1, x2, y2) 缓动：给进度 x（0..1）求 y。
pub fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    let bx = |t: f32| 3.0 * (1.0 - t) * (1.0 - t) * t * x1 + 3.0 * (1.0 - t) * t * t * x2 + t * t * t;
    let by = |t: f32| 3.0 * (1.0 - t) * (1.0 - t) * t * y1 + 3.0 * (1.0 - t) * t * t * y2 + t * t * t;
    // 二分：bx 单调
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if bx(mid) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    by((lo + hi) / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        let c = hex(0x34C759);
        assert_eq!(c.colorref().0, 0x59C734);
        assert_eq!(hexa(0xFFFFFF, 0.5).lerp(hex(0x000000), 0.5).colorref().0, 0x808080);
    }

    #[test]
    fn easing() {
        assert!((cubic_bezier(0.2, 0.8, 0.2, 1.0, 0.0)).abs() < 1e-3);
        assert!((cubic_bezier(0.2, 0.8, 0.2, 1.0, 1.0) - 1.0).abs() < 1e-3);
        // 这条曲线前快后慢：一半时间已走过大半
        assert!(cubic_bezier(0.2, 0.8, 0.2, 1.0, 0.5) > 0.8);
    }

    #[test]
    fn distances() {
        assert!(sd_rrect(0.0, 0.0, 0.0, 0.0, 10.0, 5.0, 2.0) < 0.0);
        assert!((sd_rrect(12.0, 0.0, 0.0, 0.0, 10.0, 5.0, 2.0) - 2.0).abs() < 1e-4);
        assert!((d_seg(5.0, 3.0, 0.0, 0.0, 10.0, 0.0) - 3.0).abs() < 1e-4);
        // 右半圆弧（-90° → 90°）
        let h = std::f32::consts::FRAC_PI_2;
        assert!((d_arc(12.0, 0.0, 0.0, 0.0, 10.0, -h, h) - 2.0).abs() < 1e-4);
        assert!(d_arc(-10.0, 0.0, 0.0, 0.0, 10.0, -h, h) > 10.0);
    }
}
