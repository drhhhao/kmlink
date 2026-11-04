//! 设置窗口（单独进程：`kmlink.exe settings`，关掉就退出，不占主程序内存）。纯 Win32 + GDI，中文用系统字体（微软雅黑）。
//!
//! 窗口约 760 x 560（按 DPI 缩放），标题“kmlink 设置”。内容：
//! 1. 排布画布（上半部分，约 720 x 340）：
//!    - 本机显示器（sys::monitors()）画成浅蓝色矩形，标“本机”；对方显示器（config::load_peer_desk()，
//!      没有时用 1920x1080 的占位，并在画布上写“对方未连接，先用默认尺寸”）画成浅灰色矩形，标对方名字。
//!    - 整体按比例缩放到画布内居中。对方的矩形组可以用鼠标拖动；松手时吸附到本机外接矩形最近的一侧（贴边、不重叠），
//!      沿边方向保留位置 → 得到 Layout{side, offset（本机像素，取整）, scale}。拖动时实时显示，吸附后重画。
//!    - 画布下方一行文字：“对方在本机右侧，往下错开 120 像素”（offset 为 0 时“对齐”）。
//!    - 缩放：一个下拉框或 -/+ 按钮调 layout.scale（0.25 ~ 4，步进 0.05；显示为“对方尺寸比例 1.00”），用于两边 DPI 相差大时。
//! 2. 连接：
//!    - “对方 IP”编辑框（cfg.peer），提示文字：“两台里填一台即可，建议 WiFi 那台填有线那台的 IP”。
//!    - “配对密钥”编辑框（cfg.key，64 位十六进制）+ “复制”按钮（复制到剪贴板）+ “新密钥”按钮。
//!      提示：“两台必须相同：在一台复制，粘贴到另一台。”
//!    - “本机地址”只读文字：列出本机 IPv4（GetAdaptersAddresses 或 std 解析主机名均可；没有就省略）。
//! 3. 底部按钮：“保存”（校验密钥和对方地址（同 net：IP 或 IP:端口）；config::update 只改 peer/key，排布真被改过才写 layout 并 rev = next_rev；关闭窗口）
//!    和“取消”。主程序会在约 2 秒内发现配置变化并重新加载、把排布同步给对方。
//! 只允许一个设置窗口（命名互斥量 Local\kmlink-settings），再打开就把已有的窗口提到前面。
//! 文字要简洁。所有坐标是物理像素（进程已 PerMonitorV2），字体大小按 GetDpiForWindow 缩放。

use crate::config::{self, Config};
use crate::geom::{Desk, Layout, Rect, Side};
use crate::sys;
use std::cell::RefCell;
use std::net::{IpAddr, SocketAddr};
use windows::Win32::Foundation::{
    COLORREF, GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_WINDOW, COLOR_WINDOWTEXT, CreateCompatibleBitmap, CreateCompatibleDC,
    CreateFontW, CreatePen, DC_BRUSH, DC_PEN, DEFAULT_CHARSET, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FW_BOLD, FW_NORMAL, FillRect, GetMonitorInfoW, GetStockObject, GetSysColor, GetSysColorBrush, HBRUSH,
    HDC, HFONT, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, NULL_BRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_DOT,
    Rectangle, SRCCOPY, SelectObject, SetBkColor, SetBkMode, SetDCBrushColor, SetDCPenColor, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    BN_CLICKED, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetClientRect, GetDlgCtrlID,
    GetDlgItem, GetMessageW, GetWindowTextLengthW, GetWindowTextW, HMENU, IDC_ARROW, IDC_SIZEALL, IsDialogMessageW, IsIconic, LoadCursorW,
    MB_ICONWARNING, MB_OK, MSG, MessageBoxW, MoveWindow, PostQuitMessage, RegisterClassExW, SW_RESTORE, SW_SHOW, SWP_NOACTIVATE,
    SWP_NOZORDER, SendMessageW, SetCursor, SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CAPTURECHANGED, WM_CLOSE, WM_COMMAND,
    WM_CTLCOLORSTATIC, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR,
    WM_SETFONT, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_EX_CLIENTEDGE, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

// ---------------------------------------------------------------------------
// 纯逻辑（可单测）
// ---------------------------------------------------------------------------

const SCALE_MIN: f64 = 0.25;
const SCALE_MAX: f64 = 4.0;
const SCALE_STEP: f64 = 0.05;

/// 把沿边错开量夹到“两块矩形沿边至少共享 1 像素”的范围内（否则没有可跨越的区段）。
/// me：本机外接矩形；pw/ph：对方外接矩形缩放到本机像素后的宽高。
fn clamp_offset(me: Rect, side: Side, pw: f64, ph: f64, offset: i32) -> i32 {
    let (mlen, plen) = if side.horizontal() { (me.h(), ph) } else { (me.w(), pw) };
    // 对方区间 [o, o+plen) 与本机 [0, mlen) 相交：o <= mlen-1 且 o + plen > 0
    let hi = mlen - 1;
    let lo = (-plen).floor() as i32 + 1;
    if lo > hi { hi } else { offset.clamp(lo, hi) }
}

/// 松手吸附：对方矩形左上角放在 (l, t)（本机像素），大小 pw x ph。
/// 选中心“最靠外”的一侧（按两者尺寸归一化），贴边，保留沿边位置，再夹紧。
fn snap(me: Rect, pw: f64, ph: f64, l: f64, t: f64) -> (Side, i32) {
    let cx = l + pw / 2.0;
    let cy = t + ph / 2.0;
    let mcx = (me.l as f64 + me.r as f64) / 2.0;
    let mcy = (me.t as f64 + me.b as f64) / 2.0;
    // 贴边且居中时 |nx| 或 |ny| 恰为 1
    let nx = (cx - mcx) / ((me.w() as f64 + pw) / 2.0).max(1.0);
    let ny = (cy - mcy) / ((me.h() as f64 + ph) / 2.0).max(1.0);
    let (side, raw) = if nx.abs() >= ny.abs() {
        (if nx >= 0.0 { Side::Right } else { Side::Left }, t - me.t as f64)
    } else {
        (if ny >= 0.0 { Side::Bottom } else { Side::Top }, l - me.l as f64)
    };
    (side, clamp_offset(me, side, pw, ph, raw.round() as i32))
}

fn sanitize_scale(s: f64) -> f64 {
    if s.is_finite() && s > 0.01 { s.clamp(SCALE_MIN, SCALE_MAX) } else { 1.0 }
}

/// 比例按 0.05 的格子走 `steps` 步，夹在 0.25 ~ 4。
fn step_scale(s: f64, steps: i32) -> f64 {
    let n = (sanitize_scale(s) / SCALE_STEP).round() as i32 + steps;
    let v = (n as f64 * SCALE_STEP).clamp(SCALE_MIN, SCALE_MAX);
    (v * 100.0).round() / 100.0
}

/// 用户在窗口里改过排布没有（和打开时比）。没改就不动文件里的排布和 rev。
fn layout_changed(opened: &Layout, now: &Layout) -> bool {
    opened.side != now.side || opened.offset != now.offset || (opened.scale - now.scale).abs() > 1e-9
}

/// 保存时对磁盘上最新配置的修改：只改 peer/key；排布真改了才写排布并递增 rev。
fn apply_save(c: &mut Config, peer: &str, key: String, opened: &Layout, now: &Layout) {
    c.peer = peer.trim().to_string();
    c.key = key;
    if layout_changed(opened, now) {
        c.layout.layout = *now;
        c.layout.rev = config::next_rev(c.layout.rev);
    }
}

/// 按 net 的规则解析对方地址：空 = 不主动连接（Ok(None)）；IP 或 IP:端口
/// （IPv6 可写裸地址、[地址] 或 [地址]:端口），端口 1..=65535。其余（含主机名）Err。
fn peer_ip(peer: &str) -> Result<Option<IpAddr>, ()> {
    let p = peer.trim();
    if p.is_empty() {
        return Ok(None);
    }
    if let Ok(sa) = p.parse::<SocketAddr>() {
        return if sa.port() == 0 { Err(()) } else { Ok(Some(sa.ip())) };
    }
    let bare = p.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(p);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(Some(ip));
    }
    // 电脑名（net 会做名字解析）：主机名[:端口]。全是数字和点的必须是合法 IP（防 10.0.0 这种笔误被当成名字）。
    let (host, port) = match p.rsplit_once(':') {
        Some((h, ps)) => (h, Some(ps)),
        None => (p, None),
    };
    if let Some(ps) = port
        && !matches!(ps.parse::<u16>(), Ok(n) if n > 0)
    {
        return Err(());
    }
    let label_ok = |l: &str| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if host.is_empty() || host.len() > 253 || host.bytes().all(|b| b.is_ascii_digit() || b == b'.') || !host.split('.').all(label_ok) {
        return Err(());
    }
    Ok(None)
}

/// 对方地址是不是本机（回环、0.0.0.0/::、或本机网卡地址；IPv4 映射的 IPv6 按 IPv4 看）。
fn is_own_addr(peer: &str, ips: &[String]) -> bool {
    let p = peer.trim();
    let host = p.rsplit_once(':').map_or(p, |(h, _)| h).trim();
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let ip = match peer_ip(p) {
        Ok(Some(IpAddr::V6(v6))) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        Ok(Some(ip)) => ip,
        _ => return false,
    };
    ip.is_loopback() || ip.is_unspecified() || ips.iter().any(|s| *s == ip.to_string())
}

/// 画布下方的说明：“对方在右侧，下移 120 像素” / “对方在左侧，对齐”。
fn status_text(l: &Layout) -> String {
    let o = l.offset;
    let mv = if o == 0 {
        "对齐".to_string()
    } else {
        let dir = match (l.side.horizontal(), o > 0) {
            (true, true) => "下移",
            (true, false) => "上移",
            (false, true) => "右移",
            (false, false) => "左移",
        };
        format!("{dir} {} 像素", o.unsigned_abs())
    };
    format!("对方在{}，{mv}", l.side.label_zh())
}

// ---------------------------------------------------------------------------
// 窗口
// ---------------------------------------------------------------------------

const TITLE: &str = "kmlink 设置";
const CANVAS_CLASS: PCWSTR = w!("kmlink_settings_canvas");
const MAIN_CLASS: PCWSTR = w!("kmlink_settings");

// 控件 id；保存/取消用 IDOK/IDCANCEL，这样回车、Esc 也能用
const ID_SAVE: i32 = 1;
const ID_CANCEL: i32 = 2;
const ID_SEC_LAYOUT: i32 = 101;
const ID_CANVAS: i32 = 102;
const ID_STATUS: i32 = 103;
const ID_SCALE_LBL: i32 = 104;
const ID_MINUS: i32 = 105;
const ID_SCALE_VAL: i32 = 106;
const ID_PLUS: i32 = 107;
const ID_SEC_CONN: i32 = 110;
const ID_PEER_LBL: i32 = 111;
const ID_PEER: i32 = 112;
const ID_PEER_HINT: i32 = 113;
const ID_KEY_LBL: i32 = 114;
const ID_KEY: i32 = 115;
const ID_COPY: i32 = 116;
const ID_NEWKEY: i32 = 117;
const ID_KEY_HINT: i32 = 118;
const ID_ADDR_LBL: i32 = 119;
const ID_ADDR: i32 = 120;

// 样式位（部分常量在未启用的 feature 里，直接写数值）
const SS_CENTER: u32 = 0x1;
const SS_NOPREFIX: u32 = 0x80;
const SS_CENTERIMAGE: u32 = 0x200;
const SS_ENDELLIPSIS: u32 = 0x4000;
const ES_AUTOHSCROLL: u32 = 0x80;
const ES_READONLY: u32 = 0x800;
const BS_PUSHBUTTON: u32 = 0x0;
const BS_DEFPUSHBUTTON: u32 = 0x1;
const EM_SETLIMITTEXT: u32 = 0xC5;
const HTCLIENT: usize = 1;
const CF_UNICODETEXT: u32 = 13;

/// 客户区大小（96 DPI 下的逻辑像素）。
const CLIENT_W: i32 = 760;
const CLIENT_H: i32 = 600;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Section,
    Label,
    Hint,
    Value,
    Edit,
    RoEdit,
    Button,
    DefButton,
    Canvas,
}

struct Ctl {
    id: i32,
    kind: Kind,
    text: &'static str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

const fn c(id: i32, kind: Kind, text: &'static str, x: i32, y: i32, w: i32, h: i32) -> Ctl {
    Ctl { id, kind, text, x, y, w, h }
}

/// 所有控件：位置是 96 DPI 下的逻辑像素，左右边距 20，内容宽 720（20..740）。创建顺序即 Tab 顺序。
const CTLS: &[Ctl] = &[
    c(ID_SEC_LAYOUT, Kind::Section, "屏幕排布", 20, 12, 200, 24),
    c(ID_CANVAS, Kind::Canvas, "", 20, 40, 720, 300), // 40..340
    c(ID_STATUS, Kind::Label, "", 20, 348, 500, 26),
    c(ID_SCALE_LBL, Kind::Label, "尺寸比例", 540, 348, 72, 26), // 540..612
    c(ID_MINUS, Kind::Button, "−", 616, 348, 30, 26),          // 616..646
    c(ID_SCALE_VAL, Kind::Value, "1.00", 650, 348, 56, 26),     // 650..706
    c(ID_PLUS, Kind::Button, "+", 710, 348, 30, 26),            // 710..740
    c(ID_SEC_CONN, Kind::Section, "连接", 20, 390, 200, 24),
    c(ID_PEER_LBL, Kind::Label, "对方 IP", 20, 420, 60, 26),
    c(ID_PEER, Kind::Edit, "", 84, 420, 260, 26), // 84..344
    c(ID_PEER_HINT, Kind::Hint, "填一台即可", 356, 420, 200, 26),
    c(ID_KEY_LBL, Kind::Label, "配对密钥", 20, 456, 60, 26),
    c(ID_KEY, Kind::Edit, "", 84, 456, 540, 26), // 84..624，64 位十六进制放得下
    c(ID_COPY, Kind::Button, "复制", 630, 456, 52, 26), // 630..682
    c(ID_NEWKEY, Kind::Button, "新密钥", 688, 456, 52, 26), // 688..740
    c(ID_KEY_HINT, Kind::Hint, "两台须相同", 84, 484, 300, 22),
    c(ID_ADDR_LBL, Kind::Label, "本机地址", 20, 514, 60, 26),
    c(ID_ADDR, Kind::RoEdit, "", 84, 518, 656, 20), // 无边框只读，下移 4 与左侧标签文字对齐
    c(ID_SAVE, Kind::DefButton, "保存", 560, 556, 86, 30), // 560..646
    c(ID_CANCEL, Kind::Button, "取消", 654, 556, 86, 30), // 654..740，底边 586，客户区高 600
];

// 颜色
const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}
const C_BG: COLORREF = rgb(0xF6, 0xF7, 0xF9);
const C_FRAME: COLORREF = rgb(0xD8, 0xDC, 0xE0);
const C_MINE_FILL: COLORREF = rgb(0xD6, 0xE8, 0xFF);
const C_MINE_EDGE: COLORREF = rgb(0x4A, 0x8F, 0xE2);
const C_PEER_FILL: COLORREF = rgb(0xE4, 0xE6, 0xEA);
const C_PEER_EDGE: COLORREF = rgb(0x8E, 0x95, 0x9E);
const C_PEER_DRAG: COLORREF = rgb(0x50, 0x57, 0x60);
const C_GHOST: COLORREF = rgb(0x2F, 0x7B, 0xE0);
const C_TEXT: COLORREF = rgb(0x1F, 0x23, 0x28);
const C_HINT: COLORREF = rgb(0x80, 0x86, 0x8C);

/// 画布坐标 = o + 本机像素 * k
#[derive(Clone, Copy, Debug)]
struct View {
    k: f64,
    ox: f64,
    oy: f64,
}

struct Drag {
    m0: (i32, i32),
    p0: (f64, f64),
    pos: (f64, f64),
    view: View,
}

struct App {
    main: HWND,
    canvas: HWND,
    dpi: u32,
    font: HFONT,
    bold: HFONT,
    mine: Desk,
    peer: Desk,
    peer_name: String,
    peer_known: bool,
    layout: Layout,
    /// 打开时的排布（判断用户改没改）
    layout0: Layout,
    /// 打开时的配置（保存时若重新读取失败就用它）
    cfg: Config,
    /// 本机 IPv4（保存时检查对方 IP 不是本机）
    ips: Vec<String>,
    drag: Option<Drag>,
    hover: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// 短暂借用状态；重入（借用中又收到消息）时返回 None。不要在闭包里调用会重入窗口过程的 API。
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| {
        let mut g = a.try_borrow_mut().ok()?;
        g.as_mut().map(f)
    })
}

impl App {
    /// 对方外接矩形缩放后的宽高（本机像素）。
    fn peer_wh(&self) -> (f64, f64) {
        let pb = self.peer.bounds();
        let s = sanitize_scale(self.layout.scale);
        (pb.w() as f64 * s, pb.h() as f64 * s)
    }

    /// 对方外接矩形左上角（本机像素）：拖动中用拖动位置。
    fn peer_pos(&self) -> (f64, f64) {
        match &self.drag {
            Some(d) => d.pos,
            None => {
                let (l, t, _, _) = self.layout.peer_rect_in_mine(&self.mine, &self.peer);
                (l, t)
            }
        }
    }

    /// 拖动中：松手后将得到的排布；否则当前排布。
    fn projected(&self) -> Layout {
        match &self.drag {
            Some(d) => {
                let (pw, ph) = self.peer_wh();
                let (side, offset) = snap(self.mine.bounds(), pw, ph, d.pos.0, d.pos.1);
                Layout { side, offset, scale: self.layout.scale }
            }
            None => self.layout,
        }
    }

    fn px(&self, v: i32) -> i32 {
        scale_px(v, self.dpi)
    }

    /// 把本机和对方整体缩放到画布内居中。
    fn fit_view(&self, cw: i32, ch: i32) -> View {
        let me = self.mine.bounds();
        let (pl, pt) = self.peer_pos();
        let (pw, ph) = self.peer_wh();
        let ul = (me.l as f64).min(pl);
        let ut = (me.t as f64).min(pt);
        let ur = (me.r as f64).max(pl + pw);
        let ub = (me.b as f64).max(pt + ph);
        let m = self.px(16) as f64;
        let top = if self.peer_known { m } else { self.px(32) as f64 }; // 留出提示文字
        let aw = (cw as f64 - 2.0 * m).max(1.0);
        let ah = (ch as f64 - top - m).max(1.0);
        let (uw, uh) = ((ur - ul).max(1.0), (ub - ut).max(1.0));
        let k = (aw / uw).min(ah / uh);
        View { k, ox: m + (aw - uw * k) / 2.0 - ul * k, oy: top + (ah - uh * k) / 2.0 - ut * k }
    }

    /// 画布坐标 (mx, my) 是否落在对方矩形组上。
    fn hit_peer(&self, v: &View, mx: i32, my: i32) -> bool {
        let x = (mx as f64 - v.ox) / v.k;
        let y = (my as f64 - v.oy) / v.k;
        let (pl, pt) = self.peer_pos();
        let (pw, ph) = self.peer_wh();
        x >= pl && x < pl + pw && y >= pt && y < pt + ph
    }
}

fn scale_px(v: i32, dpi: u32) -> i32 {
    ((v as i64 * dpi as i64 + 48) / 96) as i32
}

fn make_font(dpi: u32, bold: bool) -> HFONT {
    let h = -scale_px(if bold { 14 } else { 13 }, dpi);
    let weight = if bold { FW_BOLD.0 } else { FW_NORMAL.0 } as i32;
    unsafe {
        CreateFontW(
            h,
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
            w!("Microsoft YaHei UI"),
        )
    }
}

fn dlg(main: HWND, id: i32) -> Option<HWND> {
    unsafe { GetDlgItem(Some(main), id).ok() }
}

fn set_text(h: HWND, s: &str) {
    let w = sys::wide(s);
    unsafe {
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

fn get_text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h).max(0) as usize;
        let mut buf = vec![0u16; n + 1];
        let got = GetWindowTextW(h, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..got.min(n)])
    }
}

fn wide_nz(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// 按 DPI 摆放所有控件。
fn layout_controls(main: HWND, dpi: u32) {
    for c in CTLS {
        if let Some(h) = dlg(main, c.id) {
            let (x, y) = (scale_px(c.x, dpi), scale_px(c.y, dpi));
            let (w, h2) = (scale_px(c.x + c.w, dpi) - x, scale_px(c.y + c.h, dpi) - y);
            unsafe {
                let _ = MoveWindow(h, x, y, w, h2, true);
            }
        }
    }
}

fn apply_fonts(main: HWND, font: HFONT, bold: HFONT) {
    for c in CTLS {
        if c.kind == Kind::Canvas {
            continue;
        }
        if let Some(h) = dlg(main, c.id) {
            let f = if c.kind == Kind::Section { bold } else { font };
            unsafe {
                SendMessageW(h, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
            }
        }
    }
}

/// 把状态同步到文字控件并重画画布。不能在借用 APP 时调用。
fn refresh() {
    let Some((main, canvas, status, scale)) =
        with_app(|a| (a.main, a.canvas, status_text(&a.projected()), format!("{:.2}", sanitize_scale(a.layout.scale))))
    else {
        return;
    };
    if let Some(h) = dlg(main, ID_STATUS) {
        set_text(h, &status);
    }
    if let Some(h) = dlg(main, ID_SCALE_VAL) {
        set_text(h, &scale);
    }
    unsafe {
        let _ = InvalidateRect(Some(canvas), None, false);
    }
}

/// 已有设置窗口：提到前面，返回 true（本进程该退出）。已有的那个以管理员身份运行时也算。
fn already_running() -> bool {
    if !sys::already_running(w!("Local\\kmlink-settings"), false) {
        return false;
    }
    unsafe {
        if let Ok(h) = FindWindowW(MAIN_CLASS, PCWSTR::null()) {
            let _ = ShowWindow(h, if IsIconic(h).as_bool() { SW_RESTORE } else { SW_SHOW });
            let _ = SetForegroundWindow(h);
        }
        true
    }
}

pub fn run() {
    if already_running() {
        return;
    }
    // 配置：缺失就新建；解析失败由 load_or_create 备份并保留密钥，不会悄悄换钥
    let (cfg, _) = Config::load_or_create();
    let mut mine = sys::monitors();
    if mine.is_empty() {
        mine = Desk::new(vec![Rect::new(0, 0, 1920, 1080)]);
    }
    let (peer_name, peer, peer_known) = match config::load_peer_desk() {
        Some((n, d)) if !d.is_empty() => (n, d, true),
        _ => (String::new(), Desk::new(vec![Rect::new(0, 0, 1920, 1080)]), false),
    };
    let mut layout = cfg.layout.layout;
    layout.scale = sanitize_scale(layout.scale);
    let ips = local_ipv4();

    unsafe {
        let hinst: HINSTANCE = match GetModuleHandleW(None) {
            Ok(m) => m.into(),
            Err(e) => {
                crate::log!("设置窗口：GetModuleHandleW 失败 {e}");
                return;
            }
        };
        let arrow = LoadCursorW(None, IDC_ARROW).ok();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(main_proc),
            hInstance: hinst,
            hCursor: arrow.unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut _),
            lpszClassName: MAIN_CLASS,
            ..Default::default()
        };
        let wc2 = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(canvas_proc),
            hInstance: hinst,
            hCursor: arrow.unwrap_or_default(),
            lpszClassName: CANVAS_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 || RegisterClassExW(&wc2) == 0 {
            crate::log!("设置窗口：注册窗口类失败");
            return;
        }

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let title = sys::wide(TITLE);
        let main = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            MAIN_CLASS,
            PCWSTR(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CLIENT_W,
            CLIENT_H,
            None,
            None,
            Some(hinst),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                crate::log!("设置窗口：创建失败 {e}");
                return;
            }
        };
        let dpi = match GetDpiForWindow(main) {
            0 => 96,
            d => d,
        };

        // 子控件
        let mut canvas = HWND::default();
        for c in CTLS {
            let (class, st, ex): (PCWSTR, u32, u32) = match c.kind {
                Kind::Section | Kind::Label => (w!("STATIC"), SS_CENTERIMAGE | SS_NOPREFIX | SS_ENDELLIPSIS, 0),
                Kind::Hint => (w!("STATIC"), SS_CENTERIMAGE | SS_NOPREFIX | SS_ENDELLIPSIS, 0),
                Kind::Value => (w!("STATIC"), SS_CENTER | SS_CENTERIMAGE | SS_NOPREFIX, 0),
                Kind::Edit => (w!("EDIT"), WS_TABSTOP.0 | ES_AUTOHSCROLL, WS_EX_CLIENTEDGE.0),
                Kind::RoEdit => (w!("EDIT"), ES_AUTOHSCROLL | ES_READONLY, 0),
                Kind::Button => (w!("BUTTON"), WS_TABSTOP.0 | BS_PUSHBUTTON, 0),
                Kind::DefButton => (w!("BUTTON"), WS_TABSTOP.0 | BS_DEFPUSHBUTTON, 0),
                Kind::Canvas => (CANVAS_CLASS, 0, 0),
            };
            let text = sys::wide(c.text);
            match CreateWindowExW(
                WINDOW_EX_STYLE(ex),
                class,
                PCWSTR(text.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(st),
                0,
                0,
                0,
                0,
                Some(main),
                Some(HMENU(c.id as isize as *mut _)),
                Some(hinst),
                None,
            ) {
                Ok(h) => {
                    if c.kind == Kind::Canvas {
                        canvas = h;
                    }
                }
                Err(e) => crate::log!("设置窗口：控件 {} 创建失败 {e}", c.id),
            }
        }

        let font = make_font(dpi, false);
        let bold = make_font(dpi, true);
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                main,
                canvas,
                dpi,
                font,
                bold,
                mine,
                peer,
                peer_name,
                peer_known,
                layout,
                layout0: layout,
                cfg: cfg.clone(),
                ips: ips.clone(),
                drag: None,
                hover: false,
            })
        });

        // 窗口大小：客户区按 DPI 缩放，居中到所在显示器的工作区
        let mut rc = RECT { left: 0, top: 0, right: scale_px(CLIENT_W, dpi), bottom: scale_px(CLIENT_H, dpi) };
        let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), dpi);
        let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let (mut x, mut y) = (100, 100);
        if GetMonitorInfoW(MonitorFromWindow(main, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            let wa = mi.rcWork;
            x = wa.left + ((wa.right - wa.left - ww) / 2).max(0);
            y = wa.top + ((wa.bottom - wa.top - wh) / 2).max(0);
        }
        let _ = SetWindowPos(main, None, x, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
        layout_controls(main, dpi);
        apply_fonts(main, font, bold);

        // 初始内容
        if let Some(h) = dlg(main, ID_PEER) {
            SendMessageW(h, EM_SETLIMITTEXT, Some(WPARAM(200)), None);
            set_text(h, &cfg.peer);
        }
        if let Some(h) = dlg(main, ID_KEY) {
            SendMessageW(h, EM_SETLIMITTEXT, Some(WPARAM(128)), None);
            set_text(h, &cfg.key);
        }
        if let Some(h) = dlg(main, ID_ADDR) {
            set_text(h, &if ips.is_empty() { "无".to_string() } else { ips.join("  ") });
        }
        refresh();

        let _ = ShowWindow(main, SW_SHOW);
        if let Some(h) = dlg(main, ID_PEER) {
            let _ = SetFocus(Some(h));
        }

        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            if !IsDialogMessageW(main, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        if let Some(app) = APP.with(|a| a.borrow_mut().take()) {
            let _ = DeleteObject(app.font.into());
            let _ = DeleteObject(app.bold.into());
        }
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = (wp.0 & 0xFFFF) as i32;
                let code = ((wp.0 >> 16) & 0xFFFF) as u32;
                if code != BN_CLICKED {
                    return DefWindowProcW(hwnd, msg, wp, lp);
                }
                match id {
                    ID_SAVE => on_save(hwnd),
                    ID_CANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    ID_MINUS => on_scale(-1),
                    ID_PLUS => on_scale(1),
                    ID_COPY => {
                        if let Some(h) = dlg(hwnd, ID_KEY) {
                            let key: String = get_text(h).chars().filter(|c| !c.is_whitespace()).collect();
                            if !copy_text(hwnd, &key) {
                                crate::log!("设置窗口：复制到剪贴板失败");
                                warn(hwnd, "复制失败，请重试");
                            }
                        }
                    }
                    ID_NEWKEY => {
                        if let Some(h) = dlg(hwnd, ID_KEY) {
                            set_text(h, &config::new_key_hex());
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC => {
                let hdc = HDC(wp.0 as *mut _);
                let id = GetDlgCtrlID(HWND(lp.0 as *mut _));
                let fg = if id == ID_PEER_HINT || id == ID_KEY_HINT { C_HINT } else { COLORREF(GetSysColor(COLOR_WINDOWTEXT)) };
                SetTextColor(hdc, fg);
                SetBkColor(hdc, COLORREF(GetSysColor(COLOR_WINDOW)));
                LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize)
            }
            WM_DPICHANGED => {
                let dpi = (wp.0 & 0xFFFF) as u32;
                let font = make_font(dpi, false);
                let bold = make_font(dpi, true);
                let old = with_app(|a| {
                    a.dpi = dpi;
                    (std::mem::replace(&mut a.font, font), std::mem::replace(&mut a.bold, bold))
                });
                let r = lp.0 as *const RECT;
                if !r.is_null() {
                    let r = *r;
                    let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                }
                layout_controls(hwnd, dpi);
                match old {
                    Some((f, b)) => {
                        // 先换上新字体，再删旧的
                        apply_fonts(hwnd, font, bold);
                        let _ = DeleteObject(f.into());
                        let _ = DeleteObject(b.into());
                    }
                    // 没存进状态（极少见）：控件继续用旧字体，新的删掉
                    None => {
                        let _ = DeleteObject(font.into());
                        let _ = DeleteObject(bold.into());
                    }
                }
                refresh();
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn on_scale(steps: i32) {
    with_app(|a| {
        a.layout.scale = step_scale(a.layout.scale, steps);
        let (pw, ph) = a.peer_wh();
        a.layout.offset = clamp_offset(a.mine.bounds(), a.layout.side, pw, ph, a.layout.offset);
    });
    refresh();
}

fn warn(main: HWND, text: &str) {
    let t = sys::wide(text);
    let cap = sys::wide(TITLE);
    unsafe {
        MessageBoxW(Some(main), PCWSTR(t.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | MB_ICONWARNING);
    }
}

fn on_save(main: HWND) {
    let peer = dlg(main, ID_PEER).map(get_text).unwrap_or_default();
    let key: String = dlg(main, ID_KEY).map(get_text).unwrap_or_default().chars().filter(|c| !c.is_whitespace()).collect();
    if config::parse_key(&key).is_none() {
        warn(main, "配对密钥应为 64 位十六进制");
        if let Some(h) = dlg(main, ID_KEY) {
            unsafe {
                let _ = SetFocus(Some(h));
            }
        }
        return;
    }
    let Some((layout, layout0, opened, ips)) = with_app(|a| (a.layout, a.layout0, a.cfg.clone(), a.ips.clone())) else {
        return;
    };
    let bad_peer = if peer_ip(&peer).is_err() {
        Some("对方地址格式不对，例：192.168.1.8")
    } else if is_own_addr(&peer, &ips) {
        Some("对方 IP 是本机地址，请填另一台的 IP")
    } else {
        None
    };
    if let Some(text) = bad_peer {
        warn(main, text);
        if let Some(h) = dlg(main, ID_PEER) {
            unsafe {
                let _ = SetFocus(Some(h));
            }
        }
        return;
    }
    // 加锁读-改-写：只改自己负责的字段，主程序期间写入的（含对方同步来的排布）都保留
    match config::update(&opened, |c| apply_save(c, &peer, key, &layout0, &layout)) {
        Ok(_) => unsafe {
            let _ = DestroyWindow(main);
        },
        Err(e) => {
            crate::log!("设置窗口：保存失败 {e}");
            warn(main, "保存失败");
        }
    }
}

unsafe extern "system" fn canvas_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let mx = (lp.0 & 0xFFFF) as u16 as i16 as i32;
        let my = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint_canvas(hwnd);
                LRESULT(0)
            }
            WM_SETCURSOR if (lp.0 as usize & 0xFFFF) == HTCLIENT => {
                let busy = with_app(|a| a.hover || a.drag.is_some()).unwrap_or(false);
                let _ = SetCursor(LoadCursorW(None, if busy { IDC_SIZEALL } else { IDC_ARROW }).ok());
                LRESULT(1)
            }
            WM_LBUTTONDOWN => {
                let cs = client_size(hwnd);
                let started = with_app(|a| {
                    if a.drag.is_some() {
                        return false;
                    }
                    let v = a.fit_view(cs.0, cs.1);
                    if !a.hit_peer(&v, mx, my) {
                        return false;
                    }
                    let p = a.peer_pos();
                    a.drag = Some(Drag { m0: (mx, my), p0: p, pos: p, view: v });
                    true
                })
                .unwrap_or(false);
                if started {
                    SetCapture(hwnd);
                    let _ = SetCursor(LoadCursorW(None, IDC_SIZEALL).ok());
                    refresh();
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let cs = client_size(hwnd);
                let r = with_app(|a| {
                    if let Some(d) = a.drag.as_mut() {
                        d.pos = (d.p0.0 + (mx - d.m0.0) as f64 / d.view.k, d.p0.1 + (my - d.m0.1) as f64 / d.view.k);
                        (true, true)
                    } else {
                        let v = a.fit_view(cs.0, cs.1);
                        a.hover = a.hit_peer(&v, mx, my);
                        (false, a.hover)
                    }
                });
                if let Some((dragging, over)) = r {
                    let _ = SetCursor(LoadCursorW(None, if over { IDC_SIZEALL } else { IDC_ARROW }).ok());
                    if dragging {
                        refresh();
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let done = with_app(|a| match a.drag.take() {
                    Some(d) => {
                        let (pw, ph) = a.peer_wh();
                        let (side, offset) = snap(a.mine.bounds(), pw, ph, d.pos.0, d.pos.1);
                        a.layout.side = side;
                        a.layout.offset = offset;
                        true
                    }
                    None => false,
                })
                .unwrap_or(false);
                if done {
                    // 先清掉拖动状态再放开捕获（WM_CAPTURECHANGED 会同步进来）
                    let _ = ReleaseCapture();
                    refresh();
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                // 捕获被意外拿走：放弃这次拖动
                if with_app(|a| a.drag.take().is_some()).unwrap_or(false) {
                    refresh();
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    (rc.right - rc.left, rc.bottom - rc.top)
}

/// 双缓冲绘制画布。
fn paint_canvas(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let (w, h) = client_size(hwnd);
        if w > 0 && h > 0 && !hdc.is_invalid() {
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, w, h);
            if !mem.is_invalid() && !bmp.is_invalid() {
                let old = SelectObject(mem, bmp.into());
                let rc = RECT { left: 0, top: 0, right: w, bottom: h };
                let old_brush = SelectObject(mem, GetStockObject(DC_BRUSH));
                let old_pen = SelectObject(mem, GetStockObject(DC_PEN));
                SetDCBrushColor(mem, C_BG);
                FillRect(mem, &rc, HBRUSH(GetStockObject(DC_BRUSH).0));
                APP.with(|a| {
                    if let Ok(g) = a.try_borrow()
                        && let Some(app) = g.as_ref()
                    {
                        draw(mem, app, w, h);
                    }
                });
                // 外框
                SelectObject(mem, GetStockObject(NULL_BRUSH));
                SelectObject(mem, GetStockObject(DC_PEN));
                SetDCPenColor(mem, C_FRAME);
                let _ = Rectangle(mem, 0, 0, w, h);
                let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
                // 全部换回原来的对象，再删位图和 DC
                SelectObject(mem, old_pen);
                SelectObject(mem, old_brush);
                SelectObject(mem, old);
            }
            if !bmp.is_invalid() {
                let _ = DeleteObject(bmp.into());
            }
            if !mem.is_invalid() {
                let _ = DeleteDC(mem);
            }
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 一块显示器的画布矩形，内缩 1 像素让相邻显示器之间有缝。
fn box_rect(v: &View, l: f64, t: f64, r: f64, b: f64) -> RECT {
    let x0 = (v.ox + l * v.k).round() as i32;
    let y0 = (v.oy + t * v.k).round() as i32;
    let x1 = (v.ox + r * v.k).round() as i32;
    let y1 = (v.oy + b * v.k).round() as i32;
    if x1 - x0 > 4 && y1 - y0 > 4 { RECT { left: x0 + 1, top: y0 + 1, right: x1 - 1, bottom: y1 - 1 } } else { RECT { left: x0, top: y0, right: x1.max(x0 + 1), bottom: y1.max(y0 + 1) } }
}

unsafe fn draw_box(hdc: HDC, r: &RECT, fill: COLORREF, edge: COLORREF) {
    unsafe {
        SelectObject(hdc, GetStockObject(DC_BRUSH));
        SelectObject(hdc, GetStockObject(DC_PEN));
        SetDCBrushColor(hdc, fill);
        SetDCPenColor(hdc, edge);
        let _ = Rectangle(hdc, r.left, r.top, r.right, r.bottom);
    }
}

unsafe fn draw_label(hdc: HDC, r: &RECT, text: &str, color: COLORREF) {
    let mut t = wide_nz(text);
    let mut rc = *r;
    unsafe {
        SetTextColor(hdc, color);
        DrawTextW(hdc, &mut t, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
    }
}

fn largest(rs: &[RECT]) -> Option<&RECT> {
    rs.iter().max_by_key(|r| (r.right - r.left) as i64 * (r.bottom - r.top) as i64)
}

fn draw(hdc: HDC, a: &App, w: i32, h: i32) {
    let v = match &a.drag {
        Some(d) => d.view,
        None => a.fit_view(w, h),
    };
    unsafe {
        let old_font = SelectObject(hdc, a.font.into());
        SetBkMode(hdc, TRANSPARENT);

        // 本机
        let mine: Vec<RECT> = a.mine.monitors.iter().map(|m| box_rect(&v, m.l as f64, m.t as f64, m.r as f64, m.b as f64)).collect();
        for r in &mine {
            draw_box(hdc, r, C_MINE_FILL, C_MINE_EDGE);
        }
        if let Some(r) = largest(&mine) {
            draw_label(hdc, r, "本机", C_TEXT);
        }

        // 对方：各显示器相对对方外接矩形的位置 × scale，放到 peer_pos
        let pb = a.peer.bounds();
        let s = sanitize_scale(a.layout.scale);
        let (pl, pt) = a.peer_pos();
        let edge = if a.drag.is_some() { C_PEER_DRAG } else { C_PEER_EDGE };
        let peer: Vec<RECT> = a
            .peer
            .monitors
            .iter()
            .map(|m| {
                let l = pl + (m.l - pb.l) as f64 * s;
                let t = pt + (m.t - pb.t) as f64 * s;
                box_rect(&v, l, t, l + m.w() as f64 * s, t + m.h() as f64 * s)
            })
            .collect();
        for r in &peer {
            draw_box(hdc, r, C_PEER_FILL, edge);
        }
        if let Some(r) = largest(&peer) {
            let name = if a.peer_name.trim().is_empty() { "对方" } else { a.peer_name.trim() };
            draw_label(hdc, r, name, C_TEXT);
        }

        // 拖动中：虚线框标出松手后的吸附位置
        if a.drag.is_some() {
            let (l, t, r, b) = a.projected().peer_rect_in_mine(&a.mine, &a.peer);
            let g = box_rect(&v, l, t, r, b);
            let pen = CreatePen(PS_DOT, 1, C_GHOST);
            if !pen.is_invalid() {
                let old = SelectObject(hdc, pen.into());
                SelectObject(hdc, GetStockObject(NULL_BRUSH));
                let _ = Rectangle(hdc, g.left - 1, g.top - 1, g.right + 1, g.bottom + 1);
                SelectObject(hdc, old);
                let _ = DeleteObject(pen.into());
            }
        }

        if !a.peer_known {
            let m = a.px(10);
            let mut rc = RECT { left: m, top: a.px(6), right: w - m, bottom: a.px(28) };
            let mut t = wide_nz("对方未连接，按默认尺寸");
            SetTextColor(hdc, C_HINT);
            DrawTextW(hdc, &mut t, &mut rc, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
        }
        SelectObject(hdc, old_font);
    }
}

/// 复制文字到剪贴板（密钥不进 Win+V 历史、不上云剪贴板）。先分配好内存再打开剪贴板；
/// 剪贴板被别的程序占着时短暂重试（共约 0.5 秒）。
fn copy_text(owner: HWND, s: &str) -> bool {
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let Some(text) = global_from(&w) else { return false };
        let history = global_from(&[0u16, 0]);
        let cloud = global_from(&[0u16, 0]);
        let mut opened = false;
        for i in 0..25 {
            if OpenClipboard(Some(owner)).is_ok() {
                opened = true;
                break;
            }
            if i < 24 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let mut ok = false;
        let mut left = [Some(text), history, cloud];
        if opened {
            if EmptyClipboard().is_ok() && SetClipboardData(CF_UNICODETEXT, Some(HANDLE(text.0))).is_ok() {
                ok = true; // 成功后内存归系统
                left[0] = None;
                for (i, name) in [(1, w!("CanIncludeInClipboardHistory")), (2, w!("CanUploadToCloudClipboard"))] {
                    let f = RegisterClipboardFormatW(name);
                    if let Some(h) = left[i]
                        && f != 0
                        && SetClipboardData(f, Some(HANDLE(h.0))).is_ok()
                    {
                        left[i] = None;
                    }
                }
            }
            let _ = CloseClipboard();
        }
        // 没交给剪贴板的自己释放
        for h in left.into_iter().flatten() {
            let _ = GlobalFree(Some(h));
        }
        ok
    }
}

/// GlobalAlloc 一块内存并拷入 data（u16 序列）。
unsafe fn global_from(data: &[u16]) -> Option<HGLOBAL> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2).ok()?;
        let p = GlobalLock(h) as *mut u16;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        let _ = GlobalUnlock(h);
        Some(h)
    }
}

/// 本机 IPv4：已连接、非回环网卡上的单播地址（跳过 169.254 自动地址）。
fn local_ipv4() -> Vec<String> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses,
        IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    let flags = GET_ADAPTERS_ADDRESSES_FLAGS(GAA_FLAG_SKIP_ANYCAST.0 | GAA_FLAG_SKIP_MULTICAST.0 | GAA_FLAG_SKIP_DNS_SERVER.0);
    let mut out: Vec<String> = Vec::new();
    let mut size: u32 = 16 * 1024;
    for _ in 0..4 {
        // u64 保证 8 字节对齐
        let mut buf = vec![0u64; (size as usize).div_ceil(8).max(1)];
        let p = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        let r = unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(p), &mut size) };
        if r == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if r != 0 {
            crate::log!("GetAdaptersAddresses 失败：{r}");
            return out;
        }
        let mut a = p as *const IP_ADAPTER_ADDRESSES_LH;
        while !a.is_null() {
            let ad = unsafe { &*a };
            if ad.OperStatus == IfOperStatusUp && ad.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let mut u = ad.FirstUnicastAddress as *const windows::Win32::NetworkManagement::IpHelper::IP_ADAPTER_UNICAST_ADDRESS_LH;
                while !u.is_null() {
                    let ua = unsafe { &*u };
                    let sa = ua.Address.lpSockaddr;
                    if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
                        let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
                        let ip = std::net::Ipv4Addr::from(unsafe { sin.sin_addr.S_un.S_addr }.to_ne_bytes());
                        let s = ip.to_string();
                        if !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() && !out.contains(&s) {
                            out.push(s);
                        }
                    }
                    u = ua.Next;
                }
            }
            a = ad.Next;
        }
        return out;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Point;

    const ME: Rect = Rect::new(0, 0, 1920, 1080);

    #[test]
    fn snap_each_side() {
        assert_eq!(snap(ME, 1920.0, 1080.0, 2000.0, 100.0), (Side::Right, 100));
        assert_eq!(snap(ME, 1920.0, 1080.0, -2000.0, -50.0), (Side::Left, -50));
        assert_eq!(snap(ME, 1920.0, 1080.0, 300.0, -1200.0), (Side::Top, 300));
        assert_eq!(snap(ME, 1920.0, 1080.0, -100.0, 1150.0), (Side::Bottom, -100));
        // 放在本机内部偏右：仍吸到右侧
        assert_eq!(snap(ME, 1920.0, 1080.0, 500.0, 0.0), (Side::Right, 0));
        // 斜角：更靠外的那一侧胜出
        assert_eq!(snap(ME, 1920.0, 1080.0, 2400.0, 900.0).0, Side::Right);
        assert_eq!(snap(ME, 1920.0, 1080.0, 1000.0, 1500.0).0, Side::Bottom);
    }

    #[test]
    fn snap_nonzero_origin() {
        // 本机外接矩形不从 0 开始（副屏在左边）
        let me = Rect::new(-1920, -200, 1920, 1080);
        assert_eq!(snap(me, 1920.0, 1080.0, 2000.0, 0.0), (Side::Right, 200));
        assert_eq!(snap(me, 1920.0, 1080.0, -1500.0, -1500.0), (Side::Top, 420));
    }

    #[test]
    fn snap_clamps_to_shared_edge() {
        assert_eq!(snap(ME, 1920.0, 1080.0, 6000.0, 3000.0), (Side::Right, 1079));
        assert_eq!(snap(ME, 1920.0, 1080.0, 2000.0, -1070.0), (Side::Right, -1070));
        assert_eq!(clamp_offset(ME, Side::Right, 1920.0, 1080.0, -5000), -1079);
        assert_eq!(clamp_offset(ME, Side::Bottom, 1920.0, 1080.0, 9999), 1919);
        assert_eq!(clamp_offset(ME, Side::Top, 1920.0, 1080.0, -9999), -1919);
        // 小数尺寸：o + 1079.5 > 0 → o >= -1079
        assert_eq!(clamp_offset(ME, Side::Left, 1920.0, 1079.5, -5000), -1079);
    }

    #[test]
    fn clamped_layout_still_crosses() {
        let a = Desk::new(vec![ME]);
        let b = Desk::new(vec![Rect::new(0, 0, 1920, 1080)]);
        let hi = Layout { side: Side::Right, offset: clamp_offset(ME, Side::Right, 1920.0, 1080.0, 9999), scale: 1.0 };
        assert!(hi.exit_to_peer(&a, &b, Point::new(1919, 1079)).is_some());
        let lo = Layout { side: Side::Right, offset: clamp_offset(ME, Side::Right, 1920.0, 1080.0, -9999), scale: 1.0 };
        assert!(lo.exit_to_peer(&a, &b, Point::new(1919, 0)).is_some());
    }

    #[test]
    fn snap_with_scale() {
        // 对方 1920x1080，scale 2 → 本机像素里 3840x2160
        assert_eq!(snap(ME, 3840.0, 2160.0, 1950.0, -500.0), (Side::Right, -500));
        assert_eq!(snap(ME, 3840.0, 2160.0, 3000.0, -2300.0), (Side::Right, -2159));
        // scale 0.5 → 960x540，放在左边、往下
        assert_eq!(snap(ME, 960.0, 540.0, -1000.0, 300.0), (Side::Left, 300));
        assert_eq!(snap(ME, 960.0, 540.0, -1500.0, 1300.0), (Side::Left, 1079));
        assert_eq!(snap(ME, 960.0, 540.0, 100.0, -600.0), (Side::Top, 100));
    }

    #[test]
    fn scale_steps() {
        assert_eq!(step_scale(1.0, 1), 1.05);
        assert_eq!(step_scale(1.0, -1), 0.95);
        assert_eq!(step_scale(0.25, -1), 0.25);
        assert_eq!(step_scale(4.0, 1), 4.0);
        assert_eq!(step_scale(1.02, 0), 1.0);
        assert_eq!(step_scale(f64::NAN, 1), 1.05);
        let mut s = 1.0;
        for _ in 0..20 {
            s = step_scale(s, 1);
        }
        assert_eq!(s, 2.0);
    }

    #[test]
    fn save_keeps_layout_when_untouched() {
        let l0 = Layout { side: Side::Right, offset: 0, scale: 1.0 };
        assert!(!layout_changed(&l0, &l0));
        assert!(layout_changed(&l0, &Layout { offset: 1, ..l0 }));
        assert!(layout_changed(&l0, &Layout { side: Side::Left, ..l0 }));
        assert!(layout_changed(&l0, &Layout { scale: 1.05, ..l0 }));

        // 磁盘上已有对方同步来的排布；只改 IP/密钥不能动它和 rev
        let theirs = Layout { side: Side::Left, offset: 200, scale: 1.0 };
        let mut c = Config::default();
        c.layout.layout = theirs;
        c.layout.rev = 123;
        apply_save(&mut c, " 10.0.0.2 ", "ab".repeat(32), &l0, &l0);
        assert_eq!(c.peer, "10.0.0.2");
        assert_eq!(c.key, "ab".repeat(32));
        assert_eq!(c.layout.layout, theirs);
        assert_eq!(c.layout.rev, 123);

        // 改了排布：写入，rev 严格大于磁盘上的（即使磁盘 rev 来自时钟快的对方）
        let mine = Layout { side: Side::Bottom, offset: -10, scale: 1.0 };
        let far = config::now_ms() + 10_000_000;
        c.layout.rev = far;
        apply_save(&mut c, "", "ab".repeat(32), &l0, &mine);
        assert_eq!(c.layout.layout, mine);
        assert_eq!(c.layout.rev, far + 1);
    }

    #[test]
    fn own_addr() {
        let ips = vec!["192.168.1.5".to_string()];
        assert!(is_own_addr("192.168.1.5", &ips));
        assert!(is_own_addr(" 192.168.1.5:47900 ", &ips));
        assert!(is_own_addr("127.0.0.1", &ips));
        assert!(is_own_addr("localhost:1", &ips));
        assert!(!is_own_addr("192.168.1.6", &ips));
        assert!(!is_own_addr("", &ips));
        assert!(!is_own_addr("pc-b", &ips));
        // IPv6 回环 / 未指定 / IPv4 映射
        assert!(is_own_addr("::1", &ips));
        assert!(is_own_addr("[::1]", &ips));
        assert!(is_own_addr("[::1]:47900", &ips));
        assert!(is_own_addr("::", &ips));
        assert!(is_own_addr("0.0.0.0", &ips));
        assert!(is_own_addr("::ffff:127.0.0.1", &ips));
        assert!(is_own_addr("[::ffff:192.168.1.5]:9", &ips));
        assert!(!is_own_addr("fe80::1", &ips));
        assert!(!is_own_addr("[2001:db8::1]:47900", &ips));
    }

    #[test]
    fn peer_addr_format() {
        let ok = |s: &str| peer_ip(s).is_ok();
        assert_eq!(peer_ip(""), Ok(None));
        assert_eq!(peer_ip("   "), Ok(None));
        assert_eq!(peer_ip(" 10.0.0.2 "), Ok(Some("10.0.0.2".parse().unwrap())));
        assert!(ok("10.0.0.2:47900"));
        assert!(ok("10.0.0.2:1"));
        assert!(ok("10.0.0.2:65535"));
        assert!(ok("fe80::1"));
        assert!(ok("[fe80::1]"));
        assert!(ok("[::1]:9"));
        assert!(!ok("10.0.0.2:0"));
        assert!(!ok("10.0.0.2:65536"));
        assert!(!ok("10.0.0.2:"));
        assert!(!ok("10.0.0.2:abc"));
        assert!(!ok("10.0.0"));
        assert!(!ok("10.0.0.256"));
        assert!(!ok("[::1]:0"));
        assert!(ok("::1:9")); // 没有方括号就整段当 IPv6 地址（::1:9），也合法
        assert!(ok("pc-b"));
        assert!(ok("DESKTOP-5KP5N07:47900"));
        assert!(ok("pc-b.local"));
        assert!(!ok("pc-b:0"));
        assert!(!ok("pc b"));
        assert!(!ok("-pc"));
        assert!(ok("localhost:47900")); // 格式合法，由 is_own_addr 拦下
        assert!(!ok("http://10.0.0.2"));
    }

    #[test]
    fn status_texts() {
        assert_eq!(status_text(&Layout { side: Side::Right, offset: 120, scale: 1.0 }), "对方在右侧，下移 120 像素");
        assert_eq!(status_text(&Layout { side: Side::Left, offset: 0, scale: 1.0 }), "对方在左侧，对齐");
        assert_eq!(status_text(&Layout { side: Side::Left, offset: -30, scale: 1.0 }), "对方在左侧，上移 30 像素");
        assert_eq!(status_text(&Layout { side: Side::Top, offset: 5, scale: 1.0 }), "对方在上方，右移 5 像素");
        assert_eq!(status_text(&Layout { side: Side::Bottom, offset: -7, scale: 1.0 }), "对方在下方，左移 7 像素");
    }
}
