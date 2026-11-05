//! 剪贴板同步提示（“灵动岛”）：光标所在屏幕底部、任务栏上方居中弹出的黑色胶囊，两台电脑同时弹。
//!
//!   [图标]  有线电脑 复制了文字            [已同步]
//!           会议改到周四下午三点…
//!
//! 跑在 desktop 线程（有消息循环）。窗口：WS_EX_LAYERED | TOPMOST | TOOLWINDOW | NOACTIVATE | TRANSPARENT ——
//! 不抢焦点、点击穿透。画法不引入 Direct2D/GDI+（省内存）：胶囊、图标用距离场自己画抗锯齿；文字用 GDI 白字黑底
//! 画成灰度蒙版（ANTIALIASED_QUALITY，不用 ClearType），再按蒙版上色；最后以预乘 alpha 交给 UpdateLayeredWindow。
//! 出现 240ms（淡入 + 上移 12）→ 停留 2 秒（没同步 2.5 秒）→ 收起 180ms（淡出 + 下沉 8）。
//! 连续复制：同一颗胶囊原地换内容，停留重新计时；收起途中来了新的就原路回来。
//! 全屏游戏 / 演示（SHQueryUserNotificationState）时不弹。收起后释放位图，只留一个隐藏窗口。

use std::cell::RefCell;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS,
    DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, GetMonitorInfoW,
    GetTextExtentPoint32W, HBITMAP, HDC, HFONT, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{
    QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetCursorPos, HWND_TOPMOST, KillTimer, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetTimer, SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow,
    WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};
use windows::core::w;

// ───────────────────────── 提示内容 ─────────────────────────

/// 复制的是什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
    /// 密码等敏感内容（不显示内容）
    Secret,
}

impl Kind {
    /// 协议里的编号（Msg::ClipNote.kind）。
    pub fn code(self) -> u8 {
        match self {
            Kind::Text => 0,
            Kind::Image => 1,
            Kind::Secret => 2,
        }
    }

    pub fn from_code(c: u8) -> Kind {
        match c {
            1 => Kind::Image,
            2 => Kind::Secret,
            _ => Kind::Text,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Kind::Text => "复制了文字",
            Kind::Image => "复制了图片",
            Kind::Secret => "复制了密码",
        }
    }
}

/// 一条提示：谁复制的、复制了什么、第二行（预览 / 大小 / 没同步的原因）、同步了没有。
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub who: String,
    pub kind: Kind,
    pub detail: String,
    pub ok: bool,
}

/// 第二行最多这么多个字符（再长的显示时还会按宽度加省略号）。
pub const DETAIL_MAX: usize = 24;
/// 名字最多显示这么多个字符。
const NAME_MAX: usize = 16;

/// 文字预览：空白（含换行）合成一个空格，取开头 DETAIL_MAX 个字符，多了加省略号。
pub fn preview(text: &str) -> String {
    let mut out = String::new();
    let mut n = 0;
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
            continue;
        }
        if n >= DETAIL_MAX {
            out.push('…');
            return out;
        }
        if space {
            out.push(' ');
            n += 1;
            space = false;
            if n >= DETAIL_MAX {
                out.push('…');
                return out;
            }
        }
        out.push(c);
        n += 1;
    }
    out
}

/// 大小：不到 1 MB 用 KB（向上取整），否则 MB 一位小数。
pub fn size_text(bytes: usize) -> String {
    const MB: usize = 1024 * 1024;
    if bytes < MB {
        format!("{} KB", bytes.div_ceil(1024).max(1))
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

/// 收到的字符串：去掉控制字符、截短（对方发来的不可信）。
pub fn clean(s: &str, max: usize) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).take(max).collect();
    if s.chars().filter(|c| !c.is_control()).count() > max {
        out.push('…');
    }
    out
}

// ───────────────────────── 窗口与动画 ─────────────────────────

const SHOW_MS: f32 = 240.0;
const HIDE_MS: f32 = 180.0;
const HOLD_OK: Duration = Duration::from_millis(2000);
const HOLD_FAIL: Duration = Duration::from_millis(2500);
/// 动画帧间隔（毫秒）。
const FRAME_MS: u32 = 15;
/// 出现时从下方多少（逻辑像素）升上来；收起时下沉多少。
const RISE: f32 = 12.0;
const SINK: f32 = 8.0;
/// 离工作区底边（任务栏上沿）多远。
const MARGIN_BOTTOM: f32 = 12.0;
const TIMER_ANIM: usize = 1;

#[derive(Clone, Copy, Debug)]
enum Phase {
    Hidden,
    /// 出现中：从透明度 from 开始
    In { start: Instant, from: f32 },
    Hold { until: Instant },
    /// 收起中：从透明度 from 开始
    Out { start: Instant, from: f32 },
}

/// 画好的一帧内容（预乘 alpha 的 32 位位图，选进内存 DC）。
struct Canvas {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct Island {
    hwnd: HWND,
    canvas: Option<Canvas>,
    /// 停稳时左上角（物理像素）
    x: i32,
    y: i32,
    scale: f32,
    phase: Phase,
    /// 当前透明度（0~1）
    alpha: f32,
    hold: Duration,
    /// 正在显示的提示
    note: Option<Note>,
}

thread_local! {
    static ISLAND: RefCell<Option<Island>> = const { RefCell::new(None) };
}

/// 弹出（或原地刷新）提示。只能在 desktop 线程调用。
pub fn show(note: &Note) {
    if busy_fullscreen() {
        return;
    }
    ISLAND.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else { return };
        if g.is_none() {
            match create_window() {
                Some(hwnd) => {
                    *g = Some(Island {
                        hwnd,
                        canvas: None,
                        x: 0,
                        y: 0,
                        scale: 1.0,
                        phase: Phase::Hidden,
                        alpha: 0.0,
                        hold: HOLD_OK,
                        note: None,
                    })
                }
                None => return,
            }
        }
        let Some(is) = g.as_mut() else { return };
        is.present(note);
    });
}

/// 本机刚弹的“已同步”其实没被对方接收：还显示着就原地改成“未同步”（detail 换成原因）。
/// 没在显示、正在收起、已经是没同步、或显示的不是本机（name）复制的：什么都不做。
pub fn fail_own(name: &str, detail: &str) {
    ISLAND.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else { return };
        let Some(is) = g.as_mut() else { return };
        if !matches!(is.phase, Phase::In { .. } | Phase::Hold { .. }) {
            return;
        }
        let Some(n) = is.note.as_ref() else { return };
        if !n.ok || n.who != name {
            return;
        }
        let n = Note { ok: false, detail: detail.to_string(), ..n.clone() };
        is.present(&n);
    });
}

/// 全屏游戏、演示时不弹。
fn busy_fullscreen() -> bool {
    match unsafe { SHQueryUserNotificationState() } {
        Ok(s) => s == QUNS_BUSY || s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE,
        Err(_) => false,
    }
}

fn create_window() -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let class = w!("kmlink-island");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
        // 已注册过（ERROR_CLASS_ALREADY_EXISTS）也没关系
        let _ = RegisterClassW(&wc);
        let ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
        match CreateWindowExW(ex, class, w!(""), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst.into()), None) {
            Ok(h) => Some(h),
            Err(e) => {
                crate::log!("创建剪贴板提示窗口失败：{e}");
                None
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_TIMER && wp.0 == TIMER_ANIM {
        ISLAND.with(|cell| {
            if let Ok(mut g) = cell.try_borrow_mut()
                && let Some(is) = g.as_mut()
            {
                is.tick();
            }
        });
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn ease_out(p: f32) -> f32 {
    1.0 - (1.0 - p).powi(3)
}

fn ease_in(p: f32) -> f32 {
    p * p * p
}

impl Island {
    fn present(&mut self, note: &Note) {
        let Some((work, scale)) = cursor_monitor() else { return };
        let Some(c) = render(note, scale) else { return };
        let mid_x = (work.left + work.right) / 2;
        self.x = mid_x - c.w / 2;
        self.y = work.bottom - (MARGIN_BOTTOM * scale).round() as i32 - c.h;
        self.scale = scale;
        self.canvas = Some(c);
        self.hold = if note.ok { HOLD_OK } else { HOLD_FAIL };
        self.note = Some(note.clone());
        let now = Instant::now();
        self.phase = match self.phase {
            // 正在出现：接着出现
            p @ Phase::In { .. } => p,
            // 停着：原地换内容，重新计时
            Phase::Hold { .. } => Phase::Hold { until: now + self.hold },
            // 没显示 / 正在收起：从当前透明度原路回来
            Phase::Hidden | Phase::Out { .. } => Phase::In { start: now, from: self.alpha },
        };
        // 先把新内容推上去（隐藏时也能更新），再显示：不会闪出上一颗的旧画面
        self.tick();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
        }
    }

    /// 推进动画、画一帧，并按需要定下一次定时器。
    fn tick(&mut self) {
        let now = Instant::now();
        let mut offset = 0.0;
        let next_ms: Option<u32> = loop {
            match self.phase {
                Phase::Hidden => break None,
                Phase::In { start, from } => {
                    let p = (now.duration_since(start).as_secs_f32() * 1000.0 / SHOW_MS).min(1.0);
                    self.alpha = from + (1.0 - from) * ease_out(p);
                    offset = RISE * (1.0 - self.alpha);
                    if p >= 1.0 {
                        self.phase = Phase::Hold { until: now + self.hold };
                        continue;
                    }
                    break Some(FRAME_MS);
                }
                Phase::Hold { until } => {
                    self.alpha = 1.0;
                    if now >= until {
                        self.phase = Phase::Out { start: now, from: 1.0 };
                        continue;
                    }
                    break Some((until - now).as_millis().clamp(1, 60_000) as u32);
                }
                Phase::Out { start, from } => {
                    let p = (now.duration_since(start).as_secs_f32() * 1000.0 / HIDE_MS).min(1.0);
                    self.alpha = from * (1.0 - ease_in(p));
                    offset = SINK * (1.0 - self.alpha);
                    if p >= 1.0 {
                        self.hide();
                        break None;
                    }
                    break Some(FRAME_MS);
                }
            }
        };
        if let Some(ms) = next_ms {
            self.frame((offset * self.scale).round() as i32);
            unsafe {
                // 同一个 id 再 SetTimer 会改周期
                SetTimer(Some(self.hwnd), TIMER_ANIM, ms, None);
            }
        }
    }

    fn frame(&self, offset: i32) {
        let Some(c) = &self.canvas else { return };
        let pt = POINT { x: self.x, y: self.y + offset };
        let size = SIZE { cx: c.w, cy: c.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (self.alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pt),
                Some(&size),
                Some(c.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    fn hide(&mut self) {
        self.phase = Phase::Hidden;
        self.alpha = 0.0;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_ANIM);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        // 位图用完即还
        self.canvas = None;
        self.note = None;
    }
}

/// 光标所在显示器的工作区（物理像素）和缩放比例。
fn cursor_monitor() -> Option<(RECT, f32)> {
    unsafe {
        let mut p = POINT::default();
        GetCursorPos(&mut p).ok()?;
        let mon = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return None;
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        let scale = if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx > 0 {
            dx as f32 / 96.0
        } else {
            1.0
        };
        Some((mi.rcWork, scale.clamp(0.5, 4.0)))
    }
}

// ───────────────────────── 绘制 ─────────────────────────

/// 颜色（0~1）。
type Rgb = [f32; 3];

fn rgb(hex: u32) -> Rgb {
    [((hex >> 16) & 0xFF) as f32 / 255.0, ((hex >> 8) & 0xFF) as f32 / 255.0, (hex & 0xFF) as f32 / 255.0]
}

const BG: u32 = 0x161618;
const BG_ALPHA: f32 = 0.96;
const EDGE_ALPHA: f32 = 0.10;
const NAME: u32 = 0xF5F5F7;
const VERB: u32 = 0xC9C9CF;
const DETAIL: u32 = 0x9D9DA4;
const CYAN_BG: u32 = 0x22D3EE;
const CYAN: u32 = 0x67E8F9;
const AMBER: u32 = 0xFBBF24;
const TINT_ALPHA: f32 = 0.16;

/// 预乘 alpha 的浮点画布。
struct Paint {
    w: usize,
    h: usize,
    px: Vec<[f32; 4]>,
}

fn sat(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// 圆角矩形的有向距离（中心 c，半宽高 hw/hh，圆角 r）。
fn sd_rrect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - hw + r;
    let qy = (y - cy).abs() - hh + r;
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// 到线段 ab 的距离。
fn d_seg(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (px, py) = (x - a.0, y - a.1);
    let (bx, by) = (b.0 - a.0, b.1 - a.1);
    let len2 = bx * bx + by * by;
    let t = if len2 > 0.0 { ((px * bx + py * by) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (dx, dy) = (px - bx * t, py - by * t);
    (dx * dx + dy * dy).sqrt()
}

impl Paint {
    fn new(w: usize, h: usize) -> Paint {
        Paint { w, h, px: vec![[0.0; 4]; w * h] }
    }

    /// 在包围盒 (l, t, r, b) 里按覆盖率 cov(x, y)（像素中心坐标）叠一层颜色。
    fn layer(&mut self, bbox: (f32, f32, f32, f32), color: Rgb, alpha: f32, cov: impl Fn(f32, f32) -> f32) {
        let x0 = bbox.0.floor().max(0.0) as usize;
        let y0 = bbox.1.floor().max(0.0) as usize;
        let x1 = (bbox.2.ceil().max(0.0) as usize).min(self.w);
        let y1 = (bbox.3.ceil().max(0.0) as usize).min(self.h);
        for y in y0..y1 {
            for x in x0..x1 {
                let c = cov(x as f32 + 0.5, y as f32 + 0.5);
                if c <= 0.0 {
                    continue;
                }
                let a = alpha * c.min(1.0);
                let d = &mut self.px[y * self.w + x];
                let k = 1.0 - a;
                d[0] = color[0] * a + d[0] * k;
                d[1] = color[1] * a + d[1] * k;
                d[2] = color[2] * a + d[2] * k;
                d[3] = a + d[3] * k;
            }
        }
    }
}

/// 一段文字：放在 rect 里（单行、垂直居中、放不下加省略号），用 font、color 画。
struct TextRun {
    rect: RECT,
    text: Vec<u16>,
    font: HFONT,
    color: Rgb,
    extra: u32,
}

struct Fonts {
    name: HFONT,
    verb: HFONT,
    detail: HFONT,
    tag: HFONT,
}

impl Drop for Fonts {
    fn drop(&mut self) {
        unsafe {
            for f in [self.name, self.verb, self.detail, self.tag] {
                let _ = DeleteObject(f.into());
            }
        }
    }
}

fn font(px: f32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -(px.round() as i32).max(1),
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
            ANTIALIASED_QUALITY,
            0,
            w!("Microsoft YaHei UI"),
        )
    }
}

/// 32 位自顶向下的 DIB，选进一个新的内存 DC。
fn dib(w: i32, h: i32) -> Option<(Canvas, *mut u8)> {
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
        std::ptr::write_bytes(bits as *mut u8, 0, (w as usize) * (h as usize) * 4);
        Some((Canvas { dc, bmp, old, w, h }, bits as *mut u8))
    }
}

fn text_width(dc: HDC, f: HFONT, s: &[u16]) -> i32 {
    unsafe {
        let old = SelectObject(dc, f.into());
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, s, &mut sz);
        SelectObject(dc, old);
        sz.cx
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// 画一颗胶囊。scale = 显示器缩放（1.0 = 96 DPI）。
fn render(note: &Note, scale: f32) -> Option<Canvas> {
    let s = scale;
    let px = |v: f32| (v * s).round() as i32;
    let fonts = Fonts { name: font(14.0 * s, 700), verb: font(14.0 * s, 400), detail: font(12.5 * s, 400), tag: font(12.0 * s, 700) };

    let name: String = note.who.chars().take(NAME_MAX).collect();
    let name = if name.trim().is_empty() { "对方".to_string() } else { name };
    let name_w16 = wide(&name);
    let verb_w16 = wide(note.kind.verb());
    let detail_w16 = wide(&note.detail);
    let tag_w16 = wide(if note.ok { "已同步" } else { "未同步" });

    // 先量字宽（用一个临时 1x1 画布的 DC）
    let (probe, _) = dib(1, 1)?;
    let name_w = text_width(probe.dc, fonts.name, &name_w16);
    let verb_w = text_width(probe.dc, fonts.verb, &verb_w16);
    let detail_w = if detail_w16.is_empty() { 0 } else { text_width(probe.dc, fonts.detail, &detail_w16) };
    let tag_tw = text_width(probe.dc, fonts.tag, &tag_w16);
    drop(probe);

    let h = px(56.0);
    let pad_l = px(10.0);
    let icon = px(36.0);
    let gap = px(12.0);
    let pad_r = px(12.0);
    let space = px(5.0);
    let tag_h = px(24.0);
    let tag_w = tag_tw + 2 * px(10.0);
    let text_x = pad_l + icon + gap;
    let need = (name_w + space + verb_w).max(detail_w);
    let w = (text_x + need + gap + tag_w + pad_r).clamp(px(260.0), px(460.0));
    let text_w = (w - text_x - gap - tag_w - pad_r).max(1);

    // 两行（没有第二行就一行居中）
    let lh1 = px(20.0);
    let lh2 = px(18.0);
    let y1 = if detail_w16.is_empty() { (h - lh1) / 2 } else { (h - lh1 - lh2) / 2 };
    let y2 = y1 + lh1;
    let name_box = name_w.min((text_w - space - verb_w).max(px(40.0)));
    let accent = rgb(if note.ok { CYAN } else { AMBER });
    let tag_l = w - pad_r - tag_w;
    let tag_t = (h - tag_h) / 2;
    let mut runs = vec![
        TextRun {
            rect: RECT { left: text_x, top: y1, right: text_x + name_box, bottom: y1 + lh1 },
            text: name_w16,
            font: fonts.name,
            color: rgb(NAME),
            extra: DT_END_ELLIPSIS.0,
        },
        TextRun {
            rect: RECT { left: text_x + name_box + space, top: y1, right: text_x + text_w, bottom: y1 + lh1 },
            text: verb_w16,
            font: fonts.verb,
            color: rgb(VERB),
            extra: DT_END_ELLIPSIS.0,
        },
        TextRun {
            rect: RECT { left: tag_l, top: tag_t, right: tag_l + tag_w, bottom: tag_t + tag_h },
            text: tag_w16,
            font: fonts.tag,
            color: accent,
            // DT_CENTER
            extra: 0x1,
        },
    ];
    if !detail_w16.is_empty() {
        runs.push(TextRun {
            rect: RECT { left: text_x, top: y2, right: text_x + text_w, bottom: y2 + lh2 },
            text: detail_w16,
            font: fonts.detail,
            color: rgb(DETAIL),
            extra: DT_END_ELLIPSIS.0,
        });
    }

    // 文字蒙版：白字黑底（灰度抗锯齿）
    let (mask, mbits) = dib(w, h)?;
    unsafe {
        SetBkMode(mask.dc, TRANSPARENT);
        SetTextColor(mask.dc, COLORREF(0x00FF_FFFF));
        for r in runs.iter_mut() {
            let old = SelectObject(mask.dc, r.font.into());
            let fmt = windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT(
                DT_SINGLELINE.0 | DT_VCENTER.0 | DT_NOPREFIX.0 | r.extra,
            );
            let mut rc = r.rect;
            DrawTextW(mask.dc, &mut r.text, &mut rc, fmt);
            SelectObject(mask.dc, old);
        }
        // 直接读 DIB 位之前，让 GDI 把排着的绘制做完
        let _ = windows::Win32::Graphics::Gdi::GdiFlush();
    }
    let (wu, hu) = (w as usize, h as usize);
    let coverage = |x: usize, y: usize| -> f32 {
        // B、G、R 取最大（灰度抗锯齿时三者相同）
        let p = unsafe { std::slice::from_raw_parts(mbits.add((y * wu + x) * 4), 3) };
        p[0].max(p[1]).max(p[2]) as f32 / 255.0
    };

    let mut pt = Paint::new(wu, hu);
    let (wf, hf) = (w as f32, h as f32);
    let r = hf / 2.0;
    let (cx, cy) = (wf / 2.0, hf / 2.0);
    // 胶囊底
    pt.layer((0.0, 0.0, wf, hf), rgb(BG), BG_ALPHA, |x, y| sat(0.5 - sd_rrect(x, y, cx, cy, cx, cy, r)));
    // 1px 亮边（深色桌面上勾出轮廓）
    let bw = s.max(1.0);
    pt.layer((0.0, 0.0, wf, hf), [1.0, 1.0, 1.0], EDGE_ALPHA, |x, y| {
        let d = sd_rrect(x, y, cx, cy, cx, cy, r);
        sat(0.5 - d) - sat(0.5 - (d + bw))
    });
    // 图标底圆
    let (ix, iy, ir) = (pad_l as f32 + icon as f32 / 2.0, cy, icon as f32 / 2.0);
    pt.layer((ix - ir - 1.0, iy - ir - 1.0, ix + ir + 1.0, iy + ir + 1.0), rgb(if note.ok { CYAN_BG } else { AMBER }), TINT_ALPHA, |x, y| {
        let d = ((x - ix).powi(2) + (y - iy).powi(2)).sqrt() - ir;
        sat(0.5 - d)
    });
    glyph(&mut pt, note.kind, (ix, iy), s, accent);
    // 状态标签底
    let (tl, tt, tw, th) = (tag_l as f32, tag_t as f32, tag_w as f32, tag_h as f32);
    let (tcx, tcy) = (tl + tw / 2.0, tt + th / 2.0);
    pt.layer((tl - 1.0, tt - 1.0, tl + tw + 1.0, tt + th + 1.0), rgb(if note.ok { CYAN_BG } else { AMBER }), TINT_ALPHA, |x, y| {
        sat(0.5 - sd_rrect(x, y, tcx, tcy, tw / 2.0, th / 2.0, th / 2.0))
    });
    // 文字
    for run in &runs {
        let rc = run.rect;
        let (x0, y0) = (rc.left.max(0) as usize, rc.top.max(0) as usize);
        let (x1, y1) = ((rc.right.max(0) as usize).min(wu), (rc.bottom.max(0) as usize).min(hu));
        for y in y0..y1 {
            for x in x0..x1 {
                let c = coverage(x, y);
                if c > 0.0 {
                    let d = &mut pt.px[y * wu + x];
                    let k = 1.0 - c;
                    d[0] = run.color[0] * c + d[0] * k;
                    d[1] = run.color[1] * c + d[1] * k;
                    d[2] = run.color[2] * c + d[2] * k;
                    d[3] = c + d[3] * k;
                }
            }
        }
    }
    drop(mask);
    drop(fonts);

    // 转成预乘 alpha 的 BGRA
    let (out, obits) = dib(w, h)?;
    let dst = unsafe { std::slice::from_raw_parts_mut(obits, wu * hu * 4) };
    for (i, p) in pt.px.iter().enumerate() {
        let a = sat(p[3]);
        let q = |v: f32| (sat(v).min(a) * 255.0).round() as u8;
        dst[i * 4] = q(p[2]);
        dst[i * 4 + 1] = q(p[1]);
        dst[i * 4 + 2] = q(p[0]);
        dst[i * 4 + 3] = (a * 255.0).round() as u8;
    }
    Some(out)
}

/// 图标（以 c 为中心，逻辑尺寸约 16）：文字 = 三条横线；图片 = 相框 + 太阳 + 山；密码 = 锁。
fn glyph(pt: &mut Paint, kind: Kind, c: (f32, f32), s: f32, color: Rgb) {
    let sw = 1.6 * s; // 线宽
    let p = |x: f32, y: f32| (c.0 + x * s, c.1 + y * s);
    let bbox = (c.0 - 12.0 * s, c.1 - 12.0 * s, c.0 + 12.0 * s, c.1 + 12.0 * s);
    let line = |pt: &mut Paint, a: (f32, f32), b: (f32, f32)| {
        pt.layer(bbox, color, 1.0, |x, y| sat(0.5 - (d_seg(x, y, a, b) - sw / 2.0)));
    };
    match kind {
        Kind::Text => {
            line(pt, p(-6.0, -4.5), p(6.0, -4.5));
            line(pt, p(-6.0, 0.0), p(6.0, 0.0));
            line(pt, p(-6.0, 4.5), p(2.0, 4.5));
        }
        Kind::Image => {
            let (fx, fy) = p(0.0, 0.0);
            let (hw, hh, r) = (7.0 * s, 5.5 * s, 2.0 * s);
            pt.layer(bbox, color, 1.0, |x, y| sat(0.5 - ((sd_rrect(x, y, fx, fy, hw, hh, r)).abs() - sw / 2.0)));
            let (sx, sy) = p(-2.8, -1.8);
            pt.layer(bbox, color, 1.0, |x, y| sat(0.5 - (((x - sx).powi(2) + (y - sy).powi(2)).sqrt() - 1.3 * s)));
            line(pt, p(-6.0, 4.0), p(-1.5, 0.5));
            line(pt, p(-1.5, 0.5), p(1.5, 2.8));
            line(pt, p(1.5, 2.8), p(6.0, -1.0));
        }
        Kind::Secret => {
            let (bx, by) = p(0.0, 2.5);
            let (hw, hh, r) = (5.5 * s, 4.0 * s, 1.5 * s);
            pt.layer(bbox, color, 1.0, |x, y| sat(0.5 - sd_rrect(x, y, bx, by, hw, hh, r)));
            // 锁梁：上半圆环 + 两条竖线
            let (ax, ay) = p(0.0, -3.0);
            let rr = 3.5 * s;
            pt.layer(bbox, color, 1.0, |x, y| {
                if y > ay {
                    return 0.0;
                }
                sat(0.5 - ((((x - ax).powi(2) + (y - ay).powi(2)).sqrt() - rr).abs() - sw / 2.0))
            });
            line(pt, p(-3.5, -3.0), p(-3.5, -1.0));
            line(pt, p(3.5, -3.0), p(3.5, -1.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_collapses_and_truncates() {
        assert_eq!(preview("  会议改到\r\n周四\t下午  "), "会议改到 周四 下午");
        assert_eq!(preview(""), "");
        let long = "一二三四五六七八九十一二三四五六七八九十一二三四五六七八九十";
        let p = preview(long);
        assert_eq!(p.chars().count(), DETAIL_MAX + 1);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn sizes() {
        assert_eq!(size_text(10), "1 KB");
        assert_eq!(size_text(1536), "2 KB");
        assert_eq!(size_text(1024 * 1024 + 200 * 1024), "1.2 MB");
    }

    #[test]
    fn clean_strips_control_and_caps() {
        assert_eq!(clean("a\nb\u{7}c", 10), "abc");
        assert_eq!(clean("一二三四五", 3), "一二三…");
    }

    #[test]
    fn kind_codes_roundtrip() {
        for k in [Kind::Text, Kind::Image, Kind::Secret] {
            assert_eq!(Kind::from_code(k.code()), k);
        }
        assert_eq!(Kind::from_code(99), Kind::Text);
    }

    #[test]
    fn renders_without_panicking() {
        for (scale, ok) in [(1.0, true), (1.5, false), (2.0, true)] {
            let n = Note { who: "WiFi电脑".into(), kind: Kind::Image, detail: "1.2 MB".into(), ok };
            let c = render(&n, scale).expect("render");
            assert!(c.w >= (260.0 * scale) as i32 - 1 && c.w <= (460.0 * scale) as i32 + 1);
            assert_eq!(c.h, (56.0 * scale).round() as i32);
        }
    }
}
