//! 剪贴板同步提示（“灵动岛”）：光标所在屏幕底部、任务栏上方居中弹出一颗液态玻璃小胶囊，两台电脑同时弹。
//!
//!   ( [⧉] 有线电脑 已复制 )
//!
//! 只说哪台电脑复制了。没同步成功（太大 / 密码类 / 对方未接收）不加字：图标由绿变琥珀，停稳后左右轻抖一下。
//!
//! 键盘跟随的提示（同一颗胶囊，设计稿 设计\灵动岛\键盘提示\ 变体 B）：
//!   ( [⌨] 键盘 工位电脑 ) 键盘刚换到这台（动了这台的鼠标）就马上在这台弹，不等打字，后面是这台的名字（cfg.name，太长截短名字）
//!                        两台同时只留一颗：一台弹了，另一台正显示的“键盘”马上收起（Msg::KbdTip；本机鼠标跨过去时本机先收）。
//!   ( [⌨] 键盘 已收回 )  键盘被意外收回（断线、管理员窗口、锁屏）：琥珀图标，两台都弹；加强版警示——
//!                        停留 KBD_HOLD_FAIL，抖得更明显（STRONG_AMP），隔一会儿再抖一次。
//!
//! 检查更新的提示（设计稿 设计\检查更新\island.html）：
//!   ( [↑] 新版 0.2.0 [更新] ) 自动检查发现新版：只在用户正在用的那台弹（另一台收到 Msg::UpdTip 就不再弹同一版本），
//!                              带一个“更新”按钮（对方不支持一键更新时不带），停 UPD_HOLD，光标在胶囊上时不收。
//!   ( [◔] 更新中 45% ) / ( [↻] 正在重启 ) 更新过程中一直停着，原地刷新；( [✓] 已更新 0.2.0 ) 重启后弹一次；
//!   ( [↑] 更新失败 网络不通 ) 琥珀 + 轻抖。
//!   胶囊照旧点击穿透；按钮上另叠一个几乎全透明（alpha 1）的小窗口接鼠标（不抢焦点、手形光标、悬停变浅、点了调 update_both）。
//!
//! 跑在 desktop 线程（有消息循环）。窗口：WS_EX_LAYERED | TOPMOST | TOOLWINDOW | NOACTIVATE | TRANSPARENT ——
//! 不抢焦点、点击穿透。液态玻璃全在 CPU 上算（不引入 Direct2D/GDI+，省内存），参数见设计稿 设计\灵动岛\液态玻璃\glass.py：
//!   弹出前（窗口还隐藏着）截下胶囊背后那块屏幕 → 三次盒式模糊（≈高斯）+ 提饱和度（只算一次）→
//!   每帧：圆角矩形距离场；边缘一圈按圆弧截面向内取样（折射，红蓝略错开 = 色散）；按背景亮度自动选浅/深玻璃并染色
//!   （染色厚度按文字对比度自动加，每次弹出只算一次）；顶部内高光、底部内阴影、左上来光的高光描边；外投影（误差函数近似高斯）。
//!   图标、文字（GDI 灰度蒙版 + 晕影）弹出时画一次，每帧只按胶囊形状裁切、调透明度。
//! 动画：小圆点（Ø20）长成胶囊并带弹簧过冲（像一滴水）→ 停留 1.8 秒 → 先收字、再收成小圆点淡出。
//! 连续复制：原地换名字，宽度用弹簧变过去，停留重新计时。全屏游戏 / 演示（SHQueryUserNotificationState）时不弹。
//! 收起后释放全部缓冲，只留一个隐藏窗口。

use crate::ui::{Font, draw_text_px, text_px};
use std::cell::RefCell;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, BitBlt, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, ReleaseDC, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetCursorPos, HWND_TOPMOST, IDC_HAND, KillTimer, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE,
    RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetCursor,
    SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};
/// WM_MOUSELEAVE（windows crate 里没有导出常量的版本也能用）
const WM_MOUSELEAVE: u32 = 0x02A3;
use windows::core::w;

// ───────────────────────── 提示内容 ─────────────────────────

/// 复制的是什么（提示上不再区分，只用于协议和日志）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
    /// 密码等敏感内容
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
}

/// 一条提示：谁复制的、同步了没有。kind / detail 留给协议和日志，提示上不显示。
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub who: String,
    pub kind: Kind,
    pub detail: String,
    pub ok: bool,
}

/// 胶囊左边的图标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Icon {
    /// 两张叠放的圆角方块（剪贴板）
    Copy,
    /// 键盘（键盘跟随）
    Keyboard,
    /// 向上箭头（有新版 / 更新失败）
    Up,
    /// 进度环（更新中，0..=100）
    Ring(u8),
    /// 转圈箭头（正在重启）
    Restart,
    /// 对勾（已更新）
    Check,
}

/// 实际显示的内容：粗体主词 + 常规后半句（可空）+ 图标；strong = 加强版警示（停更久、抖更明显、抖两次）。
/// cut_verb：放不下时截短后半句（键盘提示的电脑名），否则截短主词（剪贴板的电脑名）。
#[derive(Clone, Debug, PartialEq)]
struct Tip {
    word: String,
    verb: String,
    cut_verb: bool,
    icon: Icon,
    ok: bool,
    strong: bool,
    /// 右边的按钮（“更新”），None = 没有
    button: Option<&'static str>,
    /// 停留时间（None = 按 ok / strong 定）
    hold: Option<Duration>,
}

impl Tip {
    fn clip(n: &Note) -> Tip {
        Tip { word: n.who.clone(), verb: VERB.into(), cut_verb: false, icon: Icon::Copy, ok: n.ok, strong: false, button: None, hold: None }
    }

    /// ok：“键盘 <本机名>”；否则“键盘 已收回”。
    fn keyboard(ok: bool, here: &str) -> Tip {
        let verb = if ok { here.trim().to_string() } else { "已收回".to_string() };
        Tip { word: "键盘".into(), verb, cut_verb: true, icon: Icon::Keyboard, ok, strong: !ok, button: None, hold: None }
    }

    fn upd(word: &str, verb: String, icon: Icon, ok: bool, button: Option<&'static str>, hold: Duration) -> Tip {
        Tip { word: word.into(), verb, cut_verb: true, icon, ok, strong: false, button, hold: Some(hold) }
    }

    /// 最小宽度：剪贴板沿用 MIN_W；键盘提示按内容来（“键盘”只有 80）。
    fn min_w(&self) -> f32 {
        if self.icon == Icon::Copy { MIN_W } else { H }
    }

    fn hold(&self) -> Duration {
        if let Some(h) = self.hold {
            h
        } else if self.strong {
            KBD_HOLD_FAIL
        } else if self.ok {
            HOLD
        } else {
            HOLD_FAIL
        }
    }
}

/// detail 最多这么多个字符（协议里截短用）。
pub const DETAIL_MAX: usize = 24;

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

// ───────────────────────── 参数（100% 缩放的逻辑像素） ─────────────────────────

const VERB: &str = "已复制";
const H: f32 = 36.0;
const PAD_L: f32 = 7.0;
const ICON: f32 = 22.0;
const GAP: f32 = 8.0;
const PAD_R: f32 = 15.0;
const WORD_GAP: f32 = 5.0;
const FONT_PX: f32 = 14.0;
const MIN_W: f32 = 120.0;
/// 胶囊里的按钮：高、左右内边距、离文字的间距、离胶囊右边的距离
const BTN_H: f32 = 26.0;
const BTN_PAD: f32 = 12.0;
const BTN_GAP: f32 = 10.0;
const BTN_R: f32 = 5.0;
const BTN_FONT_PX: f32 = 13.0;
/// 有新版：停这么久（光标在胶囊上时不收）
const UPD_HOLD: Duration = Duration::from_secs(10);
/// 更新中 / 正在重启：一直停着（状态变了原地刷新）
const UPD_BUSY_HOLD: Duration = Duration::from_secs(300);
/// 已更新：停这么久
const UPD_DONE_HOLD: Duration = Duration::from_millis(3000);
const MAX_W: f32 = 300.0;
/// 胶囊四周留给投影、上浮、弹簧过冲的边（窗口比胶囊大这么多）。
const MARGIN: f32 = 40.0;
/// 离工作区底边（任务栏上沿）多远。
const MARGIN_BOTTOM: f32 = 12.0;

/// 玻璃参数（设计稿 B 标准；浅玻璃按用户意见调得更透）。
struct Glass {
    blur: f32,
    sat: f32,
    refr_w: f32,
    refr_s: f32,
    chroma: f32,
    tint: [f32; 3],
    a: f32,
}

const LIGHT: Glass = Glass { blur: 4.0, sat: 1.8, refr_w: 12.0, refr_s: 10.0, chroma: 0.10, tint: [1.0, 1.0, 1.0], a: 0.10 };
const DARK: Glass = Glass { blur: 6.0, sat: 1.8, refr_w: 12.0, refr_s: 9.0, chroma: 0.10, tint: [0.071, 0.071, 0.086], a: 0.34 };
/// 染色自动加厚：上限、名字要达到的对比度、取文字区第几百分位的“最不利”像素。
const A_MAX: f32 = 0.82;
const MIN_CONTRAST: f32 = 4.5;
const WORST_PCT: f32 = 0.10;
const SHEEN: f32 = 0.16;
const IN_SHADE: f32 = 0.07;
const RIM_W: f32 = 1.0;
const RIM_BASE: f32 = 0.30;
const RIM_PEAK: f32 = 0.95;
const RIM_BACK: f32 = 0.55;
const EDGE_DARK: f32 = 0.10;
const SH_Y: f32 = 5.0;
const SH_BLUR: f32 = 9.0;
const SH_A_LIGHT: f32 = 0.20;
const SH_A_DARK: f32 = 0.40;
const CT_BLUR: f32 = 1.2;
const CT_A: f32 = 0.10;
const OK_LIGHT: u32 = 0x34C759;
const OK_DARK: u32 = 0x30D158;
const FAIL: u32 = 0xFF9F0A;

// ───────────────────────── 动画 ─────────────────────────

const APPEAR_MS: f32 = 480.0;
const LEAVE_MS: f32 = 360.0;
const HOLD: Duration = Duration::from_millis(1800);
const HOLD_FAIL: Duration = Duration::from_millis(2200);
const SHAKE_MS: f32 = 360.0;
/// 键盘被意外收回：停留更久。
const KBD_HOLD_FAIL: Duration = Duration::from_millis(4000);
/// 加强版轻抖：幅度、时长、来回次数；第二次在 STRONG_AGAIN_MS 后再抖一次。
const STRONG_AMP: f32 = 7.0;
const STRONG_MS: f32 = 520.0;
const STRONG_CYCLES: f32 = 4.0;
const STRONG_AGAIN_MS: f32 = 1400.0;
/// 换内容时宽度弹簧的时长（之后就当停稳）。
const RESIZE_MS: f32 = 420.0;
const FRAME_MS: u32 = 15;
const TIMER_ANIM: usize = 1;

/// CSS cubic-bezier(x1, y1, x2, y2) 在 x 处的值（二分求参数）。
fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..30 {
        let t = (lo + hi) / 2.0;
        let bx = 3.0 * (1.0 - t).powi(2) * t * x1 + 3.0 * (1.0 - t) * t * t * x2 + t.powi(3);
        if bx < x { lo = t } else { hi = t }
    }
    let t = (lo + hi) / 2.0;
    3.0 * (1.0 - t).powi(2) * t * y1 + 3.0 * (1.0 - t) * t * t * y2 + t.powi(3)
}

fn ease_out(x: f32) -> f32 {
    bezier(0.2, 0.8, 0.2, 1.0, x)
}
fn ease_in_out(x: f32) -> f32 {
    bezier(0.4, 0.0, 0.2, 1.0, x)
}
fn ease_in(x: f32) -> f32 {
    bezier(0.4, 0.0, 1.0, 1.0, x)
}

/// 弹簧（ζ≈0.72，过冲≈4%，约 0.35 秒稳定）。t 单位秒。
fn spring(t: f32) -> f32 {
    if t <= 0.0 {
        return 0.0;
    }
    1.0 - (-16.0 * t).exp() * ((15.4 * t).cos() + 16.0 / 15.4 * (15.4 * t).sin())
}

fn sat(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// 一帧的几何（逻辑像素）：胶囊宽高、整体透明度、内容透明度、竖直偏移（向下为正）、水平抖动。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pose {
    w: f32,
    h: f32,
    alpha: f32,
    content: f32,
    dy: f32,
    dx: f32,
}

/// 出现：t 毫秒。
fn appear(t: f32, wf: f32) -> Pose {
    let e = ease_out(t / 160.0);
    let d = 20.0 + 16.0 * e;
    let mut w = d;
    let mut h = d;
    if t > 100.0 {
        w = d.max(H + (wf - H) * spring((t - 100.0) / 1000.0));
    }
    if w > wf {
        // 过冲时竖向轻微压扁，像一滴水
        h = H - 1.6 * ((w - wf) / (0.04 * wf)).min(1.0);
    }
    Pose { w, h, alpha: e, content: sat((t - 160.0) / 200.0), dy: 10.0 * (1.0 - e), dx: 0.0 }
}

/// 收起：t 毫秒（停留结束后从 0 算）。
fn leave(t: f32, wf: f32) -> Pose {
    let content = 1.0 - sat(t / 120.0);
    let mut w = wf - (wf - H) * ease_in_out((t - 60.0) / 200.0);
    let e = ease_in((t - 220.0) / 140.0);
    let d = H - 16.0 * e;
    if t > 220.0 {
        w = d;
    }
    Pose { w, h: d, alpha: 1.0 - e, content, dy: 6.0 * e, dx: 0.0 }
}

/// 没同步成功的轻抖：t 毫秒。
fn shake(t: f32) -> f32 {
    burst(t, 4.0, SHAKE_MS, 3.0)
}

/// 一次衰减的左右抖动。
fn burst(t: f32, amp: f32, dur: f32, cycles: f32) -> f32 {
    if !(0.0..dur).contains(&t) {
        return 0.0;
    }
    let u = t / dur;
    amp * (std::f32::consts::TAU * cycles * u).sin() * (1.0 - u)
}

/// 加强版：抖得更明显，隔一会儿再抖一次（第二次稍弱）。
fn shake_strong(t: f32) -> f32 {
    burst(t, STRONG_AMP, STRONG_MS, STRONG_CYCLES) + burst(t - STRONG_AGAIN_MS, STRONG_AMP * 0.7, STRONG_MS, STRONG_CYCLES)
}

/// 抖动总时长（之后就当停稳）。
fn shake_len(strong: bool) -> f32 {
    if strong { STRONG_AGAIN_MS + STRONG_MS } else { SHAKE_MS }
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Hidden,
    Appear { start: Instant },
    /// 停着：until 到期收起；resize = 换内容时宽度弹簧 (开始, 原宽)；shake = 轻抖开始时间
    Hold { until: Instant, resize: Option<(Instant, f32)>, shake: Option<Instant> },
    Leave { start: Instant },
}

// ───────────────────────── 位图 ─────────────────────────

/// 32 位自顶向下的 DIB，选进一个内存 DC。
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
    bits: *mut u8,
}

impl Dib {
    fn new(w: i32, h: i32) -> Option<Dib> {
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
            Some(Dib { dc, bmp, old, w, h, bits: bits as *mut u8 })
        }
    }

    fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w as usize) * (self.h as usize) * 4) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// ───────────────────────── 背景（每次弹出截一次） ─────────────────────────

/// 截下的背景：原图（算投影深浅用）和模糊 + 提饱和度后的图（玻璃里看到的），线性 RGB 数组（0~1，sRGB 值）。
struct Backdrop {
    w: usize,
    h: usize,
    blurred: Vec<[f32; 3]>,
    /// 背景整体亮度（决定投影深浅、外侧细暗边）
    mean_lum: f32,
}

fn lum(c: [f32; 3]) -> f32 {
    c[0] * 0.2126 + c[1] * 0.7152 + c[2] * 0.0722
}

fn lin(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn rel_lum(c: [f32; 3]) -> f32 {
    lin(c[0]) * 0.2126 + lin(c[1]) * 0.7152 + lin(c[2]) * 0.0722
}

fn contrast(a: f32, b: f32) -> f32 {
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// 一维滑动平均（半径 r），边缘钳到边。
fn box_pass(src: &[[f32; 3]], dst: &mut [[f32; 3]], w: usize, h: usize, r: usize, horizontal: bool) {
    let (n, lines) = if horizontal { (w, h) } else { (h, w) };
    let idx = |line: usize, i: usize| {
        if horizontal { line * w + i } else { i * w + line }
    };
    let k = 1.0 / (2 * r + 1) as f32;
    for line in 0..lines {
        let mut acc = [0.0f32; 3];
        for j in 0..=2 * r {
            let i = (j as isize - r as isize).clamp(0, n as isize - 1) as usize;
            let p = src[idx(line, i)];
            acc[0] += p[0];
            acc[1] += p[1];
            acc[2] += p[2];
        }
        for i in 0..n {
            dst[idx(line, i)] = [acc[0] * k, acc[1] * k, acc[2] * k];
            let out = (i as isize - r as isize).clamp(0, n as isize - 1) as usize;
            let inn = (i as isize + r as isize + 1).clamp(0, n as isize - 1) as usize;
            let (a, b) = (src[idx(line, out)], src[idx(line, inn)]);
            acc[0] += b[0] - a[0];
            acc[1] += b[1] - a[1];
            acc[2] += b[2] - a[2];
        }
    }
}

/// 三次盒式模糊 ≈ 高斯 σ。
fn blur(img: &mut Vec<[f32; 3]>, w: usize, h: usize, sigma: f32) {
    if sigma <= 0.3 || w == 0 || h == 0 {
        return;
    }
    let r = (((12.0 * sigma * sigma / 3.0 + 1.0).sqrt() - 1.0) / 2.0).round().max(1.0) as usize;
    let mut tmp = vec![[0.0f32; 3]; w * h];
    for _ in 0..3 {
        box_pass(img, &mut tmp, w, h, r, true);
        box_pass(&tmp, img, w, h, r, false);
    }
}

/// 截屏幕上 (x, y, w, h) 这块（物理像素）。本窗口此时是隐藏的（或完全透明），截不到自己。
fn capture(x: i32, y: i32, w: i32, h: i32) -> Option<Vec<[f32; 3]>> {
    let mut dib = Dib::new(w, h)?;
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let ok = BitBlt(dib.dc, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_ok();
        ReleaseDC(None, screen);
        let _ = GdiFlush();
        if !ok {
            return None;
        }
    }
    let b = dib.bytes();
    Some(b.chunks_exact(4).map(|p| [p[2] as f32 / 255.0, p[1] as f32 / 255.0, p[0] as f32 / 255.0]).collect())
}

impl Backdrop {
    fn sample(&self, x: f32, y: f32) -> [f32; 3] {
        let x = x.clamp(0.0, self.w as f32 - 1.001);
        let y = y.clamp(0.0, self.h as f32 - 1.001);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let p = |xx: usize, yy: usize| self.blurred[yy * self.w + xx];
        let (a, b, c, d) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
        let mut o = [0.0; 3];
        for i in 0..3 {
            let top = a[i] * (1.0 - fx) + b[i] * fx;
            let bot = c[i] * (1.0 - fx) + d[i] * fx;
            o[i] = top * (1.0 - fy) + bot * fy;
        }
        o
    }
}

// ───────────────────────── 图标与文字（每次换内容画一次） ─────────────────────────

/// 内容层：按最终宽度排好的图标 + 文字（预乘 RGBA），以胶囊中心为原点放置。
struct Content {
    w: usize,
    h: usize,
    px: Vec<[f32; 4]>,
}

fn rgb(hex: u32) -> [f32; 3] {
    [((hex >> 16) & 0xFF) as f32 / 255.0, ((hex >> 8) & 0xFF) as f32 / 255.0, (hex & 0xFF) as f32 / 255.0]
}

/// 灵动岛用的字（界面字体 ui::Font::ui，透明底所以用灰度抗锯齿）。
struct Fonts {
    bold: Font,
    regular: Font,
    button: Font,
}

impl Fonts {
    fn new(s: f32) -> Fonts {
        let q = ANTIALIASED_QUALITY;
        Fonts { bold: Font::ui(FONT_PX, 700, s, q), regular: Font::ui(FONT_PX, 400, s, q), button: Font::ui(BTN_FONT_PX, 700, s, q) }
    }
}

/// 按钮宽（逻辑像素）。
fn btn_w(dc: HDC, fonts: &Fonts, label: &str, s: f32) -> f32 {
    text_px(dc, &fonts.button, label) as f32 / s + 2.0 * BTN_PAD
}

/// 按钮在内容层里的位置（物理像素，左上 x、y，宽、高）。内容层宽 = wf·s + 2。
fn btn_rect(label: &str, wf: f32, s: f32) -> Option<(f32, f32, f32, f32)> {
    let fonts = Fonts::new(s);
    let probe = Dib::new(1, 1)?;
    let bw = btn_w(probe.dc, &fonts, label, s) * s;
    let bh = BTN_H * s;
    let cw = (wf * s).ceil() + 2.0;
    let chh = (H * s).ceil() + 2.0;
    Some((cw - 1.0 - BTN_R * s - bw, (chh - bh) / 2.0, bw, bh))
}

fn text_width(dc: HDC, f: &Font, s: &[u16]) -> i32 {
    text_px(dc, f, &String::from_utf16_lossy(s))
}

/// 主词（粗）、后半句（常规，可空）和胶囊最终宽度（逻辑像素）。放不下时截短一段加省略号：
/// cut_verb 截后半句，否则截主词（主词为空时用“对方”）。后半句为空时不留词间距。
fn layout(name: &str, verb: &str, cut_verb: bool, min_w: f32, button: Option<&str>, s: f32) -> Option<(Vec<u16>, Vec<u16>, f32)> {
    let fonts = Fonts::new(s);
    let probe = Dib::new(1, 1)?;
    let w = |t: &[u16], f: &Font| text_width(probe.dc, f, t) as f32 / s;
    // 有按钮：右边距换成“文字—按钮间距 + 按钮宽 + 按钮右边距”
    let pad_r = match button {
        Some(b) => BTN_GAP + btn_w(probe.dc, &fonts, b, s) + BTN_R,
        None => PAD_R,
    };
    let name = if name.trim().is_empty() { "对方" } else { name.trim() };
    let verb = verb.trim();
    let total = |nm: &[u16], vb: &[u16]| {
        let vw = if vb.is_empty() { 0.0 } else { WORD_GAP + w(vb, &fonts.regular) };
        PAD_L + ICON + GAP + w(nm, &fonts.bold) + vw + pad_r
    };
    let mut nm: Vec<u16> = name.encode_utf16().collect();
    let mut vb: Vec<u16> = verb.encode_utf16().collect();
    // 放不下：截短其中一段（一个字一个字去掉，加省略号）
    let mut chars: Vec<char> = if cut_verb { verb.chars().collect() } else { name.chars().collect() };
    while total(&nm, &vb) > MAX_W && chars.len() > 1 {
        chars.pop();
        let t: Vec<u16> = (chars.iter().collect::<String>() + "…").encode_utf16().collect();
        if cut_verb {
            vb = t;
        } else {
            nm = t;
        }
    }
    let wf = total(&nm, &vb).clamp(min_w, MAX_W);
    Some((nm, vb, wf))
}

/// 画内容层：图标（绿/琥珀圆 + 白色线条图标）+ 主词（粗）+ 动词（常规，可空）+ 文字晕影。
fn draw_content(
    name: &[u16],
    verb: &[u16],
    icon: Icon,
    wf: f32,
    s: f32,
    ok: bool,
    light: bool,
    button: Option<(&str, bool)>,
) -> Option<Content> {
    let cw = (wf * s).ceil() as i32 + 2;
    let chh = (H * s).ceil() as i32 + 2;
    let (cwu, chu) = (cw as usize, chh as usize);
    // 文字蒙版：主词和动词分开画（颜色/透明度不同）
    let fonts = Fonts::new(s);
    let tx = ((PAD_L + ICON + GAP) * s).round() as i32;
    let mut masks = Vec::new();
    for (txt, f, x) in [(name, &fonts.bold, tx), (verb, &fonts.regular, 0)] {
        let mut m = Dib::new(cw, chh)?;
        let x = if masks.is_empty() {
            x
        } else {
            let nw = text_width(m.dc, &fonts.bold, name);
            tx + nw + (WORD_GAP * s).round() as i32
        };
        unsafe {
            SetBkMode(m.dc, TRANSPARENT);
            SetTextColor(m.dc, COLORREF(0x00FF_FFFF));
            // 竖直居中：按字号算基线（ui::Font::baseline_at）
            let y = f.baseline_at(chh as f32 / 2.0 + 0.5 * s).round() as i32;
            draw_text_px(m.dc, f, x, y, &String::from_utf16_lossy(txt));
            let _ = GdiFlush();
        }
        let cov: Vec<f32> = m.bytes().chunks_exact(4).map(|p| p[0].max(p[1]).max(p[2]) as f32 / 255.0).collect();
        masks.push(cov);
    }
    // 按钮文字蒙版
    let btn = match button {
        Some((label, hover)) => {
            let (bx, by, bw, bh) = btn_rect(label, wf, s)?;
            let mut m = Dib::new(cw, chh)?;
            unsafe {
                SetBkMode(m.dc, TRANSPARENT);
                SetTextColor(m.dc, COLORREF(0x00FF_FFFF));
                let tw = text_px(m.dc, &fonts.button, label);
                let x = (bx + (bw - tw as f32) / 2.0).round() as i32;
                let y = fonts.button.baseline_at(by + bh / 2.0 + 0.5 * s).round() as i32;
                draw_text_px(m.dc, &fonts.button, x, y, label);
                let _ = GdiFlush();
            }
            let cov: Vec<f32> = m.bytes().chunks_exact(4).map(|p| p[0].max(p[1]).max(p[2]) as f32 / 255.0).collect();
            Some((bx, by, bw, bh, cov, hover))
        }
        None => None,
    };
    drop(fonts);

    let mut px = vec![[0.0f32; 4]; cwu * chu];
    let over = |d: &mut [f32; 4], c: [f32; 3], a: f32| {
        if a <= 0.0 {
            return;
        }
        let k = 1.0 - a;
        d[0] = c[0] * a + d[0] * k;
        d[1] = c[1] * a + d[1] * k;
        d[2] = c[2] * a + d[2] * k;
        d[3] = a + d[3] * k;
    };
    // 文字晕影：两层文字蒙版合起来模糊
    let mut halo: Vec<[f32; 3]> = masks[0].iter().zip(&masks[1]).map(|(a, b)| {
        let v = a.max(*b);
        [v, v, v]
    }).collect();
    blur(&mut halo, cwu, chu, 1.6 * s);
    let (halo_c, halo_a, halo_dy) = if light { ([1.0, 1.0, 1.0], 0.55, 0) } else { ([0.0, 0.0, 0.0], 0.45, (0.5 * s).round() as usize) };
    for y in 0..chu {
        for x in 0..cwu {
            let sy = y.saturating_sub(halo_dy);
            over(&mut px[y * cwu + x], halo_c, halo[sy * cwu + x][0] * halo_a);
        }
    }
    // 图标：圆 + “复制”（两张叠放的圆角方块，Lucide copy）
    let icol = rgb(if !ok { FAIL } else if light { OK_LIGHT } else { OK_DARK });
    let r = ICON * s / 2.0;
    let (icx, icy) = (PAD_L * s + r, chu as f32 / 2.0);
    let u = ICON * s / 22.0; // 图标画布 22 单位
    let sw = 1.7 * u;
    let sq = |x: f32, y: f32, ox: f32, oy: f32| {
        // 7.6 见方、圆角 1.8 的方块，左上角在 (ox, oy)（22 单位画布）
        let half = 3.8 * u;
        sd_rrect(x, y, icx - 11.0 * u + (ox + 3.8) * u, icy - 11.0 * u + (oy + 3.8) * u, half, half, 1.8 * u)
    };
    for y in 0..chu {
        for x in 0..cwu {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let dc = ((fx - icx).powi(2) + (fy - icy).powi(2)).sqrt() - r;
            let cov = sat(0.5 - dc);
            if cov <= 0.0 {
                continue;
            }
            let p = &mut px[y * cwu + x];
            over(p, icol, cov);
            match icon {
                Icon::Copy => {
                    // 后一张：描边，被前一张（加描边宽的实心）盖住的部分不画
                    let back = sat(0.5 - ((sq(fx, fy, 5.6, 5.6)).abs() - sw / 2.0));
                    let front_fill = sat(0.5 - (sq(fx, fy, 8.8, 8.8) - sw));
                    let front = sat(0.5 - ((sq(fx, fy, 8.8, 8.8)).abs() - sw / 2.0));
                    over(p, [1.0, 1.0, 1.0], back * (1.0 - front_fill) * cov);
                    over(p, [1.0, 1.0, 1.0], front * cov);
                }
                Icon::Keyboard => {
                    // 22 单位画布：描边外框 12.4×8（圆角 2）+ 上排 4 个、下排 2 个 1.5 见方的小键 + 中间空格键
                    let k = |v: f32| v * u;
                    let mut d = sd_rrect(fx, fy, icx, icy, k(6.2), k(4.0), k(2.0)).abs() - sw / 2.0;
                    for (kx, ky) in [(7.4, 9.75), (9.8, 9.75), (12.2, 9.75), (14.6, 9.75), (7.4, 12.25), (14.6, 12.25)] {
                        d = d.min(sd_rrect(fx, fy, icx + k(kx - 11.0), icy + k(ky - 11.0), k(0.75), k(0.75), k(0.4)));
                    }
                    d = d.min(sd_rrect(fx, fy, icx, icy + k(1.25), k(1.95), k(0.75), k(0.75)));
                    over(p, [1.0, 1.0, 1.0], sat(0.5 - d) * cov);
                }
                Icon::Up => {
                    // 竖线 + 两撇（22 单位画布：从 (11, 15.5) 到 (11, 6.5)，箭头两翼 3.6）
                    let k = |v: f32| v * u;
                    let d = seg_d(fx, fy, icx, icy + k(4.5), icx, icy - k(4.5))
                        .min(seg_d(fx, fy, icx - k(3.6), icy - k(0.9), icx, icy - k(4.5)))
                        .min(seg_d(fx, fy, icx + k(3.6), icy - k(0.9), icx, icy - k(4.5)))
                        - sw * 0.6;
                    over(p, [1.0, 1.0, 1.0], sat(0.5 - d) * cov);
                }
                Icon::Check => {
                    let k = |v: f32| v * u;
                    let d = seg_d(fx, fy, icx - k(4.2), icy + k(0.2), icx - k(1.2), icy + k(3.2)).min(seg_d(
                        fx,
                        fy,
                        icx - k(1.2),
                        icy + k(3.2),
                        icx + k(4.5),
                        icy - k(3.0),
                    )) - sw * 0.6;
                    over(p, [1.0, 1.0, 1.0], sat(0.5 - d) * cov);
                }
                Icon::Ring(pct) => {
                    // 淡的整圈 + 从 12 点顺时针的进度弧
                    let rr = 5.6 * u;
                    let dr = ((fx - icx).powi(2) + (fy - icy).powi(2)).sqrt() - rr;
                    let band = sat(0.5 - (dr.abs() - sw * 0.6));
                    let ang = (fx - icx).atan2(-(fy - icy)); // 12 点 = 0，顺时针为正
                    let ang = if ang < 0.0 { ang + std::f32::consts::TAU } else { ang };
                    let done = pct as f32 / 100.0 * std::f32::consts::TAU;
                    over(p, [1.0, 1.0, 1.0], band * cov * if ang <= done { 1.0 } else { 0.35 });
                }
                Icon::Restart => {
                    // 3/4 圈 + 箭头
                    let rr = 5.4 * u;
                    let dr = ((fx - icx).powi(2) + (fy - icy).powi(2)).sqrt() - rr;
                    let ang = (fx - icx).atan2(-(fy - icy));
                    let ang = if ang < 0.0 { ang + std::f32::consts::TAU } else { ang };
                    let arc = if ang > 0.9 { sat(0.5 - (dr.abs() - sw * 0.6)) } else { 0.0 };
                    // 弧的终点在 12 点，箭头顺着顺时针方向朝右
                    let k = |v: f32| v * u;
                    let (ax, ay) = (icx + k(1.4), icy - rr);
                    let head = sat(0.5
                        - (seg_d(fx, fy, ax, ay, ax - k(2.8), ay - k(2.6)).min(seg_d(fx, fy, ax, ay, ax - k(2.8), ay + k(2.6)))
                            - sw * 0.6));
                    over(p, [1.0, 1.0, 1.0], arc.max(head) * cov);
                }
            }
        }
    }
    // 文字
    let tc = if light { [0.051, 0.051, 0.059] } else { [1.0, 1.0, 1.0] };
    let (na, va) = if light { (0.92, 0.70) } else { (1.0, 0.82) };
    for i in 0..cwu * chu {
        over(&mut px[i], tc, masks[0][i] * na);
        over(&mut px[i], tc, masks[1][i] * va);
    }
    // 按钮：浅玻璃上黑底白字，深玻璃上白底黑字；悬停变浅
    if let Some((bx, by, bw, bh, cov, hover)) = btn {
        let (bg, fg) = if light { ([0.110, 0.110, 0.118], [1.0, 1.0, 1.0]) } else { ([0.949, 0.949, 0.969], [0.110, 0.110, 0.118]) };
        let ba = if hover { 0.78 } else { 1.0 };
        let (bcx, bcy) = (bx + bw / 2.0, by + bh / 2.0);
        for y in 0..chu {
            for x in 0..cwu {
                let d = sd_rrect(x as f32 + 0.5, y as f32 + 0.5, bcx, bcy, bw / 2.0, bh / 2.0, bh / 2.0);
                let a = sat(0.5 - d);
                if a > 0.0 {
                    let i = y * cwu + x;
                    over(&mut px[i], bg, a * ba);
                    over(&mut px[i], fg, cov[i] * a);
                }
            }
        }
    }
    Some(Content { w: cwu, h: chu, px })
}

/// 点到线段 (ax, ay)–(bx, by) 的距离。
fn seg_d(x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy).max(1e-6)).clamp(0.0, 1.0);
    ((x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2)).sqrt()
}

/// 圆角矩形的有向距离（中心 c，半宽高 hw/hh，圆角 r）。
fn sd_rrect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - hw + r;
    let qy = (y - cy).abs() - hh + r;
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// 误差函数补（Abramowitz–Stegun 7.1.26），算“模糊后的形状”用。
fn erfc(x: f32) -> f32 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * z);
    let y = t * (0.254_829_6 + t * (-0.284_496_74 + t * (1.421_413_8 + t * (-1.453_152 + t * 1.061_405_4)))) * (-z * z).exp();
    if x >= 0.0 { y } else { 2.0 - y }
}

/// 一个形状（距离场 d）被高斯 σ 模糊后的覆盖率（大形状近似）。
fn soft(d: f32, sigma: f32) -> f32 {
    0.5 * erfc(d / (sigma * std::f32::consts::SQRT_2))
}

// ───────────────────────── 每帧合成 ─────────────────────────

/// 浅/深玻璃及染色厚度：按胶囊最终位置下的背景决定（每次弹出算一次）。
fn choose_glass(bd: &Backdrop, cx: f32, cy: f32, wf: f32, s: f32) -> (bool, f32) {
    let (hw, hh) = (wf * s / 2.0, H * s / 2.0);
    let mut all = Vec::new();
    let mut zone = Vec::new();
    let tx0 = cx - hw + (PAD_L + ICON + GAP) * s;
    let tx1 = cx + hw - PAD_R * s;
    let y0 = (cy - hh).max(0.0) as usize;
    let y1 = ((cy + hh) as usize).min(bd.h);
    let x0 = (cx - hw).max(0.0) as usize;
    let x1 = ((cx + hw) as usize).min(bd.w);
    for y in y0..y1 {
        for x in x0..x1 {
            let c = bd.blurred[y * bd.w + x];
            all.push(lum(c));
            let (fx, fy) = (x as f32, y as f32);
            if fx >= tx0 && fx < tx1 && (fy - cy).abs() < 8.0 * s {
                zone.push(c);
            }
        }
    }
    if all.is_empty() || zone.is_empty() {
        return (true, LIGHT.a);
    }
    let mean = all.iter().sum::<f32>() / all.len() as f32;
    zone.sort_by(|a, b| rel_lum(*a).total_cmp(&rel_lum(*b)));
    let need = |light: bool| -> f32 {
        let g = if light { &LIGHT } else { &DARK };
        let txt = if light { rel_lum([0.05, 0.05, 0.06]) } else { 1.0 };
        let pct = if light { WORST_PCT } else { 1.0 - WORST_PCT };
        let worst = zone[((zone.len() - 1) as f32 * pct) as usize];
        let mut a = g.a;
        while a < A_MAX {
            let c = [0, 1, 2].map(|i| worst[i] * (1.0 - a) + g.tint[i] * a);
            if contrast(rel_lum(c), txt) >= MIN_CONTRAST {
                break;
            }
            a += 0.02;
        }
        a
    };
    if mean < 0.30 {
        (false, need(false))
    } else if mean > 0.70 {
        (true, need(true))
    } else {
        let (al, ad) = (need(true), need(false));
        if al - LIGHT.a <= ad - DARK.a + 0.06 { (true, al) } else { (false, ad) }
    }
}

struct Frame<'a> {
    bd: &'a Backdrop,
    content: &'a Content,
    light: bool,
    tint_a: f32,
    s: f32,
    /// 胶囊停稳时的中心（窗口坐标，物理像素）
    cx: f32,
    cy: f32,
}

impl Frame<'_> {
    /// 把一帧画进窗口位图（预乘 BGRA）。
    fn draw(&self, pose: Pose, out: &mut [u8]) {
        let s = self.s;
        let g = if self.light { &LIGHT } else { &DARK };
        let (w, h) = (self.bd.w, self.bd.h);
        let cx = self.cx + pose.dx * s;
        let cy = self.cy + pose.dy * s;
        let (hw, hh) = (pose.w * s / 2.0, pose.h * s / 2.0);
        let r = hw.min(hh);
        let bg_light = self.bd.mean_lum > 0.45;
        let sha = if bg_light { SH_A_LIGHT } else { SH_A_DARK };
        let (shy, ct_y) = (SH_Y * s, 0.5 * s);
        let reach = (SH_Y + 3.0 * SH_BLUR) * s + 2.0;
        let (lx, ly) = (-0.55f32, -0.83f32);
        let sd = |x: f32, y: f32| sd_rrect(x, y, cx, cy, hw, hh, r);
        // 内容层的左上角（内容按最终宽度、以胶囊中心对齐）
        let (ox, oy) = (cx - self.content.w as f32 / 2.0, cy - self.content.h as f32 / 2.0);
        let alpha = pose.alpha;
        for y in 0..h {
            let fy = y as f32 + 0.5;
            if (fy - cy).abs() > hh + reach {
                out[y * w * 4..(y + 1) * w * 4].fill(0);
                continue;
            }
            for x in 0..w {
                let fx = x as f32 + 0.5;
                let o = (y * w + x) * 4;
                let d = sd(fx, fy);
                if d > reach {
                    out[o..o + 4].fill(0);
                    continue;
                }
                let cover = sat(0.5 - d) * alpha;
                // 外投影 + 贴边接触阴影 + 浅背景上的外侧细暗边
                let mut shadow = 0.0;
                if cover < 1.0 {
                    let ds = sd(fx, fy - shy);
                    let dc = sd(fx, fy - ct_y);
                    shadow = (soft(ds, SH_BLUR * s) * sha + soft(dc, CT_BLUR * s) * CT_A).min(1.0);
                    if bg_light {
                        shadow += EDGE_DARK * sat(1.0 - (d - 0.5 * s).abs() / (0.6 * s));
                    }
                    shadow = sat(shadow) * alpha;
                }
                let mut col = [0.0f32; 3];
                if cover > 0.0 {
                    // 法线（指向外）：距离场的梯度
                    let e = 0.5;
                    let gx = sd(fx + e, fy) - sd(fx - e, fy);
                    let gy = sd(fx, fy + e) - sd(fx, fy - e);
                    let gl = (gx * gx + gy * gy).sqrt() + 1e-6;
                    let (nx, ny) = (gx / gl, gy / gl);
                    let din = (-d).max(0.0);
                    // 折射：边缘一圈按圆弧截面向内取样，红蓝略错开
                    let t = sat(1.0 - din / (g.refr_w * s));
                    let prof = 1.0 - (1.0 - t * t).max(0.0).sqrt();
                    let disp = g.refr_s * s * prof;
                    for (i, k) in [1.0 + g.chroma, 1.0, 1.0 - g.chroma].into_iter().enumerate() {
                        col[i] = self.bd.sample(fx - nx * disp * k, fy - ny * disp * k)[i];
                    }
                    // 染色
                    for i in 0..3 {
                        col[i] = col[i] * (1.0 - self.tint_a) + g.tint[i] * self.tint_a;
                    }
                    // 顶部内高光 + 边缘内光
                    let vy = sat((fy - (cy - hh)) / (2.0 * hh));
                    let sheen = SHEEN * sat(1.0 - vy / 0.55).powf(1.6) + 0.10 * sat(1.0 - din / (3.0 * s));
                    // 底部内阴影
                    let shade = IN_SHADE * sat(1.0 - din / (4.0 * s)).powf(1.5) * ny.max(0.0);
                    // 高光描边（左上来光，右下弱反光）
                    let dot = nx * lx + ny * ly;
                    let ang = if dot > 0.0 { dot } else { -dot * RIM_BACK }.powf(2.2);
                    let rim_a = RIM_BASE + (RIM_PEAK - RIM_BASE) * ang;
                    let band = sat(1.0 - (din - RIM_W * s * 0.5).abs() / (RIM_W * s * 0.5 + 0.5)) * if d < 0.0 { 1.0 } else { 0.0 };
                    let ra = rim_a * band;
                    for c in col.iter_mut() {
                        *c += (1.0 - *c) * sheen;
                        *c *= 1.0 - shade;
                        *c = *c * (1.0 - ra) + ra;
                    }
                }
                // 合成：投影（黑）在下，玻璃在上
                let mut a = cover + shadow * (1.0 - cover);
                let mut pc = [col[0] * cover, col[1] * cover, col[2] * cover];
                // 内容（只在胶囊里，离边 2px 以内渐隐）
                if pose.content > 0.0 && cover > 0.0 {
                    let (cxp, cyp) = (fx - ox, fy - oy);
                    if cxp >= 0.0 && cyp >= 0.0 && (cxp as usize) < self.content.w && (cyp as usize) < self.content.h {
                        let c = self.content.px[cyp as usize * self.content.w + cxp as usize];
                        let ca = c[3] * pose.content * alpha * sat(-d - 2.0 * s);
                        if ca > 0.0 {
                            // c 是“直通”颜色乘了自己的 alpha（预乘）：按比例缩放
                            let k = ca / c[3].max(1e-6);
                            for i in 0..3 {
                                pc[i] = c[i] * k + pc[i] * (1.0 - ca);
                            }
                            a = ca + a * (1.0 - ca);
                        }
                    }
                }
                let a = sat(a);
                out[o] = (sat(pc[2]).min(a) * 255.0).round() as u8;
                out[o + 1] = (sat(pc[1]).min(a) * 255.0).round() as u8;
                out[o + 2] = (sat(pc[0]).min(a) * 255.0).round() as u8;
                out[o + 3] = (a * 255.0).round() as u8;
            }
        }
    }
}

// ───────────────────────── 窗口 ─────────────────────────

struct Island {
    hwnd: HWND,
    /// 窗口位图（窗口大小，预乘 BGRA）
    dib: Option<Dib>,
    bd: Option<Backdrop>,
    content: Option<Content>,
    /// 窗口左上角（屏幕物理像素）
    x: i32,
    y: i32,
    s: f32,
    light: bool,
    tint_a: f32,
    /// 胶囊最终宽度（逻辑像素）
    wf: f32,
    phase: Phase,
    hold: Duration,
    /// 正在显示的提示
    note: Option<Tip>,
    /// 接按钮点击的小窗口（几乎全透明）；悬停中
    hit: Option<HWND>,
    hover: bool,
    /// 排好的主词、后半句（悬停变化时重画内容层用）
    name_verb: Option<(Vec<u16>, Vec<u16>)>,
}

thread_local! {
    static ISLAND: RefCell<Option<Island>> = const { RefCell::new(None) };
}

/// 弹出（或原地刷新）提示。只能在 desktop 线程调用。
pub fn show(note: &Note) {
    if busy_fullscreen() {
        return;
    }
    with_island(|is| is.present(&Tip::clip(note)));
}

/// 键盘跟随提示：ok = 键盘打到了这台（“键盘 <here>”，here = 本机名）；否则键盘被意外收回
/// （“键盘 已收回”，加强版警示）。只能在 desktop 线程调用。
pub fn keyboard(ok: bool, here: &str) {
    if busy_fullscreen() {
        return;
    }
    with_island(|is| is.present(&Tip::keyboard(ok, here)));
}

/// 检查更新的提示要显示成什么。
#[derive(Clone, Debug, PartialEq)]
pub enum Upd {
    /// 有新版；with_button = 能一键更新两台（带“更新”按钮）
    Available {
        version: String,
        with_button: bool,
    },
    Downloading(u8),
    Restarting,
    Updated(String),
    /// 更新失败，短原因（≤ 4 字，可空）
    Failed(&'static str),
}

/// 弹出 / 原地刷新检查更新的提示。只能在 desktop 线程调用。
pub fn update(u: &Upd) {
    if busy_fullscreen() && matches!(u, Upd::Available { .. } | Upd::Updated(_)) {
        return;
    }
    let tip = match u {
        Upd::Available { version, with_button } => {
            Tip::upd("新版", version.clone(), Icon::Up, true, with_button.then_some("更新"), UPD_HOLD)
        }
        Upd::Downloading(p) => Tip::upd("更新中", format!("{p}%"), Icon::Ring(*p), true, None, UPD_BUSY_HOLD),
        Upd::Restarting => Tip::upd("正在重启", String::new(), Icon::Restart, true, None, UPD_BUSY_HOLD),
        Upd::Updated(v) => Tip::upd("已更新", v.clone(), Icon::Check, true, None, UPD_DONE_HOLD),
        Upd::Failed(why) => Tip::upd("更新失败", why.to_string(), Icon::Up, false, None, HOLD_FAIL),
    };
    with_island(|is| is.present(&tip));
}

/// 收起正显示的“键盘 <本机名>”（用户到另一台干活了，那台弹它自己的）：两台同时只留一颗。
/// 停着的收起（正常收起动画）；还在长出来的直接隐藏；别的提示（剪贴板、“键盘 已收回”）不动。
pub fn keyboard_gone() {
    ISLAND.with(|cell| {
        // 从没弹过就没有窗口：不为收起而建窗口
        let Ok(mut g) = cell.try_borrow_mut() else { return };
        let Some(is) = g.as_mut() else { return };
        if !is.note.as_ref().is_some_and(|n| n.icon == Icon::Keyboard && n.ok) {
            return;
        }
        match is.phase {
            Phase::Appear { .. } => is.hide(),
            Phase::Hold { .. } => {
                is.phase = Phase::Leave { start: Instant::now() };
                is.tick();
            }
            Phase::Hidden | Phase::Leave { .. } => {}
        }
    });
}

/// 本机刚弹的“已复制”其实没被对方接收：还显示着就原地改成没同步成功（琥珀图标 + 轻抖）。
/// 没在显示、正在收起、已经是没同步、或显示的不是本机（name）复制的：什么都不做。
pub fn fail_own(name: &str, detail: &str) {
    with_island(|is| {
        if !matches!(is.phase, Phase::Appear { .. } | Phase::Hold { .. }) {
            return;
        }
        let _ = detail;
        let Some(n) = is.note.as_ref() else { return };
        if n.icon != Icon::Copy || !n.ok || n.word != name {
            return;
        }
        let n = Tip { ok: false, ..n.clone() };
        is.present(&n);
    });
}

fn with_island(f: impl FnOnce(&mut Island)) {
    ISLAND.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else { return };
        if g.is_none() {
            let Some(hwnd) = create_window() else { return };
            *g = Some(Island {
                hwnd,
                dib: None,
                bd: None,
                content: None,
                x: 0,
                y: 0,
                s: 1.0,
                light: true,
                tint_a: LIGHT.a,
                wf: MIN_W,
                phase: Phase::Hidden,
                hold: HOLD,
                note: None,
                hit: None,
                hover: false,
                name_verb: None,
            });
        }
        if let Some(is) = g.as_mut() {
            f(is);
        }
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
        // 已注册过也没关系
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

/// 按钮上的接点击窗口：分层、alpha 1（看不见但能接鼠标）、不抢焦点。
fn create_hit_window() -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let class = w!("kmlink-island-hit");
        let wc = WNDCLASSW { lpfnWndProc: Some(hit_proc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
        let _ = RegisterClassW(&wc);
        let ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let h = CreateWindowExW(ex, class, w!(""), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst.into()), None).ok()?;
        let _ = SetLayeredWindowAttributes(h, COLORREF(0), 1, LWA_ALPHA);
        Some(h)
    }
}

unsafe extern "system" fn hit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let set_hover = |on: bool| {
        ISLAND.with(|cell| {
            if let Ok(mut g) = cell.try_borrow_mut()
                && let Some(is) = g.as_mut()
            {
                is.set_hover(on);
            }
        });
    };
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR => unsafe {
            let _ = SetCursor(LoadCursorW(None, IDC_HAND).ok());
            LRESULT(1)
        },
        WM_MOUSEMOVE => {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
            set_hover(true);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            set_hover(false);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            crate::update::update_both();
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            // 胶囊原地变成“更新中 0%”（状态变化随后也会刷新）
            update(&Upd::Downloading(0));
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
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

impl Island {
    /// 窗口大小（物理像素）：按最宽的胶囊 + 弹簧过冲 + 四周留边，固定不变（换内容不用改窗口）。
    fn win_size(s: f32) -> (i32, i32) {
        (((MAX_W * 1.05 + 2.0 * MARGIN) * s).ceil() as i32, ((H + 2.0 * MARGIN) * s).ceil() as i32)
    }

    fn present(&mut self, note: &Tip) {
        let fresh = matches!(self.phase, Phase::Hidden);
        if fresh {
            // 新弹出：定位、截背景
            let Some((work, s)) = cursor_monitor() else { return };
            let (ww, wh) = Island::win_size(s);
            let rest_cy = work.bottom as f32 - (MARGIN_BOTTOM + H / 2.0) * s;
            self.x = (work.left + work.right) / 2 - ww / 2;
            self.y = (rest_cy - wh as f32 / 2.0).round() as i32;
            self.s = s;
            let Some(mut raw) = capture(self.x, self.y, ww, wh) else { return };
            let (wu, hu) = (ww as usize, wh as usize);
            let mean_lum = raw.iter().map(|c| lum(*c)).sum::<f32>() / raw.len().max(1) as f32;
            // 浅/深玻璃的模糊程度不同：先按“原图亮度”粗定，再精确选
            let light_guess = mean_lum > 0.5;
            let g = if light_guess { &LIGHT } else { &DARK };
            blur(&mut raw, wu, hu, g.blur * s);
            for c in raw.iter_mut() {
                let l = lum(*c);
                for v in c.iter_mut() {
                    *v = sat(l + (*v - l) * g.sat);
                }
            }
            self.bd = Some(Backdrop { w: wu, h: hu, blurred: raw, mean_lum });
            self.dib = Dib::new(ww, wh);
            if self.dib.is_none() {
                self.bd = None;
                return;
            }
        }
        let s = self.s;
        let Some((name, verb, wf)) = layout(&note.word, &note.verb, note.cut_verb, note.min_w(), note.button, s) else {
            return;
        };
        let old_wf = self.wf;
        self.wf = wf;
        if fresh {
            let bd = self.bd.as_ref().unwrap();
            let (cx, cy) = (bd.w as f32 / 2.0, bd.h as f32 / 2.0);
            let (light, a) = choose_glass(bd, cx, cy, wf, s);
            self.light = light;
            self.tint_a = a;
        }
        if note.button.is_none() || self.note.as_ref().is_none_or(|n| n.button != note.button) {
            self.hover = false;
        }
        self.content = draw_content(&name, &verb, note.icon, wf, s, note.ok, self.light, note.button.map(|b| (b, self.hover)));
        if self.content.is_none() {
            return;
        }
        self.name_verb = Some((name, verb));
        self.hold = note.hold();
        let was_ok = self.note.as_ref().is_none_or(|n| n.ok);
        self.note = Some(note.clone());
        let now = Instant::now();
        self.phase = match self.phase {
            Phase::Hidden | Phase::Leave { .. } => Phase::Appear { start: now },
            p @ Phase::Appear { .. } => p,
            Phase::Hold { shake, .. } => Phase::Hold {
                until: now + self.hold,
                resize: (old_wf != wf).then_some((now, old_wf)),
                // 刚变成没同步：抖一下
                shake: if !note.ok && was_ok { Some(now) } else { shake },
            },
        };
        // 先把这一帧推上去（隐藏时也能更新），再显示：不会闪出上一颗的旧画面
        self.tick();
        if fresh {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
            }
        }
    }

    /// 推进动画、画一帧，并定下一次定时器。
    fn tick(&mut self) {
        let now = Instant::now();
        let ms = |t: Instant| now.saturating_duration_since(t).as_secs_f32() * 1000.0;
        let wf = self.wf;
        let fail = self.note.as_ref().is_some_and(|n| !n.ok);
        let strong = self.note.as_ref().is_some_and(|n| n.strong);
        let (pose, next) = loop {
            match self.phase {
                Phase::Hidden => return,
                Phase::Appear { start } => {
                    let t = ms(start);
                    if t >= APPEAR_MS {
                        self.phase = Phase::Hold { until: now + self.hold, resize: None, shake: fail.then_some(now) };
                        continue;
                    }
                    break (appear(t, wf), FRAME_MS);
                }
                Phase::Hold { until, resize, shake: sh } => {
                    let has_btn = self.note.as_ref().is_some_and(|n| n.button.is_some());
                    if now >= until {
                        // 有按钮的提示：光标还在胶囊上就再停一会儿
                        if has_btn && self.cursor_on_capsule() {
                            self.phase = Phase::Hold { until: now + Duration::from_millis(600), resize, shake: sh };
                            continue;
                        }
                        self.phase = Phase::Leave { start: now };
                        self.place_hit(false);
                        continue;
                    }
                    if has_btn {
                        self.place_hit(resize.is_none_or(|(t0, _)| ms(t0) >= RESIZE_MS));
                    }
                    let mut p = Pose { w: wf, h: H, alpha: 1.0, content: 1.0, dy: 0.0, dx: 0.0 };
                    let mut animating = false;
                    if let Some((t0, from)) = resize {
                        let t = ms(t0);
                        if t < RESIZE_MS {
                            p.w = from + (wf - from) * spring(t / 1000.0);
                            animating = true;
                        }
                    }
                    if let Some(t0) = sh {
                        let t = ms(t0);
                        if t < shake_len(strong) {
                            p.dx = if strong { shake_strong(t) } else { shake(t) };
                            animating = true;
                        }
                    }
                    let next = if animating { FRAME_MS } else { (until - now).as_millis().clamp(1, 60_000) as u32 };
                    // 有按钮时多看看光标（停留到期前光标进来了要续上）
                    let next = if has_btn { next.min(250) } else { next };
                    break (p, next);
                }
                Phase::Leave { start } => {
                    let t = ms(start);
                    if t >= LEAVE_MS {
                        self.hide();
                        return;
                    }
                    break (leave(t, wf), FRAME_MS);
                }
            }
        };
        self.frame(pose);
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_ANIM, next, None);
        }
    }

    fn frame(&mut self, pose: Pose) {
        let (Some(bd), Some(content), Some(dib)) = (self.bd.as_ref(), self.content.as_ref(), self.dib.as_mut()) else {
            return;
        };
        let f = Frame { bd, content, light: self.light, tint_a: self.tint_a, s: self.s, cx: bd.w as f32 / 2.0, cy: bd.h as f32 / 2.0 };
        f.draw(pose, dib.bytes());
        self.push(255);
    }

    /// 把窗口位图推给系统（a = 整体透明度）。
    fn push(&self, a: u8) {
        let Some(dib) = self.dib.as_ref() else { return };
        let pt = POINT { x: self.x, y: self.y };
        let size = SIZE { cx: dib.w, cy: dib.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: a, AlphaFormat: AC_SRC_ALPHA as u8 };
        unsafe {
            let _ = UpdateLayeredWindow(self.hwnd, None, Some(&pt), Some(&size), Some(dib.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }

    fn hide(&mut self) {
        // 先推一帧全透明：紧接着再弹时截背景不会截到残影
        self.push(0);
        self.phase = Phase::Hidden;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_ANIM);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.place_hit(false);
        self.hover = false;
        // 缓冲用完即还
        self.dib = None;
        self.bd = None;
        self.content = None;
        self.note = None;
        self.name_verb = None;
    }

    /// 胶囊在屏幕上的矩形（物理像素，停稳时）。
    fn capsule_rect(&self) -> RECT {
        let (ww, wh) = Island::win_size(self.s);
        let (cx, cy) = (self.x + ww / 2, self.y + wh / 2);
        let (hw, hh) = ((self.wf * self.s / 2.0).ceil() as i32, (H * self.s / 2.0).ceil() as i32);
        RECT { left: cx - hw, top: cy - hh, right: cx + hw, bottom: cy + hh }
    }

    fn cursor_on_capsule(&self) -> bool {
        let mut p = POINT::default();
        if unsafe { GetCursorPos(&mut p) }.is_err() {
            return false;
        }
        let r = self.capsule_rect();
        p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
    }

    /// 按钮上的接点击窗口：show = 摆到按钮上并显示，否则隐藏。
    fn place_hit(&mut self, show: bool) {
        if !show {
            if let Some(h) = self.hit {
                unsafe {
                    let _ = ShowWindow(h, SW_HIDE);
                }
            }
            return;
        }
        let Some(label) = self.note.as_ref().and_then(|n| n.button) else {
            return;
        };
        let Some((bx, by, bw, bh)) = btn_rect(label, self.wf, self.s) else {
            return;
        };
        if self.hit.is_none() {
            self.hit = create_hit_window();
        }
        let Some(h) = self.hit else { return };
        let (ww, wh) = Island::win_size(self.s);
        let cw = (self.wf * self.s).ceil() + 2.0;
        let chh = (H * self.s).ceil() + 2.0;
        let ox = self.x as f32 + ww as f32 / 2.0 - cw / 2.0;
        let oy = self.y as f32 + wh as f32 / 2.0 - chh / 2.0;
        unsafe {
            let _ = SetWindowPos(
                h,
                Some(HWND_TOPMOST),
                (ox + bx).round() as i32,
                (oy + by).round() as i32,
                bw.round() as i32,
                bh.round() as i32,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    /// 按钮悬停变了：重画内容层。
    fn set_hover(&mut self, on: bool) {
        if self.hover == on {
            return;
        }
        self.hover = on;
        let (Some(n), Some((name, verb))) = (self.note.as_ref(), self.name_verb.as_ref()) else {
            return;
        };
        if let Some(c) = draw_content(name, verb, n.icon, self.wf, self.s, n.ok, self.light, n.button.map(|b| (b, on))) {
            self.content = Some(c);
            self.tick();
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
    fn animation_curves() {
        // 出现：从 Ø20 小圆开始，结束时停在最终尺寸
        let a0 = appear(0.0, 160.0);
        assert!((a0.w - 20.0).abs() < 0.01 && (a0.h - 20.0).abs() < 0.01 && a0.alpha == 0.0);
        let a1 = appear(APPEAR_MS, 160.0);
        assert!((a1.w - 160.0).abs() < 1.5 && (a1.h - H).abs() < 0.5 && a1.content == 1.0);
        // 弹簧过冲约 4%
        let peak = (0..480).map(|t| appear(t as f32, 160.0).w).fold(0.0f32, f32::max);
        assert!(peak > 160.0 && peak < 160.0 * 1.06, "{peak}");
        // 收起：结束时 Ø20、透明
        let l = leave(LEAVE_MS, 160.0);
        assert!((l.w - 20.0).abs() < 0.01 && l.alpha.abs() < 1e-3);
        assert_eq!(shake(SHAKE_MS), 0.0);
        assert!(shake(40.0).abs() > 1.0);
    }

    #[test]
    fn blur_keeps_flat_color() {
        let (w, h) = (20, 10);
        let mut img = vec![[0.3, 0.5, 0.7]; w * h];
        blur(&mut img, w, h, 3.0);
        for c in img {
            assert!((c[0] - 0.3).abs() < 1e-4 && (c[2] - 0.7).abs() < 1e-4);
        }
    }

    #[test]
    fn renders_glass_on_flat_backgrounds() {
        for (bgc, scale) in [([0.97, 0.97, 0.98], 1.5f32), ([0.05, 0.07, 0.12], 1.0), ([0.9, 0.4, 0.1], 2.0)] {
            let (ww, wh) = Island::win_size(scale);
            let (wu, hu) = (ww as usize, wh as usize);
            let bd = Backdrop { w: wu, h: hu, blurred: vec![bgc; wu * hu], mean_lum: lum(bgc) };
            let (name, verb, wf) = layout("WiFi电脑", VERB, false, MIN_W, None, scale).expect("layout");
            assert!((MIN_W..=MAX_W).contains(&wf));
            let (light, a) = choose_glass(&bd, wu as f32 / 2.0, hu as f32 / 2.0, wf, scale);
            // 明显亮 / 明显暗时浅深玻璃是定的；中间调（橙色）两种都可能
            if !(0.30..=0.70).contains(&lum(bgc)) {
                assert_eq!(light, lum(bgc) > 0.5, "bg {bgc:?}");
            }
            assert!((LIGHT.a.min(DARK.a)..=A_MAX + 0.02).contains(&a));
            let content = draw_content(&name, &verb, Icon::Copy, wf, scale, true, light, None).expect("content");
            let f = Frame { bd: &bd, content: &content, light, tint_a: a, s: scale, cx: wu as f32 / 2.0, cy: hu as f32 / 2.0 };
            let mut out = vec![0u8; wu * hu * 4];
            f.draw(Pose { w: wf, h: H, alpha: 1.0, content: 1.0, dy: 0.0, dx: 0.0 }, &mut out);
            // 胶囊中心不透明，四角全透明
            let c = (hu / 2 * wu + wu / 2) * 4;
            assert_eq!(out[c + 3], 255);
            assert_eq!(out[3], 0);
            // 预乘：颜色不超过 alpha
            assert!(out.chunks_exact(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
        }
    }

    #[test]
    fn keyboard_tip_layout_and_shake() {
        let lay = |t: &Tip| layout(&t.word, &t.verb, t.cut_verb, t.min_w(), None, 1.0).expect("layout");
        // “键盘 工位电脑”：不受剪贴板的 MIN_W 限制，名字跟在后面
        let (word, verb, w_ok) = lay(&Tip::keyboard(true, "工位电脑"));
        assert_eq!(String::from_utf16_lossy(&word), "键盘");
        assert_eq!(String::from_utf16_lossy(&verb), "工位电脑");
        assert!((120.0..160.0).contains(&w_ok), "{w_ok}");
        let (_, verb, w_fail) = lay(&Tip::keyboard(false, "工位电脑"));
        assert_eq!(String::from_utf16_lossy(&verb), "已收回");
        assert!(w_fail < w_ok, "{w_fail}");
        // 名字太长：截短名字，“键盘”留着
        let (word, verb, w_long) = lay(&Tip::keyboard(true, &"很长的名字".repeat(10)));
        assert_eq!(String::from_utf16_lossy(&word), "键盘");
        assert!(String::from_utf16_lossy(&verb).ends_with('…'));
        assert!(w_long <= MAX_W);
        // 剪贴板照旧（截主词）
        let (_, _, w_clip) = layout("A", VERB, false, MIN_W, None, 1.0).expect("layout");
        assert_eq!(w_clip, MIN_W);
        let (nm, vb, _) = layout(&"名".repeat(60), VERB, false, MIN_W, None, 1.0).expect("layout");
        assert!(String::from_utf16_lossy(&nm).ends_with('…'));
        assert_eq!(String::from_utf16_lossy(&vb), VERB);
        // 加强版：停得更久，抖两次，幅度更大
        assert!(Tip::keyboard(false, "x").hold() > HOLD_FAIL);
        assert_eq!(Tip::keyboard(true, "x").hold(), HOLD);
        let peak = |f: fn(f32) -> f32, from: f32, to: f32| (from as i32..to as i32).map(|t| f(t as f32).abs()).fold(0.0, f32::max);
        assert!(peak(shake_strong, 0.0, STRONG_MS) > peak(shake, 0.0, SHAKE_MS) + 1.5);
        assert!(peak(shake_strong, STRONG_AGAIN_MS, STRONG_AGAIN_MS + STRONG_MS) > 2.0);
        assert_eq!(shake_strong(STRONG_AGAIN_MS - 100.0), 0.0);
        assert_eq!(shake_strong(shake_len(true) + 1.0), 0.0);
        for ok in [true, false] {
            let t = Tip::keyboard(ok, "机械革命");
            let (word, verb, wf) = layout(&t.word, &t.verb, true, H, None, 1.5).expect("layout");
            assert!(draw_content(&word, &verb, Icon::Keyboard, wf, 1.5, ok, true, None).is_some());
        }
    }

    /// 手动看效果：cargo test dump_keyboard_tips -- --ignored，输出到 %TEMP%\kmlink-island-*.bmp。
    #[test]
    #[ignore]
    fn dump_keyboard_tips() {
        crate::ui::load_fonts();
        let scale = 2.0f32;
        for (tag, bgc) in [("light", [0.95, 0.95, 0.96]), ("dark", [0.06, 0.08, 0.14])] {
            for (verb, ok) in [("工位电脑", true), ("已收回", false)] {
                let (ww, wh) = Island::win_size(scale);
                let (wu, hu) = (ww as usize, wh as usize);
                let bd = Backdrop { w: wu, h: hu, blurred: vec![bgc; wu * hu], mean_lum: lum(bgc) };
                let (name, verb, wf) = layout("键盘", verb, true, H, None, scale).unwrap();
                let (light, a) = choose_glass(&bd, wu as f32 / 2.0, hu as f32 / 2.0, wf, scale);
                let content = draw_content(&name, &verb, Icon::Keyboard, wf, scale, ok, light, None).unwrap();
                let f = Frame { bd: &bd, content: &content, light, tint_a: a, s: scale, cx: wu as f32 / 2.0, cy: hu as f32 / 2.0 };
                let mut out = vec![0u8; wu * hu * 4];
                f.draw(Pose { w: wf, h: H, alpha: 1.0, content: 1.0, dy: 0.0, dx: 0.0 }, &mut out);
                // 合成到背景上，写 24 位 BMP（自底向上）
                let mut bmp = Vec::new();
                let row = (wu * 3 + 3) & !3;
                let size = 54 + row * hu;
                bmp.extend_from_slice(b"BM");
                bmp.extend_from_slice(&(size as u32).to_le_bytes());
                bmp.extend_from_slice(&[0; 4]);
                bmp.extend_from_slice(&54u32.to_le_bytes());
                bmp.extend_from_slice(&40u32.to_le_bytes());
                bmp.extend_from_slice(&(wu as i32).to_le_bytes());
                bmp.extend_from_slice(&(hu as i32).to_le_bytes());
                bmp.extend_from_slice(&1u16.to_le_bytes());
                bmp.extend_from_slice(&24u16.to_le_bytes());
                bmp.extend_from_slice(&[0; 24]);
                for y in (0..hu).rev() {
                    let mut r = Vec::with_capacity(row);
                    for x in 0..wu {
                        let p = &out[(y * wu + x) * 4..][..4];
                        let k = 1.0 - p[3] as f32 / 255.0;
                        for (c, bgv) in [(p[0], bgc[2]), (p[1], bgc[1]), (p[2], bgc[0])] {
                            r.push((c as f32 + bgv * 255.0 * k).round().clamp(0.0, 255.0) as u8);
                        }
                    }
                    r.resize(row, 0);
                    bmp.extend_from_slice(&r);
                }
                let path = std::env::temp_dir().join(format!("kmlink-island-{tag}-{}.bmp", if ok { "ok" } else { "lost" }));
                std::fs::write(&path, bmp).unwrap();
            }
        }
    }

    /// 检查更新的几种提示渲染成 BMP（手动跑：cargo test -- --ignored dump_update_tips），看样子用。
    #[test]
    #[ignore]
    fn dump_update_tips() {
        crate::ui::load_fonts();
        let scale = 2.0f32;
        let tips = [
            ("avail", Tip::upd("新版", "0.2.0".into(), Icon::Up, true, Some("更新"), UPD_HOLD), false),
            ("avail-hover", Tip::upd("新版", "0.2.0".into(), Icon::Up, true, Some("更新"), UPD_HOLD), true),
            ("solo", Tip::upd("新版", "0.2.0".into(), Icon::Up, true, None, UPD_HOLD), false),
            ("down", Tip::upd("更新中", "45%".into(), Icon::Ring(45), true, None, UPD_BUSY_HOLD), false),
            ("restart", Tip::upd("正在重启", String::new(), Icon::Restart, true, None, UPD_BUSY_HOLD), false),
            ("done", Tip::upd("已更新", "0.2.0".into(), Icon::Check, true, None, UPD_DONE_HOLD), false),
            ("fail", Tip::upd("更新失败", "网络不通".into(), Icon::Up, false, None, HOLD_FAIL), false),
        ];
        for (tag, bgc) in [("light", [0.95, 0.95, 0.96]), ("dark", [0.06, 0.08, 0.14])] {
            for (name_tag, t, hover) in &tips {
                let (ww, wh) = Island::win_size(scale);
                let (wu, hu) = (ww as usize, wh as usize);
                let bd = Backdrop { w: wu, h: hu, blurred: vec![bgc; wu * hu], mean_lum: lum(bgc) };
                let (name, verb, wf) = layout(&t.word, &t.verb, true, t.min_w(), t.button, scale).unwrap();
                let (light, a) = choose_glass(&bd, wu as f32 / 2.0, hu as f32 / 2.0, wf, scale);
                let content = draw_content(&name, &verb, t.icon, wf, scale, t.ok, light, t.button.map(|b| (b, *hover))).unwrap();
                let f = Frame { bd: &bd, content: &content, light, tint_a: a, s: scale, cx: wu as f32 / 2.0, cy: hu as f32 / 2.0 };
                let mut out = vec![0u8; wu * hu * 4];
                f.draw(Pose { w: wf, h: H, alpha: 1.0, content: 1.0, dy: 0.0, dx: 0.0 }, &mut out);
                let mut bmp = Vec::new();
                let row = (wu * 3 + 3) & !3;
                let size = 54 + row * hu;
                bmp.extend_from_slice(b"BM");
                bmp.extend_from_slice(&(size as u32).to_le_bytes());
                bmp.extend_from_slice(&[0; 4]);
                bmp.extend_from_slice(&54u32.to_le_bytes());
                bmp.extend_from_slice(&40u32.to_le_bytes());
                bmp.extend_from_slice(&(wu as i32).to_le_bytes());
                bmp.extend_from_slice(&(hu as i32).to_le_bytes());
                bmp.extend_from_slice(&1u16.to_le_bytes());
                bmp.extend_from_slice(&24u16.to_le_bytes());
                bmp.extend_from_slice(&[0; 24]);
                for y in (0..hu).rev() {
                    let mut r = Vec::with_capacity(row);
                    for x in 0..wu {
                        let p = &out[(y * wu + x) * 4..][..4];
                        let k = 1.0 - p[3] as f32 / 255.0;
                        for (c, bgv) in [(p[0], bgc[2]), (p[1], bgc[1]), (p[2], bgc[0])] {
                            r.push((c as f32 + bgv * 255.0 * k).round().clamp(0.0, 255.0) as u8);
                        }
                    }
                    r.resize(row, 0);
                    bmp.extend_from_slice(&r);
                }
                let path = std::env::temp_dir().join(format!("kmlink-upd-{tag}-{name_tag}.bmp"));
                std::fs::write(&path, bmp).unwrap();
            }
        }
    }
}
