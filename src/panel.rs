//! 托盘面板（替换原生右键菜单；设计稿 设计\托盘菜单\proof.html，底用系统亚克力）。
//!
//! 左键、右键点托盘图标都打开，弹在图标正上方（任务栏在上面时弹在下方），离任务栏 12。宽 300，从上到下：
//!   头部：状态图标块（已连接 绿 / 已暂停 琥珀 / 等待连接 灰）+ 标题 + 灰字（对方名字），右边“暂停 / 继续”；
//!   声音：分段“关 | 1 | 2 | 两台”，灰字说在哪台放；键盘跟随、开机自启两个开关；
//!   更新一行（检查更新 / 正在检查… / 已是最新 / 新版 x 更新 / 正在更新 45% / 正在重启 / 已更新到 x / 更新失败 重试）；
//!   屏幕排布与连接设置 ›、退出。
//! 点开关、分段、暂停、检查更新、更新不关面板；点设置、退出，按 Esc、点到外面（失去激活）就关。
//!
//! 窗口：WS_POPUP + TOOLWINDOW + TOPMOST，DWM 系统亚克力（DWMWA_SYSTEMBACKDROP_TYPE = 临时窗口）、圆角、深浅色跟系统，
//! 客户区整块扩展成“玻璃”：自己画的像素带透明度，透明处露出亚克力。src/ui.rs 的 Painter 和 GDI 文字只会画在不透明底上，
//! 所以每帧画两遍（黑底、白底），两遍一减就得到每个像素的透明度（预乘 BGRA），再 BitBlt 上去。
//!
//! 跑在 desktop 线程。点了的动作放进队列（`take_cmds`），叫醒 desktop 的消息循环去做（和菜单事件一样，不在窗口过程里动状态）。

use crate::ui::{Font, Painter, Rgba, Seg, Surface, hex, hexa};
use std::cell::RefCell;
use std::time::Instant;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_SYSTEMBACKDROP_TYPE, DWM_WINDOW_CORNER_PREFERENCE, DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOPMOST, IDC_ARROW, IDC_HAND, KillTimer, LoadCursorW, RegisterClassW, SW_SHOW,
    SWP_NOACTIVATE, SetCursor, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, WA_INACTIVE, WM_ACTIVATE, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};
use windows::core::w;

const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_SPIN: usize = 1;
/// 没抢到前台时的兜底：前台窗口换了（用户点了别处）就关
const TIMER_FG: usize = 2;

/// 面板上显示的东西（desktop 每次打开 / 状态变了时给）。
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub connected: bool,
    pub paused: bool,
    pub peer_name: String,
    /// 声音：选中第几段（0 关 / 1 / 2 / 3 两台）和段下灰字
    pub audio: usize,
    pub audio_hint: String,
    pub follow: bool,
    pub autostart: bool,
    pub upd: crate::update::State,
    /// 对方连着且支持一键更新
    pub peer_upd: bool,
}

/// 用户在面板上点的动作。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    Pause(bool),
    Follow(bool),
    Autostart(bool),
    Audio(usize),
    Check,
    Update,
    Settings,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hot {
    None,
    Pause,
    Seg(usize),
    Follow,
    Autostart,
    UpdRow,
    UpdBtn,
    Settings,
    Quit,
}

// 布局（逻辑像素）
const W: f32 = 300.0;
const PAD: f32 = 6.0;
const IN: f32 = 16.0; // 行内容左右边
const HEAD_H: f32 = 58.0;
const ROW_H: f32 = 42.0;
const AUDIO_H: f32 = 52.0;
const UPD_H: f32 = 52.0;
const LINK_H: f32 = 38.0;
const SEG_W: f32 = 38.0;
const SEG_H: f32 = 28.0;
const SW_W: f32 = 38.0;
const SW_H: f32 = 23.0;
/// 离任务栏的距离
const GAP: f32 = 12.0;

#[derive(Clone, Copy)]
struct R {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl R {
    const fn new(x: f32, y: f32, w: f32, h: f32) -> R {
        R { x, y, w, h }
    }
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
}

/// 各块的位置（逻辑像素，客户区坐标）。
struct Lay {
    head: R,
    pause: R,
    audio: R,
    segs: [R; 4],
    follow: R,
    autostart: R,
    upd: R,
    upd_btn: Option<R>,
    settings: R,
    quit: R,
    seps: [f32; 3],
    h: f32,
}

struct Theme {
    dark: bool,
    fg: Rgba,
    mutd: Rgba,
    sep: Rgba,
    hover: Rgba,
    fill: Rgba,
    seg_track: Rgba,
    seg_thumb: Rgba,
    sw_off: Rgba,
    green: Rgba,
    green_bg: Rgba,
    amber: Rgba,
    amber_bg: Rgba,
    ink: Rgba,
    ink_fg: Rgba,
    gray: Rgba,
    /// 盖在亚克力上的一层底色（字更清楚）
    veil: Rgba,
}

const LIGHT: Theme = Theme {
    dark: false,
    fg: hex(0x1C1C1E),
    mutd: hex(0x8E8E93),
    sep: hexa(0x000000, 0.08),
    hover: hexa(0x000000, 0.05),
    fill: hexa(0x000000, 0.06),
    seg_track: hexa(0x767680, 0.12),
    seg_thumb: hex(0xFFFFFF),
    sw_off: hexa(0x787880, 0.20),
    green: hex(0x34C759),
    green_bg: hexa(0x34C759, 0.14),
    amber: hex(0xFF9F0A),
    amber_bg: hexa(0xFF9F0A, 0.14),
    ink: hex(0x1C1C1E),
    ink_fg: hex(0xFFFFFF),
    gray: hex(0xAEAEB2),
    veil: hexa(0xFAFAFC, 0.35),
};

const DARK: Theme = Theme {
    dark: true,
    fg: hex(0xF2F2F7),
    mutd: hex(0x98989F),
    sep: hexa(0xFFFFFF, 0.08),
    hover: hexa(0xFFFFFF, 0.07),
    fill: hexa(0xFFFFFF, 0.10),
    seg_track: hexa(0x767680, 0.24),
    seg_thumb: hex(0x636366),
    sw_off: hexa(0x787880, 0.36),
    green: hex(0x30D158),
    green_bg: hexa(0x30D158, 0.18),
    amber: hex(0xFF9F0A),
    amber_bg: hexa(0xFF9F0A, 0.18),
    ink: hex(0xF2F2F7),
    ink_fg: hex(0x1C1C1E),
    gray: hex(0x636366),
    veil: hexa(0x242428, 0.35),
};

struct Fonts {
    title: Font,
    sub: Font,
    row: Font,
    link: Font,
    btn: Font,
    seg: Font,
    seg_on: Font,
    ver: Font,
    upd_b: Font,
}

impl Fonts {
    fn new(s: f32) -> Fonts {
        let q = ANTIALIASED_QUALITY;
        Fonts {
            title: Font::ui(14.0, 700, s, q),
            sub: Font::ui(11.5, 400, s, q),
            row: Font::ui(13.5, 400, s, q),
            link: Font::ui(13.0, 400, s, q),
            btn: Font::ui(12.5, 700, s, q),
            seg: Font::ui(12.5, 400, s, q),
            seg_on: Font::ui(12.5, 700, s, q),
            ver: Font::ui(12.0, 400, s, q),
            upd_b: Font::ui(13.5, 700, s, q),
        }
    }
}

struct Panel {
    hwnd: HWND,
    s: f32,
    view: View,
    th: &'static Theme,
    fonts: Fonts,
    hot: Hot,
    press: Hot,
    tracking: bool,
    spin0: Instant,
    /// 物理像素：窗口左上、宽高
    rect: (i32, i32, i32, i32),
    /// 弹出的锚点（托盘图标矩形，物理像素）：高度变了重新摆位置
    anchor: RECT,
    /// 打开后的前台窗口（没抢到前台时是别人的窗口）
    fg0: HWND,
}

thread_local! {
    static PANEL: RefCell<Option<Panel>> = const { RefCell::new(None) };
    static CMDS: RefCell<Vec<Cmd>> = const { RefCell::new(Vec::new()) };
    /// 上次因失去激活而关掉的时间：紧接着的托盘点击（就是让它失去激活的那一下）不再打开（点图标 = 开关）
    static CLOSED_AT: RefCell<Option<Instant>> = const { RefCell::new(None) };
    /// 调试截图（panel-demo）：不因失去激活而关
    static DEMO: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 叫醒 desktop 消息循环的窗口（PostMessage WM_APP_WAKE）。
static WAKE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub fn set_wake(hwnd: isize) {
    WAKE.store(hwnd, std::sync::atomic::Ordering::Relaxed);
}

/// 取走用户点的动作。
pub fn take_cmds() -> Vec<Cmd> {
    CMDS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

pub fn is_open() -> bool {
    PANEL.with(|p| p.try_borrow().map(|g| g.is_some()).unwrap_or(true))
}

/// 托盘图标被点了：开着就关，关着就开（刚因为点图标失去激活而关掉的，不再打开）。
pub fn toggle(anchor: RECT, view: View) {
    if is_open() {
        close();
        return;
    }
    if CLOSED_AT.with(|c| c.borrow().is_some_and(|t| t.elapsed().as_millis() < 300)) {
        return;
    }
    open(anchor, view);
}

/// 状态变了：开着就刷新。
pub fn refresh(view: View) {
    let h = PANEL.with(|p| {
        let Ok(mut g) = p.try_borrow_mut() else {
            return None;
        };
        let pn = g.as_mut()?;
        if pn.view == view {
            return None;
        }
        pn.view = view;
        Some(pn.hwnd)
    });
    if let Some(h) = h {
        relayout(h);
    }
}

pub fn close() {
    let h = PANEL.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.take()).map(|pn| pn.hwnd));
    if let Some(h) = h {
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}

fn system_dark() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let mut v: u32 = 1;
    let mut n = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut _),
            Some(&mut n),
        )
    };
    r.is_ok() && v == 0
}

fn open(anchor: RECT, view: View) {
    let (s, _) = monitor_at(anchor);
    let th: &'static Theme = if system_dark() { &DARK } else { &LIGHT };
    let Some(hwnd) = create_window(th.dark) else {
        return;
    };
    let pn = Panel {
        hwnd,
        s,
        view,
        th,
        fonts: Fonts::new(s),
        hot: Hot::None,
        press: Hot::None,
        tracking: false,
        spin0: Instant::now(),
        rect: (0, 0, 0, 0),
        anchor,
        fg0: HWND::default(),
    };
    PANEL.with(|p| *p.borrow_mut() = Some(pn));
    relayout(hwnd);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let fg = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
        PANEL.with(|p| {
            if let Ok(mut g) = p.try_borrow_mut()
                && let Some(pn) = g.as_mut()
            {
                pn.fg0 = fg;
            }
        });
        if fg != hwnd {
            SetTimer(Some(hwnd), TIMER_FG, 150, None);
        }
    }
}

/// 图标所在显示器的缩放和工作区。
fn monitor_at(anchor: RECT) -> (f32, RECT) {
    unsafe {
        let p = POINT { x: (anchor.left + anchor.right) / 2, y: (anchor.top + anchor.bottom) / 2 };
        let mon = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let s = if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx > 0 { dx as f32 / 96.0 } else { 1.0 };
        (s.clamp(0.5, 4.0), mi.rcWork)
    }
}

fn create_window(dark: bool) -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let class = w!("kmlink-panel");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
        let _ = RegisterClassW(&wc);
        let h = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            w!("kmlink"),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;
        let on: i32 = dark as i32;
        let _ = DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const i32 as *const _, 4);
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const DWM_WINDOW_CORNER_PREFERENCE as *const _, 4);
        let bd = DWMSBT_TRANSIENTWINDOW;
        let _ = DwmSetWindowAttribute(h, DWMWA_SYSTEMBACKDROP_TYPE, &bd as *const DWM_SYSTEMBACKDROP_TYPE as *const _, 4);
        let m = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
        let _ = DwmExtendFrameIntoClientArea(h, &m);
        Some(h)
    }
}

/// 布局（随更新那一行的样子变高度）。
fn layout(v: &View, f: &Fonts, measure: &dyn Fn(&Font, &str) -> f32) -> Lay {
    let x0 = PAD;
    let iw = W - 2.0 * PAD;
    let mut y = PAD;
    let head = R::new(x0, y, iw, HEAD_H);
    let pause_t = if v.paused { "继续" } else { "暂停" };
    let pw = measure(&f.btn, pause_t) + 14.0 + 12.0 + 6.0 + 2.0;
    let pause = R::new(W - IN - pw + 4.0, head.cy() - 15.0, pw, 30.0);
    y += HEAD_H;
    let sep0 = y;
    y += 1.0;
    let audio = R::new(x0, y, iw, AUDIO_H);
    let sx = W - IN - 4.0 * SEG_W - 4.0;
    let segs = [0, 1, 2, 3].map(|k| R::new(sx + 2.0 + SEG_W * k as f32, audio.cy() - SEG_H / 2.0 + 2.0, SEG_W, SEG_H - 4.0));
    y += AUDIO_H;
    let follow = R::new(x0, y, iw, ROW_H);
    y += ROW_H;
    let autostart = R::new(x0, y, iw, ROW_H);
    y += ROW_H;
    let sep1 = y;
    y += 1.0;
    let big = !matches!(
        v.upd,
        crate::update::State::Idle | crate::update::State::Checking | crate::update::State::Latest | crate::update::State::Updated(_)
    );
    let uh = if big { UPD_H } else { ROW_H };
    let upd = R::new(x0, y + if big { 4.0 } else { 0.0 }, iw, uh);
    y += uh + if big { 8.0 } else { 0.0 };
    let upd_btn = upd_button(v).map(|t| {
        let bw = measure(&f.btn, t) + 28.0;
        R::new(upd.x + upd.w - 10.0 - bw, upd.cy() - 14.0, bw, 28.0)
    });
    let sep2 = y;
    y += 1.0;
    let settings = R::new(x0, y, iw, LINK_H);
    y += LINK_H;
    let quit = R::new(x0, y, iw, LINK_H);
    y += LINK_H + PAD;
    Lay { head, pause, audio, segs, follow, autostart, upd, upd_btn, settings, quit, seps: [sep0, sep1, sep2], h: y }
}

/// 更新那一行右边的按钮文字（没有按钮 None）。
fn upd_button(v: &View) -> Option<&'static str> {
    use crate::update::State as U;
    match v.upd {
        U::Available(_) if v.peer_upd || !v.connected => Some("更新"),
        U::Available(_) => Some("更新本机"),
        U::Failed(_) => Some("重试"),
        _ => None,
    }
}

fn hit(l: &Lay, v: &View, x: f32, y: f32) -> Hot {
    if l.pause.contains(x, y) {
        return Hot::Pause;
    }
    for (k, r) in l.segs.iter().enumerate() {
        if r.contains(x, y) {
            return Hot::Seg(k);
        }
    }
    if l.follow.contains(x, y) {
        return Hot::Follow;
    }
    if l.autostart.contains(x, y) {
        return Hot::Autostart;
    }
    if let Some(b) = l.upd_btn
        && b.contains(x, y)
    {
        return Hot::UpdBtn;
    }
    // 只有“检查更新 / 已是最新 / 已更新”这种整行可点
    if l.upd.contains(x, y) && matches!(v.upd, crate::update::State::Idle | crate::update::State::Latest | crate::update::State::Updated(_))
    {
        return Hot::UpdRow;
    }
    if l.settings.contains(x, y) {
        return Hot::Settings;
    }
    if l.quit.contains(x, y) {
        return Hot::Quit;
    }
    Hot::None
}

fn relayout(h: HWND) {
    let r = PANEL.with(|p| {
        let Ok(mut g) = p.try_borrow_mut() else {
            return None;
        };
        let pn = g.as_mut()?;
        let probe = Surface::new(1, 1)?;
        let s = pn.s;
        let measure = |f: &Font, t: &str| crate::ui::text_w(probe_dc(&probe), f, s, t);
        let l = layout(&pn.view, &pn.fonts, &measure);
        let (pw, ph) = ((W * s).round() as i32, (l.h * s).round() as i32);
        let (_, work) = monitor_at(pn.anchor);
        let a = pn.anchor;
        let gap = (GAP * s).round() as i32;
        let cx = (a.left + a.right) / 2;
        let x = (cx - pw / 2).clamp(work.left + gap, (work.right - gap - pw).max(work.left + gap));
        // 图标在工作区下面（任务栏在下）：弹在上方；在上面：弹在下方；在两侧：贴着工作区底边
        let y = if a.top >= work.bottom - 2 {
            work.bottom - gap - ph
        } else if a.bottom <= work.top + 2 {
            work.top + gap
        } else {
            (a.top - gap - ph).clamp(work.top + gap, work.bottom - gap - ph)
        };
        pn.rect = (x, y, pw, ph);
        Some((x, y, pw, ph))
    });
    if let Some((x, y, w, hh)) = r {
        unsafe {
            let _ = SetWindowPos(h, Some(HWND_TOPMOST), x, y, w, hh, SWP_NOACTIVATE);
            let _ = InvalidateRect(Some(h), None, false);
        }
        ensure_spin(h);
    }
}

fn probe_dc(s: &Surface) -> windows::Win32::Graphics::Gdi::HDC {
    s.dc()
}

fn spinning(v: &View) -> bool {
    matches!(v.upd, crate::update::State::Checking | crate::update::State::Restarting(_))
}

fn ensure_spin(h: HWND) {
    let spin = PANEL.with(|p| p.try_borrow().ok().and_then(|g| g.as_ref().map(|pn| spinning(&pn.view)))).unwrap_or(false);
    unsafe {
        if spin {
            SetTimer(Some(h), TIMER_SPIN, 33, None);
        } else {
            let _ = KillTimer(Some(h), TIMER_SPIN);
        }
    }
}

// ───────────────────────── 绘制 ─────────────────────────

fn paint(h: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc = unsafe { BeginPaint(h, &mut ps) };
    PANEL.with(|p| {
        let Ok(g) = p.try_borrow() else { return };
        let Some(pn) = g.as_ref() else { return };
        let (_, _, pw, ph) = pn.rect;
        let (Some(mut black), Some(mut white)) = (Surface::new(pw, ph), Surface::new(pw, ph)) else {
            return;
        };
        for (surf, bg) in [(&mut black, hex(0x000000)), (&mut white, hex(0xFFFFFF))] {
            let mut pt = Painter::new(surf, pn.s);
            pt.clear(bg);
            render(&mut pt, pn);
        }
        // 两遍一减：a = 1 - (白 - 黑)，颜色（预乘）= 黑底那遍
        let wp: Vec<u32> = white.pixels().to_vec();
        for (b, w) in black.pixels().iter_mut().zip(wp) {
            let ch = |v: u32, sh: u32| ((v >> sh) & 0xFF) as i32;
            let mut a = 0;
            for sh in [0, 8, 16] {
                a += 255 - (ch(w, sh) - ch(*b, sh)).clamp(0, 255);
            }
            let a = (a / 3).clamp(0, 255) as u32;
            let mut out = a << 24;
            for sh in [0, 8, 16] {
                out |= (ch(*b, sh).min(a as i32) as u32) << sh;
            }
            *b = out;
        }
        black.blit(dc);
    });
    unsafe {
        let _ = EndPaint(h, &ps);
    }
}

fn render(p: &mut Painter, pn: &Panel) {
    use crate::update::State as U;
    let th = pn.th;
    let f = &pn.fonts;
    let v = &pn.view;
    let l = layout(v, f, &|ft, t| p.measure(ft, t));
    let hot = pn.hot;

    // 盖一层底色（亚克力上字更清楚）
    p.rrect(0.0, 0.0, W, l.h, 0.0, th.veil);

    // 悬停：整行浅底
    let row_hover = |p: &mut Painter, r: R| p.rrect(r.x, r.y, r.w, r.h, 10.0, th.hover);
    match hot {
        Hot::Follow => row_hover(p, l.follow),
        Hot::Autostart => row_hover(p, l.autostart),
        Hot::UpdRow => row_hover(p, l.upd),
        Hot::Settings => row_hover(p, l.settings),
        Hot::Quit => row_hover(p, l.quit),
        _ => {}
    }

    // 头部
    let (block, title) = if !v.connected {
        (th.gray, "等待连接")
    } else if v.paused {
        (th.amber, "已暂停")
    } else {
        (th.green, "已连接")
    };
    let ib = R::new(IN, l.head.cy() - 16.0, 32.0, 32.0);
    p.rrect(ib.x, ib.y, ib.w, ib.h, 9.0, block);
    // 图标：两块叠放的圆角方块（同托盘图标）
    p.rrect(ib.x + 7.0, ib.y + 7.0, 12.0, 12.0, 3.0, hexa(0xFFFFFF, 0.6));
    p.rrect(ib.x + 13.0, ib.y + 13.0, 12.0, 12.0, 3.0, hex(0xFFFFFF));
    let tx = ib.x + ib.w + 10.0;
    p.text_mid(tx, l.head.cy() - 8.5, title, &f.title, th.fg);
    let sub = if !v.connected {
        "对方未开或不在同网".to_string()
    } else if v.peer_name.trim().is_empty() {
        "对方".to_string()
    } else {
        v.peer_name.chars().take(16).collect()
    };
    p.text_mid(tx, l.head.cy() + 9.5, &sub, &f.sub, th.mutd);
    // 暂停 / 继续
    let pb = l.pause;
    let (bg, fg) = if v.paused { (th.ink, th.ink_fg) } else { (th.fill, th.fg) };
    let bg = if hot == Hot::Pause { bg.lerp(if th.dark { hex(0xFFFFFF) } else { hex(0x000000) }, 0.06) } else { bg };
    p.rrect(pb.x, pb.y, pb.w, pb.h, 15.0, bg);
    let gx = pb.x + 14.0;
    if v.paused {
        // 三角
        p.stroke(
            &[
                Seg::Line(gx, pb.cy() - 4.5, gx, pb.cy() + 4.5),
                Seg::Line(gx, pb.cy() - 4.5, gx + 7.0, pb.cy()),
                Seg::Line(gx, pb.cy() + 4.5, gx + 7.0, pb.cy()),
            ],
            1.6,
            fg,
        );
    } else {
        p.rrect(gx, pb.cy() - 4.5, 2.4, 9.0, 1.0, fg);
        p.rrect(gx + 4.6, pb.cy() - 4.5, 2.4, 9.0, 1.0, fg);
    }
    p.text_mid(gx + 12.0, pb.cy(), if v.paused { "继续" } else { "暂停" }, &f.btn, fg);

    let sep = |p: &mut Painter, y: f32| p.rrect(IN, y, W - 2.0 * IN, 1.0, 0.0, th.sep);
    sep(p, l.seps[0]);

    // 声音
    p.text_mid(IN, l.audio.cy() - 8.0, "声音", &f.row, th.fg);
    let hint = if v.connected { v.audio_hint.clone() } else { "连上后生效".to_string() };
    p.text_mid(IN, l.audio.cy() + 10.0, &hint, &f.sub, th.mutd);
    let tr = R::new(l.segs[0].x - 2.0, l.audio.cy() - SEG_H / 2.0 + 2.0 - 2.0, SEG_W * 4.0 + 4.0, SEG_H);
    p.rrect(tr.x, tr.y, tr.w, tr.h, 8.0, th.seg_track);
    for (k, label) in ["关", "1", "2", "两台"].iter().enumerate() {
        let r = l.segs[k];
        let on = k == v.audio;
        if on {
            p.shadow(r.x, r.y, r.w, r.h, 6.0, 0.0, 1.0, 2.0, 0.0, hexa(0, if th.dark { 0.35 } else { 0.12 }));
            p.rrect(r.x, r.y, r.w, r.h, 6.0, th.seg_thumb);
        } else if hot == Hot::Seg(k) {
            p.rrect(r.x, r.y, r.w, r.h, 6.0, th.hover);
        }
        let ft = if on { &f.seg_on } else { &f.seg };
        let w = p.measure(ft, label);
        p.text_mid(r.cx() - w / 2.0, r.cy(), label, ft, th.fg);
    }

    // 开关
    for (r, label, on) in [(l.follow, "键盘跟随", v.follow), (l.autostart, "开机自启", v.autostart)] {
        p.text_mid(IN, r.cy(), label, &f.row, th.fg);
        let sx = W - IN - SW_W;
        let sy = r.cy() - SW_H / 2.0;
        p.rrect(sx, sy, SW_W, SW_H, SW_H / 2.0, if on { th.green } else { th.sw_off });
        let kx = if on { sx + SW_W - 2.0 - 19.0 } else { sx + 2.0 };
        p.shadow(kx, sy + 2.0, 19.0, 19.0, 9.5, 0.0, 1.0, 3.0, 0.0, hexa(0, 0.2));
        p.circle(kx + 9.5, sy + SW_H / 2.0, 9.5, hex(0xFFFFFF));
    }
    sep(p, l.seps[1]);

    // 更新
    let u = l.upd;
    let ver = crate::update::VERSION;
    let spin_at = |p: &mut Painter, cx: f32, cy: f32| {
        let a0 = pn.spin0.elapsed().as_secs_f32() * std::f32::consts::TAU * 1.2;
        p.stroke(&[Seg::Arc(cx, cy, 6.0, a0, a0 + 4.2)], 1.7, th.mutd);
    };
    let block_row = |p: &mut Painter, bg: Rgba| p.rrect(u.x, u.y, u.w, u.h, 12.0, bg);
    let icon_circle = |p: &mut Painter, c: Rgba| p.circle(IN + 10.0, u.cy(), 10.0, c);
    let two = |p: &mut Painter, bold: &str, rest: &str, sub: &str| {
        let x = IN + 28.0;
        let w = p.text_mid(x, u.cy() - 8.0, bold, &f.upd_b, th.fg);
        if !rest.is_empty() {
            p.text_mid(x + w + 5.0, u.cy() - 8.0, rest, &f.row, th.fg);
        }
        p.text_mid(x, u.cy() + 10.0, sub, &f.sub, th.mutd);
    };
    let up_arrow = |p: &mut Painter, cx: f32, cy: f32| {
        p.stroke(
            &[
                Seg::Line(cx, cy + 4.5, cx, cy - 4.5),
                Seg::Line(cx - 3.6, cy - 0.9, cx, cy - 4.5),
                Seg::Line(cx + 3.6, cy - 0.9, cx, cy - 4.5),
            ],
            1.8,
            hex(0xFFFFFF),
        );
    };
    match &v.upd {
        U::Idle => {
            p.text_mid(IN, u.cy(), "检查更新", &f.row, th.fg);
            let w = p.measure(&f.ver, ver);
            p.text_mid(W - IN - w, u.cy(), ver, &f.ver, th.mutd);
        }
        U::Checking => {
            p.text_mid(IN, u.cy(), "正在检查…", &f.row, th.mutd);
            spin_at(p, W - IN - 6.0, u.cy());
        }
        U::Latest | U::Updated(_) => {
            let t = if let U::Updated(nv) = &v.upd { format!("已更新到 {nv}") } else { "已是最新".to_string() };
            p.text_mid(IN, u.cy(), &t, &f.row, th.fg);
            let w = p.measure(&f.ver, ver);
            p.text_mid(W - IN - w, u.cy(), ver, &f.ver, th.mutd);
            let cx = W - IN - w - 12.0;
            p.stroke(
                &[Seg::Line(cx - 4.5, u.cy(), cx - 1.5, u.cy() + 3.0), Seg::Line(cx - 1.5, u.cy() + 3.0, cx + 4.5, u.cy() - 3.5)],
                1.8,
                th.green,
            );
        }
        U::Available(nv) => {
            block_row(p, th.green_bg);
            icon_circle(p, th.green);
            up_arrow(p, IN + 10.0, u.cy());
            let sub = if v.peer_upd || !v.connected { "两台一起更新" } else { "对方需手动更新" };
            two(p, "新版", nv, sub);
        }
        U::Downloading(_, pct) => {
            block_row(p, th.green_bg);
            let (cx, cy) = (IN + 10.0, u.cy());
            p.stroke(&[Seg::Arc(cx, cy, 8.0, 0.0, std::f32::consts::TAU)], 2.4, th.green.alpha(0.25));
            let a0 = -std::f32::consts::FRAC_PI_2;
            let a1 = a0 + std::f32::consts::TAU * (*pct as f32 / 100.0).max(0.02);
            p.stroke(&[Seg::Arc(cx, cy, 8.0, a0, a1)], 2.4, th.green);
            two(p, "正在更新", &format!("{pct}%"), "两台会断开片刻");
        }
        U::Restarting(_) => {
            block_row(p, th.green_bg);
            spin_at(p, IN + 10.0, u.cy());
            two(p, "正在重启", "", "马上自动重连");
        }
        U::Failed(e) => {
            block_row(p, th.amber_bg);
            icon_circle(p, th.amber);
            p.stroke(&[Seg::Line(IN + 10.0, u.cy() - 5.0, IN + 10.0, u.cy() + 1.5)], 2.0, hex(0xFFFFFF));
            p.circle(IN + 10.0, u.cy() + 4.8, 1.2, hex(0xFFFFFF));
            let e = e.split('（').next().unwrap_or("").trim();
            two(p, "更新失败", "", if e.is_empty() { "稍后再试" } else { e });
        }
    }
    if let (Some(b), Some(t)) = (l.upd_btn, upd_button(v)) {
        let ink = matches!(v.upd, U::Available(_));
        let (bg, fg) = if ink { (th.ink, th.ink_fg) } else { (th.fill, th.fg) };
        let bg = if hot == Hot::UpdBtn { bg.lerp(if th.dark { hex(0x000000) } else { hex(0xFFFFFF) }, 0.15) } else { bg };
        p.rrect(b.x, b.y, b.w, b.h, 14.0, bg);
        let w = p.measure(&f.btn, t);
        p.text_mid(b.cx() - w / 2.0, b.cy(), t, &f.btn, fg);
    }
    sep(p, l.seps[2]);

    // 链接
    p.text_mid(IN, l.settings.cy(), "屏幕排布与连接设置", &f.link, th.fg);
    let cx = W - IN - 3.0;
    let cy = l.settings.cy();
    p.stroke(&[Seg::Line(cx - 3.5, cy - 4.5, cx, cy), Seg::Line(cx - 3.5, cy + 4.5, cx, cy)], 1.5, th.mutd);
    p.text_mid(IN, l.quit.cy(), "退出", &f.link, th.fg);
}

/// 调试截图：`kmlink panel-demo <状态>` 单独弹出面板（不用主程序），停 secs 秒。
/// 状态：idle / checking / latest / avail / solo / down / restart / fail / done / paused / offline。
pub fn demo(state: &str, secs: u64) {
    use crate::update::State as U;
    let mut v = View {
        connected: true,
        paused: false,
        peer_name: "机械革命".into(),
        audio: 1,
        audio_hint: "全在 机械革命 放".into(),
        follow: true,
        autostart: true,
        upd: U::Idle,
        peer_upd: true,
    };
    match state {
        "checking" => v.upd = U::Checking,
        "latest" => v.upd = U::Latest,
        "avail" => v.upd = U::Available("0.2.0".into()),
        "solo" => {
            v.upd = U::Available("0.2.0".into());
            v.peer_upd = false;
        }
        "down" => v.upd = U::Downloading("0.2.0".into(), 45),
        "restart" => v.upd = U::Restarting("0.2.0".into()),
        "fail" => v.upd = U::Failed("连不上 GitHub（连接）".into()),
        "done" => v.upd = U::Updated("0.2.0".into()),
        "paused" => v.paused = true,
        "offline" => {
            v.connected = false;
            v.peer_upd = false;
        }
        _ => {}
    }
    DEMO.with(|d| d.set(true));
    // 锚点：主显示器工作区右下角（假装托盘图标在任务栏上）
    let (_, work) = monitor_at(RECT { left: 0, top: 0, right: 1, bottom: 1 });
    let anchor = RECT { left: work.right - 200, top: work.bottom + 4, right: work.right - 168, bottom: work.bottom + 36 };
    open(anchor, v);
    let end = Instant::now() + std::time::Duration::from_secs(secs);
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};
        let mut m = MSG::default();
        while Instant::now() < end && is_open() {
            while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    close();
}

// ───────────────────────── 窗口过程 ─────────────────────────

fn push_cmd(c: Cmd) {
    CMDS.with(|q| q.borrow_mut().push(c));
    let h = WAKE.load(std::sync::atomic::Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(HWND(h as *mut core::ffi::c_void)),
                crate::hub::WM_APP_WAKE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

fn pos(lp: LPARAM, s: f32) -> (f32, f32) {
    let x = (lp.0 & 0xFFFF) as i16 as f32;
    let y = ((lp.0 >> 16) & 0xFFFF) as i16 as f32;
    (x / s, y / s)
}

fn hot_at(lp: LPARAM) -> Hot {
    PANEL
        .with(|p| {
            let g = p.try_borrow().ok()?;
            let pn = g.as_ref()?;
            let probe = Surface::new(1, 1)?;
            let measure = |f: &Font, t: &str| crate::ui::text_w(probe.dc(), f, pn.s, t);
            let l = layout(&pn.view, &pn.fonts, &measure);
            let (x, y) = pos(lp, pn.s);
            Some(hit(&l, &pn.view, x, y))
        })
        .unwrap_or(Hot::None)
}

fn set_hot(h: HWND, hot: Hot) {
    let changed = PANEL.with(|p| {
        let Ok(mut g) = p.try_borrow_mut() else {
            return false;
        };
        let Some(pn) = g.as_mut() else { return false };
        let c = pn.hot != hot;
        pn.hot = hot;
        c
    });
    if changed {
        unsafe {
            let _ = InvalidateRect(Some(h), None, false);
        }
    }
}

fn click(h: HWND, hot: Hot) {
    use crate::update::State as U;
    let Some(v) = PANEL.with(|p| p.try_borrow().ok().and_then(|g| g.as_ref().map(|pn| pn.view.clone()))) else {
        return;
    };
    let mut nv = v.clone();
    let cmd = match hot {
        Hot::Pause => {
            nv.paused = !v.paused;
            Some(Cmd::Pause(nv.paused))
        }
        Hot::Seg(k) if k != v.audio => {
            nv.audio = k;
            Some(Cmd::Audio(k))
        }
        Hot::Follow => {
            nv.follow = !v.follow;
            Some(Cmd::Follow(nv.follow))
        }
        Hot::Autostart => {
            nv.autostart = !v.autostart;
            Some(Cmd::Autostart(nv.autostart))
        }
        Hot::UpdRow => {
            nv.upd = U::Checking;
            Some(Cmd::Check)
        }
        Hot::UpdBtn => match v.upd {
            U::Available(ref ver) => {
                nv.upd = U::Downloading(ver.clone(), 0);
                Some(Cmd::Update)
            }
            _ => {
                nv.upd = U::Checking;
                Some(Cmd::Check)
            }
        },
        Hot::Settings => Some(Cmd::Settings),
        Hot::Quit => Some(Cmd::Quit),
        _ => None,
    };
    let Some(cmd) = cmd else { return };
    if matches!(cmd, Cmd::Settings | Cmd::Quit) {
        close();
    } else {
        // 先按点了之后的样子画（desktop 做完后会再刷新成实际状态）
        refresh(nv);
        let _ = h;
    }
    push_cmd(cmd);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER if wp.0 == TIMER_FG => {
            let fg = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
            let fg0 = PANEL.with(|p| p.try_borrow().ok().and_then(|g| g.as_ref().map(|pn| pn.fg0)));
            if !DEMO.with(|d| d.get()) && fg0.is_some_and(|f0| fg != f0 && fg != hwnd) {
                close();
            }
            LRESULT(0)
        }
        WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE && !DEMO.with(|d| d.get()) => {
            CLOSED_AT.with(|c| *c.borrow_mut() = Some(Instant::now()));
            close();
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => {
            close();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let tracking =
                PANEL.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(|pn| std::mem::replace(&mut pn.tracking, true))));
            if tracking == Some(false) {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe {
                    let _ = TrackMouseEvent(&mut tme);
                }
            }
            set_hot(hwnd, hot_at(lp));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            PANEL.with(|p| {
                if let Ok(mut g) = p.try_borrow_mut()
                    && let Some(pn) = g.as_mut()
                {
                    pn.tracking = false;
                }
            });
            set_hot(hwnd, Hot::None);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let h = hot_at(lp);
            PANEL.with(|p| {
                if let Ok(mut g) = p.try_borrow_mut()
                    && let Some(pn) = g.as_mut()
                {
                    pn.press = h;
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let h = hot_at(lp);
            let pressed =
                PANEL.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(|pn| std::mem::replace(&mut pn.press, Hot::None))));
            if pressed == Some(h) && h != Hot::None {
                click(hwnd, h);
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let hot = PANEL.with(|p| p.try_borrow().ok().and_then(|g| g.as_ref().map(|pn| pn.hot))).unwrap_or(Hot::None);
            let c = if hot == Hot::None { IDC_ARROW } else { IDC_HAND };
            unsafe {
                let _ = SetCursor(LoadCursorW(None, c).ok());
            }
            LRESULT(1)
        }
        WM_TIMER if wp.0 == TIMER_SPIN => {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            ensure_spin(hwnd);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
