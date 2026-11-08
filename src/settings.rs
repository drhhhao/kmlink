//! 设置窗口（单独进程：`kmlink.exe settings`，关掉就退出，不占主程序内存）。全部自绘（src/ui.rs），
//! 设计稿：设计\设置窗口\侧边栏\（方向 A：侧栏图标块点亮 + 页头总开关；说明.md 有坐标和配色）。
//!
//! 窗口客户区 960 x 540（按 DPI 缩放），不可调大小，标题“kmlink”。跟随系统浅色 / 深色
//! （注册表 AppsUseLightTheme；收到 WM_SETTINGCHANGE "ImmersiveColorSet" 重画），标题栏用 DWMWA_CAPTION_COLOR 涂成背景顶色。
//! 改了就生效，没有“保存 / 取消”。
//!
//! 左边玻璃侧栏：顶上连接状态（已连接 / 未连接 / 已暂停 + 对方名字），下面 连接 /（分隔）鼠标、键盘、声音、剪贴板 / 通用。
//! 鼠标、键盘、声音、剪贴板各有一个总开关（在页头右边），互不影响：开着侧栏图标块是绿的，关着变灰、右边写“关”；
//! 关着时那一页的内容变淡但照样能改。鼠标、键盘开关两台同步（config [share]，改了写新 rev，主程序发给对方）；
//! 声音开关 = audio.mode 关 / 上次开着的那档（audio.last）；剪贴板开关 = clipboard.enabled（只管本机收发）。
//! 未连接时页头开关左边一颗“连上后生效”。
//!
//! 1. 连接页：两台设备条（左 1 号、右 2 号，中间一条连线 + 链接图标；没连上时虚点、对方虚线框），
//!    卡片：对方 IP（填一台即可）、配对密钥（两台须相同；只显示首尾，旁边复制 / 换新密钥，换之前弹小确认框）、
//!    本机 IP（只列真实局域网 IPv4：去掉回环、169.254、Clash 的 198.18/15；有网关的优先）+ 复制。
//!    页头右边“暂停 / 继续”（和托盘同一个动作，写 update_cmd.json 的 pause / resume，主程序每秒取走）。
//! 2. 鼠标页：舞台——两台的显示器缩放到舞台区域里居中，每块屏幕是圆角矩形 + “壁纸”渐变（本机偏蓝、对方偏绿）+ 柔和投影；
//!    主显示器左上角一颗玻璃编号圆（config num，定了不随排布变），底部玻璃胶囊：名字（本机带“本机”标签）+ 分辨率 · 缩放。
//!    两台相接的区段画一条绿色细条（鼠标从这里过去；鼠标关着 / 没连上时是虚点，鼠标关着壁纸去色）。
//!    右上“拖动摆放”，底行“机械革命 在本机左侧 · 下移 416 像素”、“尺寸比例 − 1.00 +”（0.25 ~ 4，步进 0.05）。
//!    - 拖对方的屏幕：放大 2.5%、投影加深、94% 不透明，绿色虚线框标出松手后吸附的位置，底行文字实时变；拖动中画布比例不变。
//!      松手后 240ms 滑进虚线框（cubic-bezier(.2,.8,.2,1)），然后写配置（rev = next_rev，排布真变了才写）。
//!    - 改名：悬停名字出现浅底 + 铅笔、手形光标；点一下原地变输入框（绿圈），第二行变“回车保存”，Esc 取消，最多 16 字；
//!      改好后名字旁绿勾 1.2 秒后淡出。本机名写 config.toml 的 name；对方名写 rename_peer.json（主程序约每秒取走，
//!      连着时发给对方）。未连接时对方名字灰色、不能改，悬停提示“连上后可改”。
//!    - 对方屏幕来自 peer.json；从没连上过：名字“对方”、尺寸“—”、1920×1080 灰色虚线占位。
//!    卡片：移动速度滑杆（input.speed，0.5～2.0，步长 0.1；拖的时候只改内存，松手写配置）。
//! 3. 键盘页：设备条 + “跟随鼠标”开关（input.kbd_follow；鼠标关着时键盘只能靠跟随过去，开关显示为开、不能点）。
//! 4. 声音页：设备条（正在放的那台编号实心 + 屏幕中间喇叭；点屏幕 = 选在哪放），卡片：在哪放（1 / 2 / 两台，
//!    1、2 = 标着这个编号的那台放，存成本机视角的 audio.mode here/there/both，改了写新 rev 由主程序同步给对方；
//!    声音关着时点了只记进 audio.last），音量滑杆（audio.volume，0～100%，步长 5%）。
//! 5. 剪贴板页：复制提示开关（clipboard.notify）。
//! 6. 通用页：开机自启（直接改注册表）、暂停热键（只显示）；版本 + 更新卡片：左“kmlink 0.1.2”（对方不支持一键更新时带
//!    “· 对方 0.1.0”），有新版时绿色小标签；右边一个动作：检查更新 / 正在检查… / 已是最新 / 更新两台（对方不支持：
//!    “对方需手动更新”+ 更新本机）/ 正在更新 45% + 进度条 / 正在重启… / 已更新到 x / 更新失败 · 原因 + 重试。
//!    状态来自 status.json 的 upd* 字段；点按钮写 update_cmd.json（"check" / "update"）。主程序没在运行时右边不放东西。
//! 输入框（对方 IP、密钥、名字）按回车或失焦时校验：对就写入；不对则该行提示变红、输入框红圈。输入框用真的 EDIT 控件
//! （光标、选择、中文输入法、粘贴都靠它），平时隐藏，点到哪个框才盖上去。
//! 状态：每 500ms 读 status.json（主程序写；connected 为真、主程序互斥量在且 pid 还活着才算已连接）、peer.json、config.toml（改了就重新读入）。
//!
//! 只允许一个设置窗口（命名互斥量 Local\kmlink-settings），再打开就把已有的窗口提到前面。
//! 所有窗口坐标是物理像素（进程已 PerMonitorV2）；布局常量是 96 DPI 下的逻辑像素，乘 s = dpi/96。
//! 调试截图用：KMLINK_THEME=dark|light 强制主题；KMLINK_PAGE=conn|mouse|kbd|audio|clip|general 打开就在这一页；
//! KMLINK_UI=hover|edit|drag|offhover|popover 直接呈现某个状态（hover/edit/drag/offhover 在鼠标页，popover 在连接页）。

use crate::config::{self, Config};
use crate::geom::{Desk, Layout, Rect, Side};
use crate::sys;
use crate::ui::{self, Font, Painter, Rgba, Seg, Surface, hex, hexa};
use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant, SystemTime};
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, EnumDisplayMonitors, GetMonitorInfoW, HBRUSH,
    HDC, HMONITOR, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, PAINTSTRUCT, SetBkColor, SetTextColor,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL, VK_ESCAPE, VK_RETURN,
    VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, DispatchMessageW, FindWindowW,
    GetClientRect, GetForegroundWindow, GetMessageW, GetWindowTextLengthW, GetWindowTextW, HICON, HMENU, ICONINFO, IDC_ARROW,
    IDC_HAND, IDC_IBEAM, IDC_SIZEALL, IsIconic, KillTimer, LoadCursorW, MSG, MoveWindow, PostQuitMessage, RegisterClassExW, SW_HIDE,
    SW_RESTORE, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetCursor, SetForegroundWindow, SetTimer, SetWindowPos,
    SetWindowTextW, ShowWindow, TranslateMessage, WA_INACTIVE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CAPTURECHANGED,
    WM_CLOSE, WM_COMMAND, WM_CTLCOLOREDIT, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR, WM_SETFONT, WM_SETICON, WM_SETTINGCHANGE, WM_SIZE, WM_TIMER, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN,
    WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
};
use windows::core::{BOOL, PCWSTR, s, w};

// ---------------------------------------------------------------------------
// 纯逻辑（可单测）
// ---------------------------------------------------------------------------

const SCALE_MIN: f64 = 0.25;
const SCALE_MAX: f64 = 4.0;
const SCALE_STEP: f64 = 0.05;
/// 名字最多几个字。
const NAME_MAX: usize = 16;

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

/// 两个排布是否不同。
fn layout_changed(old: &Layout, now: &Layout) -> bool {
    old.side != now.side || old.offset != now.offset || (old.scale - now.scale).abs() > 1e-9
}

/// 写排布（对磁盘上最新的配置）：真变了才写，并递增 rev；没变就不动排布和 rev（不跟对方同步来的打架）。
fn apply_layout(c: &mut Config, now: &Layout) {
    if layout_changed(&c.layout.layout, now) {
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

/// 舞台左下的说明（名字另画）：“在本机右侧 · 下移 120 像素” / “在本机左侧 · 对齐”。
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
    format!("在本机{} · {mv}", l.side.label_zh())
}

/// 名字校验：去掉首尾空白，非空，最多 16 个字。
fn clean_name(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t.chars().count() > NAME_MAX || t.chars().any(char::is_control) { None } else { Some(t.to_string()) }
}

/// 输入框提交时和打开时相比没改：按字段规范化后比较（名字按 clean_name、IP 去首尾空白、密钥去空白转小写）。
fn edit_unchanged(f: Field, text: &str, orig: &str) -> bool {
    let key = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    match f {
        Field::Ip => text.trim() == orig.trim(),
        Field::Key => key(text) == key(orig),
        Field::LocalName | Field::PeerName => match (clean_name(text), clean_name(orig)) {
            (Some(a), Some(b)) => a == b,
            _ => text.trim() == orig.trim(),
        },
    }
}

/// 密钥只显示首尾：“982e0241…2895a8b”。
fn short_key(k: &str) -> String {
    let c: Vec<char> = k.chars().collect();
    if c.len() <= 17 { k.to_string() } else { format!("{}…{}", c[..8].iter().collect::<String>(), c[c.len() - 7..].iter().collect::<String>()) }
}

/// 真实局域网地址：去掉回环、未指定、广播、169.254/16（自动地址）、198.18.0.0/15（Clash fake-ip 的 TUN 网卡）。
fn usable_lan_ip(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_loopback() || ip.is_unspecified() || ip.is_broadcast() || ip.is_link_local() || ip.is_multicast() || (o[0] == 198 && (o[1] & 0xFE) == 18))
}

/// 一块显示器的说明：“3840×2160 · 150%”（不知道 DPI 就只写分辨率）。
fn res_text(r: &Rect, dpi: Option<u32>) -> String {
    match dpi {
        Some(d) if d > 0 => format!("{}×{} · {}%", r.w(), r.h(), (d * 100 + 48) / 96),
        _ => format!("{}×{}", r.w(), r.h()),
    }
}

/// 一块显示器：位置（物理像素）、DPI（不知道为 None）、是否主显示器。
#[derive(Clone, Debug, PartialEq)]
struct Mon {
    r: Rect,
    dpi: Option<u32>,
    primary: bool,
}

/// 保证恰好一块主显示器：没标的话，取左上角在 (0,0) 的，再不行取第一块。
fn fix_primary(ms: &mut [Mon]) {
    if ms.iter().filter(|m| m.primary).count() == 1 {
        return;
    }
    let i = ms.iter().position(|m| m.r.l == 0 && m.r.t == 0).unwrap_or(0);
    for (j, m) in ms.iter_mut().enumerate() {
        m.primary = i == j;
    }
}

/// peer.json：{"name", "monitors": [Rect...], "dpis": [u32...]}（dpis 可缺、可短）。
fn parse_peer(s: &str) -> Option<(String, Vec<Mon>)> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let rects: Vec<Rect> = serde_json::from_value(v.get("monitors")?.clone()).ok()?;
    let dpis: Vec<u32> = v.get("dpis").and_then(|d| serde_json::from_value(d.clone()).ok()).unwrap_or_default();
    let mut ms: Vec<Mon> = rects
        .into_iter()
        .enumerate()
        .filter(|(_, r)| r.w() > 0 && r.h() > 0)
        .map(|(i, r)| Mon { r, dpi: dpis.get(i).copied().filter(|d| *d > 0), primary: false })
        .collect();
    if ms.is_empty() {
        return None;
    }
    fix_primary(&mut ms);
    Some((name, ms))
}

/// status.json：{"pid", "connected", "peer_name", ...} → (pid, connected, peer_name)。
fn parse_status(s: &str) -> Option<(u32, bool, String)> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let pid = v.get("pid")?.as_u64()? as u32;
    let connected = v.get("connected").and_then(|c| c.as_bool()).unwrap_or(false);
    let name = v.get("peer_name").and_then(|n| n.as_str()).unwrap_or("").trim().to_string();
    Some((pid, connected, name))
}

/// 两台相接的区段（本机像素）：返回 (边所在的坐标, [(沿边起, 止)...])。
/// mine：本机各显示器；peer：对方各显示器已放到本机坐标系里的 (l, t, r, b)；pb：对方外接矩形（同坐标系）。
fn contact_segments(mine: &[Rect], peer: &[(f64, f64, f64, f64)], pb: (f64, f64, f64, f64), side: Side) -> (f64, Vec<(f64, f64)>) {
    let me = Desk::new(mine.to_vec()).bounds();
    let near = |a: f64, b: f64| (a - b).abs() < 0.5;
    let (line, mi, pi): (f64, Vec<(f64, f64)>, Vec<(f64, f64)>) = match side {
        Side::Left => (
            me.l as f64,
            mine.iter().filter(|m| m.l == me.l).map(|m| (m.t as f64, m.b as f64)).collect(),
            peer.iter().filter(|p| near(p.2, pb.2)).map(|p| (p.1, p.3)).collect(),
        ),
        Side::Right => (
            me.r as f64,
            mine.iter().filter(|m| m.r == me.r).map(|m| (m.t as f64, m.b as f64)).collect(),
            peer.iter().filter(|p| near(p.0, pb.0)).map(|p| (p.1, p.3)).collect(),
        ),
        Side::Top => (
            me.t as f64,
            mine.iter().filter(|m| m.t == me.t).map(|m| (m.l as f64, m.r as f64)).collect(),
            peer.iter().filter(|p| near(p.3, pb.3)).map(|p| (p.0, p.2)).collect(),
        ),
        Side::Bottom => (
            me.b as f64,
            mine.iter().filter(|m| m.b == me.b).map(|m| (m.l as f64, m.r as f64)).collect(),
            peer.iter().filter(|p| near(p.1, pb.1)).map(|p| (p.0, p.2)).collect(),
        ),
    };
    let mut out = Vec::new();
    for a in &mi {
        for b in &pi {
            let (lo, hi) = (a.0.max(b.0), a.1.min(b.1));
            if hi > lo {
                out.push((lo, hi));
            }
        }
    }
    (line, out)
}

// ---------------------------------------------------------------------------
// 外观
// ---------------------------------------------------------------------------

/// 一块“壁纸”：径向柔光（at gx 15%，半径 120% x 90%，0 → 60% 渐隐）叠在 135° 三段线性渐变上。
#[derive(Clone, Copy)]
struct Wall {
    glow: Rgba,
    gx: f32,
    stops: [Rgba; 3],
    mid: f32,
}

impl Wall {
    /// (u, v)：在屏幕矩形内的位置（逻辑像素），w/h：矩形大小。
    fn at(&self, u: f32, v: f32, w: f32, h: f32) -> Rgba {
        let t = (((u - w / 2.0) + (v - h / 2.0)) / (w + h) + 0.5).clamp(0.0, 1.0);
        let lin = if t < self.mid { self.stops[0].lerp(self.stops[1], t / self.mid) } else { self.stops[1].lerp(self.stops[2], (t - self.mid) / (1.0 - self.mid)) };
        let dx = (u - self.gx * w) / (1.2 * w);
        let dy = (v - 0.15 * h) / (0.9 * h);
        let a = (1.0 - (dx * dx + dy * dy).sqrt() / 0.6).clamp(0.0, 1.0);
        lin.lerp(self.glow, a)
    }
}

/// 配色（照抄 proof.html 的 .light / .dark）。
#[derive(Clone, Copy)]
struct Theme {
    dark: bool,
    bg: Rgba,
    card: Rgba,
    cardline: Rgba,
    card_shadow: f32,
    hair: Rgba,
    fg: Rgba,
    fg2: Rgba,
    mutd: Rgba,
    mut2: Rgba,
    fill: Rgba,
    fillh: Rgba,
    green: Rgba,
    greenring: Rgba,
    swoff: Rgba,
    knob: Rgba,
    stage_top: Rgba,
    glass: Rgba,
    glassline: Rgba,
    glassedge: Rgba,
    scrline: Rgba,
    off: Rgba,
    offline: Rgba,
    tip: Rgba,
    tipfg: Rgba,
    tipline: Rgba,
    tag: Rgba,
    hov: Rgba,
    red: Rgba,
    redring: Rgba,
    /// 分段控件的槽和滑块
    seg_track: Rgba,
    seg_thumb: Rgba,
    wa: Wall,
    wb: Wall,
}

const LIGHT: Theme = Theme {
    dark: false,
    bg: hex(0xF3F3F3),
    card: hex(0xFFFFFF),
    cardline: hexa(0x000000, 0.06),
    card_shadow: 0.05,
    hair: hexa(0x000000, 0.07),
    fg: hex(0x1C1C1E),
    fg2: hex(0x3A3A3C),
    mutd: hex(0x8E8E93),
    mut2: hex(0xAEAEB2),
    fill: hex(0xF2F2F4),
    fillh: hex(0xE9E9EC),
    green: hex(0x34C759),
    greenring: hexa(0x34C759, 0.28),
    swoff: hex(0xE3E3E7),
    knob: hex(0xFFFFFF),
    stage_top: hex(0xEEF1F6),
    glass: hexa(0xFFFFFF, 0.66),
    glassline: hexa(0xFFFFFF, 0.9),
    glassedge: hexa(0x000000, 0.08),
    scrline: hexa(0x000000, 0.10),
    off: hexa(0xFFFFFF, 0.45),
    offline: hexa(0x3C3C43, 0.32),
    tip: hex(0xFFFFFF),
    tipfg: hex(0x1C1C1E),
    tipline: hexa(0x000000, 0.10),
    tag: hexa(0x000000, 0.07),
    hov: hexa(0x7F7F87, 0.16),
    red: hex(0xFF3B30),
    redring: hexa(0xFF3B30, 0.22),
    seg_track: hex(0xECECEF),
    seg_thumb: hex(0xFFFFFF),
    wa: Wall { glow: hex(0xE3EEFF), gx: 0.2, stops: [hex(0xC7DBFF), hex(0xA6C4FB), hex(0xD3CBFB)], mid: 0.48 },
    wb: Wall { glow: hex(0xE9FBEF), gx: 0.8, stops: [hex(0xCDF2DB), hex(0x9FDFBB), hex(0xC9E9F7)], mid: 0.5 },
};

const DARK: Theme = Theme {
    dark: true,
    bg: hex(0x202020),
    card: hex(0x2B2B2B),
    cardline: hexa(0xFFFFFF, 0.06),
    card_shadow: 0.3,
    hair: hexa(0xFFFFFF, 0.07),
    fg: hex(0xF2F2F7),
    fg2: hex(0xD1D1D6),
    mutd: hex(0x98989F),
    mut2: hex(0x636366),
    fill: hex(0x383838),
    fillh: hex(0x424242),
    green: hex(0x30D158),
    greenring: hexa(0x30D158, 0.30),
    swoff: hex(0x48484A),
    knob: hex(0xFFFFFF),
    stage_top: hex(0x1B1E24),
    glass: hexa(0x2C2C30, 0.62),
    glassline: hexa(0xFFFFFF, 0.14),
    glassedge: hexa(0x000000, 0.35),
    scrline: hexa(0xFFFFFF, 0.10),
    off: hexa(0xFFFFFF, 0.04),
    offline: hexa(0xEBEBF5, 0.28),
    tip: hex(0x2C2C2E),
    tipfg: hex(0xF2F2F7),
    tipline: hexa(0xFFFFFF, 0.12),
    tag: hexa(0xFFFFFF, 0.12),
    hov: hexa(0x7F7F87, 0.22),
    red: hex(0xFF453A),
    redring: hexa(0xFF453A, 0.28),
    seg_track: hex(0x383838),
    seg_thumb: hex(0x5C5C60),
    wa: Wall { glow: hex(0x3A4F7A), gx: 0.2, stops: [hex(0x25365A), hex(0x1D2B4A), hex(0x2E2850)], mid: 0.5 },
    wb: Wall { glow: hex(0x2C5A41), gx: 0.8, stops: [hex(0x1F3D2E), hex(0x183528), hex(0x1B3040)], mid: 0.5 },
};

/// 系统是不是深色（KMLINK_THEME=dark|light 可强制，调试截图用）。
fn system_dark() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    if let Ok(v) = std::env::var("KMLINK_THEME") {
        if v.eq_ignore_ascii_case("dark") {
            return true;
        }
        if v.eq_ignore_ascii_case("light") {
            return false;
        }
    }
    let mut v: u32 = 1;
    let mut sz = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut core::ffi::c_void),
            Some(&mut sz),
        )
    };
    r.is_ok() && v == 0
}
// ---------------------------------------------------------------------------
// 布局常量（逻辑像素，客户区坐标；客户区 960 x 540；设计稿 设计\设置窗口\侧边栏\说明.md）
// ---------------------------------------------------------------------------

const CLIENT_W: f32 = 960.0;
const CLIENT_H: f32 = 540.0;

/// 侧栏里的一页。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Conn,
    Mouse,
    Kbd,
    Audio,
    Clip,
    General,
}

const PAGES: [Page; 6] = [Page::Conn, Page::Mouse, Page::Kbd, Page::Audio, Page::Clip, Page::General];

impl Page {
    fn title(self) -> &'static str {
        match self {
            Page::Conn => "连接",
            Page::Mouse => "鼠标",
            Page::Kbd => "键盘",
            Page::Audio => "声音",
            Page::Clip => "剪贴板",
            Page::General => "通用",
        }
    }
    /// 页头标题下的灰字（连接页按连接状态现写）。
    fn desc(self) -> &'static str {
        match self {
            Page::Mouse => "推过屏幕边就过去",
            Page::Kbd => "打字到对方",
            Page::Audio => "两台声音互通",
            Page::Clip => "两边复制粘贴",
            _ => "",
        }
    }
    fn icon(self) -> Icon {
        match self {
            Page::Conn => Icon::Link,
            Page::Mouse => Icon::Mouse,
            Page::Kbd => Icon::Keyboard,
            Page::Audio => Icon::Speaker,
            Page::Clip => Icon::Clipboard,
            Page::General => Icon::Gear,
        }
    }
    /// 有总开关的页（对应 Sw 的下标）。
    fn sw(self) -> Option<Sw> {
        match self {
            Page::Mouse => Some(Sw::Mouse),
            Page::Kbd => Some(Sw::Kbd),
            Page::Audio => Some(Sw::Audio),
            Page::Clip => Some(Sw::Clip),
            _ => None,
        }
    }
    /// 侧栏里这一项的顶。
    fn nav_top(self) -> f32 {
        match self {
            Page::Conn => 72.0,
            Page::Mouse => 130.0,
            Page::Kbd => 174.0,
            Page::Audio => 218.0,
            Page::Clip => 262.0,
            Page::General => 468.0,
        }
    }
    fn from_env(s: &str) -> Option<Page> {
        PAGES.iter().copied().find(|p| format!("{p:?}").eq_ignore_ascii_case(s))
    }
}

/// 开关（下标 = 动画数组的位置）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sw {
    Mouse,
    Kbd,
    Audio,
    Clip,
    Follow,
    Notify,
    Autostart,
}

const SW_N: usize = 7;

/// 侧栏玻璃板
const SIDEBAR: R = R::new(10.0, 10.0, 200.0, 520.0);

fn nav_rect(p: Page) -> R {
    R::new(18.0, p.nav_top(), 182.0, 40.0)
}

/// 页头：图标块、总开关、连接页的暂停按钮（右对齐到 936，宽度量字）
const HEAD_ICON: R = R::new(236.0, 24.0, 40.0, 40.0);
const HEAD_SW: R = R::new(888.0, 32.0, 48.0, 28.0);
/// 内容区左右、卡片里控件的右边
const CX: f32 = 236.0;
const CW: f32 = 700.0;
const CTL_R: f32 = 916.0;
const ROW_H: f32 = 50.0;

// 连接 / 键盘 / 声音页：顶上的两台设备条，下面一张卡片
const STRIP: R = R::new(236.0, 92.0, 700.0, 156.0);
const CARD_Y: f32 = 264.0;
/// 卡片第一行的顶（卡片上内边距 8）
const ROW0: f32 = CARD_Y + 8.0;
const IP_BOX: R = R::new(716.0, ROW0 + 9.0, 200.0, 32.0);
const KEY_BOX: R = R::new(684.0, ROW0 + ROW_H + 9.0, 156.0, 32.0);
const COPY_KEY: R = R::new(846.0, ROW0 + ROW_H + 9.0, 32.0, 32.0);
const NEW_KEY: R = R::new(884.0, ROW0 + ROW_H + 9.0, 32.0, 32.0);
const COPY_IP: R = R::new(888.0, ROW0 + ROW_H * 2.0 + 11.0, 28.0, 28.0);
const POP: R = R::new(676.0, KEY_BOX.y - 8.0 - 80.0, 240.0, 80.0);
const POP_OK: R = R::new(POP.x + 168.0, POP.y + 40.0, 60.0, 28.0);
const POP_CANCEL: R = R::new(POP.x + 100.0, POP.y + 40.0, 60.0, 28.0);

// 鼠标页：舞台 + 移动速度
const STAGE: R = R::new(236.0, 92.0, 700.0, 330.0);
/// 舞台里画屏幕的区域
const AREA: R = R::new(266.0, 130.0, 640.0, 220.0);
/// 两组屏幕之间各让开 3（中间放接缝绿条）
const GAP: f32 = 3.0;
const STEPPER: R = R::new(828.0, 382.0, 94.0, 28.0);
const MINUS: R = R::new(832.0, 385.0, 24.0, 22.0);
const PLUS: R = R::new(894.0, 385.0, 24.0, 22.0);
const SPEED_CARD_Y: f32 = 438.0;
const SPEED_MIN: f64 = 0.5;
const SPEED_MAX: f64 = 2.0;

// 声音页：在哪放（3 段 x 52，槽里留 2）、音量
const AUDIO_SEG: R = R::new(CTL_R - 160.0, ROW0 + 11.0, 160.0, 28.0);
/// 段 k 放第 k+1 档（audio_slot_mode 的 1 / 2 / 3；0 = 关 挪成了页头开关）。
const AUDIO_LABELS: [&str; 3] = ["1", "2", "两台"];
const AUDIO_HINTS: [&str; 3] = ["全在 1 号放", "全在 2 号放", "两台都放"];

// 剪贴板 / 通用页：卡片从内容区顶开始
const TOP_CARD_Y: f32 = 92.0;
/// 通用页第二张卡片（版本 / 更新），一行
const VER_CARD_Y: f32 = 224.0;
const FOOT_CY: f32 = VER_CARD_Y + 8.0 + ROW_H / 2.0;

/// 滑杆：轨道左边、宽（右边留 60 写数值）
const SLIDER_X: f32 = CTL_R - 60.0 - 200.0;
const SLIDER_W: f32 = 200.0;

/// 第 row 行（从 top 起）里滑杆的点击区。
fn slider_hit(top: f32) -> R {
    R::new(SLIDER_X - 12.0, top + 8.0, SLIDER_W + 24.0, ROW_H - 16.0)
}

/// 滑杆上 x 处的比例（0～1）。
fn slider_frac(x: f32) -> f32 {
    ((x - SLIDER_X) / SLIDER_W).clamp(0.0, 1.0)
}

fn speed_frac(s: f64) -> f32 {
    ((s.clamp(SPEED_MIN, SPEED_MAX) - SPEED_MIN) / (SPEED_MAX - SPEED_MIN)) as f32
}

/// 滑杆比例 → 速度（步长 0.1）。
fn speed_at(frac: f32) -> f64 {
    let v = SPEED_MIN + (SPEED_MAX - SPEED_MIN) * frac as f64;
    ((v * 10.0).round() / 10.0).clamp(SPEED_MIN, SPEED_MAX)
}

/// 滑杆比例 → 音量（步长 5%）。
fn volume_at(frac: f32) -> f32 {
    ((frac * 20.0).round() / 20.0).clamp(0.0, 1.0)
}

/// 声音开着时选中的段（关着时 = 上次开着的那档）。
fn audio_seg_index(c: &Config) -> usize {
    let m = if c.audio.mode == config::AudioMode::Off { c.audio.on_mode() } else { c.audio.mode };
    config::audio_mode_slot(m, c.me_one()).max(1) - 1
}

/// 分段控件里 x 落在第几段（两端的 2 像素边算进最近的段）。
fn audio_hit(x: f32) -> usize {
    (((x - AUDIO_SEG.x - 2.0) / 52.0).floor().max(0.0) as usize).min(2)
}

fn audio_seg(k: usize) -> R {
    R::new(AUDIO_SEG.x + 2.0 + 52.0 * k as f32, AUDIO_SEG.y + 2.0, 52.0, AUDIO_SEG.h - 4.0)
}

/// 逻辑像素矩形。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
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
    fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
}

/// 卡片里第 i 行（从 top 起）的行矩形。
fn row_rect(top: f32, i: usize) -> R {
    R::new(CX + 20.0, top + 8.0 + ROW_H * i as f32, CW - 40.0, ROW_H)
}

/// 行里右边的普通开关（40 x 24）。
fn row_switch(row: R) -> R {
    R::new(CTL_R - 40.0, row.y + (ROW_H - 24.0) / 2.0, 40.0, 24.0)
}

/// 连接页的暂停 / 继续按钮（宽度量字后定）。
fn pause_rect(w: f32) -> R {
    R::new(CX + CW - w, 31.0, w, 30.0)
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

/// 鼠标下面是什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hot {
    None,
    Peer,
    NameLocal,
    NamePeer,
    NamePeerOff,
    Minus,
    Plus,
    Ip,
    Key,
    CopyKey,
    NewKey,
    CopyIp,
    Switch(Sw),
    /// 侧栏的一项
    Nav(Page),
    /// 声音分段控件的第几段（0 = “1”）
    Audio(usize),
    /// 滑杆：移动速度 / 音量
    Speed,
    Volume,
    /// 连接页的暂停 / 继续
    Pause,
    PopCancel,
    PopOk,
    Pop,
    /// 底部的更新按钮
    Upd,
}

/// 底部一行的样子（由 status.json 的 upd* 字段得出）。
#[derive(Clone, Debug, PartialEq)]
enum UpdView {
    /// 主程序没在运行
    None,
    Idle,
    Checking,
    Latest,
    Avail(String),
    /// 有新版，但对方连着却不支持一键更新
    AvailSolo(String),
    Down(u8),
    Restarting,
    Failed(String),
    Done(String),
}

/// 可编辑的字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Ip,
    Key,
    LocalName,
    PeerName,
}

impl Field {
    /// 用哪个 EDIT（两个名字共用一个）。
    fn slot(self) -> usize {
        match self {
            Field::Ip => 0,
            Field::Key => 1,
            Field::LocalName | Field::PeerName => 2,
        }
    }
}

/// 舞台坐标 = o + 本机像素 * k（逻辑像素）。
#[derive(Clone, Copy, Debug)]
struct View {
    k: f64,
    ox: f64,
    oy: f64,
}

impl View {
    fn lerp(&self, o: &View, t: f64) -> View {
        View { k: self.k + (o.k - self.k) * t, ox: self.ox + (o.ox - self.ox) * t, oy: self.oy + (o.oy - self.oy) * t }
    }
}

struct Drag {
    /// 按下时鼠标（逻辑像素）
    m0: (f32, f32),
    /// 按下时对方外接矩形左上角（本机像素）
    p0: (f64, f64),
    pos: (f64, f64),
    view: View,
    /// 移动超过 3 像素才算开始拖
    active: bool,
}

/// 松手后滑进吸附位置的动画。
struct DropAnim {
    from: (f64, f64),
    view0: View,
    t0: Instant,
}

const DROP_MS: f32 = 240.0;
const SWITCH_MS: f32 = 180.0;
const FLASH_MS: f32 = 1200.0;
const FADE_MS: f32 = 300.0;

const TIMER_POLL: usize = 1;
const TIMER_ANIM: usize = 2;

struct Fonts {
    name: Font,
    res: Font,
    tag: Font,
    chip: Font,
    small: Font,
    stat: Font,
    stat_b: Font,
    step: Font,
    label: Font,
    hint: Font,
    input: Font,
    mono: Font,
    btn: Font,
    seg_on: Font,
    /// 页头标题、侧栏项（选中加粗）、侧栏顶的连接状态
    title: Font,
    nav: Font,
    nav_b: Font,
    side_b: Font,
    /// 底部一行
    foot: Font,
    foot_b: Font,
    upchip: Font,
    fbtn: Font,
    fbtn_ink: Font,
}

const SEGOE: &[&str] = &["Segoe UI"];

/// 界面字体（中文 Km Rounded + 英文数字 Nunito，ui::Font::ui）。
fn uf(px: f32, weight: i32, s: f32) -> Font {
    Font::ui(px, weight, s, windows::Win32::Graphics::Gdi::CLEARTYPE_QUALITY)
}

impl Fonts {
    fn new(s: f32) -> Fonts {
        Fonts {
            name: uf(13.5, 700, s),
            res: uf(11.5, 400, s),
            tag: uf(10.5, 700, s),
            chip: uf(12.0, 700, s),
            small: uf(12.0, 400, s),
            stat: uf(12.5, 400, s),
            stat_b: uf(12.5, 700, s),
            step: uf(12.5, 600, s),
            label: uf(13.0, 400, s),
            hint: uf(11.5, 400, s),
            input: Font::new(SEGOE, 13.0, 400, s),
            mono: Font::new(&["Cascadia Mono", "Consolas"], 12.0, 400, s),
            btn: uf(12.5, 400, s),
            seg_on: uf(12.5, 700, s),
            title: uf(20.0, 700, s),
            nav: uf(13.5, 400, s),
            nav_b: uf(13.5, 700, s),
            side_b: uf(14.0, 700, s),
            foot: uf(12.5, 400, s),
            foot_b: uf(12.5, 700, s),
            upchip: uf(11.5, 700, s),
            fbtn: uf(12.5, 600, s),
            fbtn_ink: uf(12.5, 700, s),
        }
    }
}

struct App {
    hwnd: HWND,
    s: f32,
    th: Theme,
    fonts: Fonts,
    surf: Option<Surface>,
    /// 量字用的 DC
    mdc: HDC,
    /// 三个 EDIT：对方 IP、密钥、名字
    edits: [HWND; 3],
    /// EDIT 当前摆放（物理像素），None = 隐藏
    edit_pos: [Option<(i32, i32, i32, i32)>; 3],
    brush_fill: HBRUSH,
    brush_card: HBRUSH,
    cfg: Config,
    cfg_mtime: Option<SystemTime>,
    mine: Vec<Mon>,
    peer: Vec<Mon>,
    /// peer.json 里的名字（没有为空）
    peer_file_name: String,
    peer_known: bool,
    connected: bool,
    /// status.json 里的对方名字
    status_name: String,
    /// 刚改的对方名字（等对方确认，最多 10 秒）
    rename_pending: Option<(String, Instant)>,
    layout: Layout,
    drag: Option<Drag>,
    drop: Option<DropAnim>,
    hover: Hot,
    press: Hot,
    tracking: bool,
    edit: Option<Field>,
    /// 名字输入框里当前的字（量宽度用）
    edit_text: String,
    /// 打开输入框时框里的字：提交时没改就什么都不写（免得拿旧值盖掉期间别处改的新值）
    edit_orig: String,
    /// 窗口是否在前台（WM_ACTIVATE）。切走时输入框失焦不算提交，留着草稿，切回来焦点还给它
    active: bool,
    draft_ip: Option<String>,
    draft_key: Option<String>,
    err_ip: Option<&'static str>,
    err_key: Option<&'static str>,
    err_name: bool,
    sw: [bool; SW_N],
    sw_t: [Option<Instant>; SW_N],
    /// 当前页；侧栏选中块的滑动动画（起点的顶 y）
    page: Page,
    page_from: f32,
    page_t: Option<Instant>,
    /// 正在拖的滑杆（Hot::Speed / Hot::Volume）
    slide: Option<Hot>,
    /// 主程序报的“已暂停”
    paused: bool,
    /// 刚点了暂停 / 继续：主程序取走命令前先按点的显示（最多 3 秒）
    pause_click: Option<(bool, Instant)>,
    /// 声音分段选中的段（0 = “1”）、滑块动画（起点位置，以段为单位）
    audio: usize,
    audio_from: f32,
    audio_t: Option<Instant>,
    flash: Option<(Field, Instant)>,
    copied: Option<(Hot, Instant)>,
    popover: bool,
    /// 本机局域网 IPv4（有网关的在前）
    ips: Vec<String>,
    /// 调试截图：悬停状态不随鼠标变
    frozen: bool,
    /// status.json 里的版本 / 更新状态（upd* 等字段）；主程序没在运行时为默认值
    upd: config::StatusFile,
    /// 主程序在运行（能检查更新）
    daemon: bool,
    /// 刚点了“检查更新”：主程序取走命令前先显示“正在检查…”（最多 3 秒）
    upd_click: Option<Instant>,
    /// 转圈动画的起点
    spin0: Instant,
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

fn ms_since(t: Instant) -> f32 {
    t.elapsed().as_secs_f32() * 1000.0
}

impl App {
    fn mine_desk(&self) -> Desk {
        Desk::new(self.mine.iter().map(|m| m.r).collect())
    }

    fn peer_desk(&self) -> Desk {
        Desk::new(self.peer.iter().map(|m| m.r).collect())
    }

    fn local_name(&self) -> String {
        let n = self.cfg.name.trim();
        if n.is_empty() { std::env::var("COMPUTERNAME").unwrap_or_else(|_| "本机".into()) } else { n.to_string() }
    }

    fn peer_name(&self) -> String {
        if let Some((n, _)) = &self.rename_pending {
            return n.clone();
        }
        if self.connected && !self.status_name.is_empty() {
            return self.status_name.clone();
        }
        if self.peer_known && !self.peer_file_name.trim().is_empty() {
            return self.peer_file_name.trim().to_string();
        }
        "对方".to_string()
    }

    /// 对方外接矩形缩放后的宽高（本机像素）。
    fn peer_wh(&self) -> (f64, f64) {
        let pb = self.peer_desk().bounds();
        let s = sanitize_scale(self.layout.scale);
        (pb.w() as f64 * s, pb.h() as f64 * s)
    }

    fn layout_pos(&self, l: &Layout) -> (f64, f64) {
        let (x, y, _, _) = l.peer_rect_in_mine(&self.mine_desk(), &self.peer_desk());
        (x, y)
    }

    /// 拖动中：松手后将得到的排布；否则当前排布。
    fn projected(&self) -> Layout {
        match &self.drag {
            Some(d) if d.active => {
                let (pw, ph) = self.peer_wh();
                let (side, offset) = snap(self.mine_desk().bounds(), pw, ph, d.pos.0, d.pos.1);
                Layout { side, offset, scale: self.layout.scale }
            }
            _ => self.layout,
        }
    }

    /// 把本机和对方（左上角在 pos）整体缩放到舞台区域里居中。
    fn fit_view(&self, pos: (f64, f64)) -> View {
        let me = self.mine_desk().bounds();
        let (pw, ph) = self.peer_wh();
        let ul = (me.l as f64).min(pos.0);
        let ut = (me.t as f64).min(pos.1);
        let ur = (me.r as f64).max(pos.0 + pw);
        let ub = (me.b as f64).max(pos.1 + ph);
        let (aw, ah) = (AREA.w as f64, AREA.h as f64);
        let (uw, uh) = ((ur - ul).max(1.0), (ub - ut).max(1.0));
        let k = (aw / uw).min(ah / uh);
        View { k, ox: AREA.x as f64 + (aw - uw * k) / 2.0 - ul * k, oy: AREA.y as f64 + (ah - uh * k) / 2.0 - ut * k }
    }

    /// 声音滑块当前位置（段，带动画）。
    fn audio_pos(&self) -> f32 {
        let to = self.audio as f32;
        match self.audio_t {
            Some(t0) => {
                let p = (ms_since(t0) / SWITCH_MS).min(1.0);
                let e = ui::cubic_bezier(0.25, 0.1, 0.25, 1.0, p);
                self.audio_from + (to - self.audio_from) * e
            }
            None => to,
        }
    }

    /// 选中声音第 k 段（带滑动动画）。
    fn set_audio(&mut self, k: usize) {
        if k != self.audio {
            self.audio_from = self.audio_pos();
            self.audio = k;
            self.audio_t = Some(Instant::now());
        }
    }

    fn drop_t(&self) -> Option<f32> {
        self.drop.as_ref().map(|d| ui::cubic_bezier(0.2, 0.8, 0.2, 1.0, ms_since(d.t0) / DROP_MS))
    }

    /// 侧栏选中块现在的顶（带滑动动画）。
    fn nav_y(&self) -> f32 {
        let to = self.page.nav_top();
        match self.page_t {
            Some(t0) => {
                let e = ui::cubic_bezier(0.2, 0.8, 0.2, 1.0, (ms_since(t0) / DROP_MS).min(1.0));
                self.page_from + (to - self.page_from) * e
            }
            None => to,
        }
    }

    fn go_page(&mut self, p: Page) {
        if p != self.page {
            self.page_from = self.nav_y();
            self.page = p;
            self.page_t = Some(Instant::now());
            self.popover = false;
        }
    }

    fn is_on(&self, s: Sw) -> bool {
        self.sw[s as usize]
    }

    fn set_sw(&mut self, s: Sw, v: bool) {
        let i = s as usize;
        if self.sw[i] != v {
            self.sw[i] = v;
            self.sw_t[i] = Some(Instant::now());
        }
    }

    /// 配置里各开关的样子（开机自启以注册表为准，见 run）。
    fn sw_from(c: &Config, autostart: bool) -> [bool; SW_N] {
        [
            c.share.mouse,
            c.share.keyboard,
            c.audio.mode != config::AudioMode::Off,
            c.clipboard.enabled,
            c.input.kbd_follow,
            c.clipboard.notify,
            autostart,
        ]
    }

    /// 显示的“已暂停”（刚点过按钮时先按点的显示）。
    fn paused_now(&self) -> bool {
        match self.pause_click {
            Some((v, t)) if t.elapsed() < Duration::from_secs(3) => v,
            _ => self.paused,
        }
    }

    fn switch_t(&self, s: Sw) -> f32 {
        let i = s as usize;
        let target = if self.sw[i] { 1.0 } else { 0.0 };
        match self.sw_t[i] {
            Some(t0) => {
                let p = (ms_since(t0) / SWITCH_MS).min(1.0);
                let e = ui::cubic_bezier(0.25, 0.1, 0.25, 1.0, p);
                if self.sw[i] { e } else { 1.0 - e }
            }
            None => target,
        }
    }

    /// 名字旁绿勾的不透明度。
    fn flash_alpha(&self, f: Field) -> f32 {
        match self.flash {
            Some((g, t0)) if g == f => {
                let ms = ms_since(t0);
                if ms < FLASH_MS { 1.0 } else { (1.0 - (ms - FLASH_MS) / FADE_MS).max(0.0) }
            }
            _ => 0.0,
        }
    }

    fn copied_now(&self, h: Hot) -> bool {
        matches!(self.copied, Some((g, t0)) if g == h && ms_since(t0) < FLASH_MS)
    }

    fn animating(&self) -> bool {
        self.drop.is_some()
            || self.sw_t.iter().flatten().any(|t| ms_since(*t) < SWITCH_MS + 20.0)
            || self.audio_t.is_some_and(|t| ms_since(t) < SWITCH_MS + 20.0)
            || self.page_t.is_some_and(|t| ms_since(t) < DROP_MS + 20.0)
            || self.flash.is_some_and(|(_, t)| ms_since(t) < FLASH_MS + FADE_MS + 20.0)
            || self.copied.is_some_and(|(_, t)| ms_since(t) < FLASH_MS + 20.0)
            || matches!(self.upd_view(), UpdView::Checking | UpdView::Restarting)
    }

    /// 底部一行现在该显示什么。
    fn upd_view(&self) -> UpdView {
        if !self.daemon {
            return UpdView::None;
        }
        let u = &self.upd;
        match u.upd.as_str() {
            "checking" => UpdView::Checking,
            "latest" => UpdView::Latest,
            "avail" if self.connected && !u.peer_upd => UpdView::AvailSolo(u.upd_ver.clone()),
            "avail" => UpdView::Avail(u.upd_ver.clone()),
            "down" => UpdView::Down(u.upd_pct),
            "restart" => UpdView::Restarting,
            "fail" => UpdView::Failed(u.upd_err.clone()),
            "done" => UpdView::Done(u.upd_ver.clone()),
            _ if self.upd_click.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) => UpdView::Checking,
            _ => UpdView::Idle,
        }
    }

    /// 底部右边按钮：(位置, 文字, 黑底)。没有按钮返回 None。
    fn upd_button(&self) -> Option<(R, &'static str, bool)> {
        let (t, ink) = match self.upd_view() {
            UpdView::Idle => ("检查更新", false),
            UpdView::Avail(_) => ("更新两台", true),
            UpdView::AvailSolo(_) => ("更新本机", true),
            UpdView::Failed(_) => ("重试", false),
            _ => return None,
        };
        let f = if ink { &self.fonts.fbtn_ink } else { &self.fonts.fbtn };
        let w = self.measure(f, t) + 28.0;
        Some((R::new(CTL_R - w, FOOT_CY - 15.0, w, 30.0), t, ink))
    }

    /// 暂停 / 继续按钮的宽。
    fn pause_w(&self) -> f32 {
        let t = if self.paused_now() { "继续" } else { "暂停" };
        14.0 + 12.0 + 6.0 + self.measure(&self.fonts.fbtn_ink, t) + 16.0
    }

    fn measure(&self, f: &Font, t: &str) -> f32 {
        ui::text_w(self.mdc, f, self.s, t)
    }
}

// ---------------------------------------------------------------------------
// 场景：每次绘制 / 命中测试都现算（便宜）
// ---------------------------------------------------------------------------

struct Pill {
    r: R,
    primary: bool,
    glass: bool,
    name: String,
    /// 名字文字（悬停时含铅笔）的位置
    name_r: R,
    /// 名字的点击区（总含铅笔位置，悬停时不抖）
    name_hit: R,
    hovered: bool,
    editing: bool,
    tag_r: Option<R>,
    check: Option<(R, f32)>,
    line1_cy: f32,
    line2: String,
    line2_cy: f32,
    dim: bool,
}

struct Scr {
    r: R,
    peer: bool,
    offline: bool,
    lift: f32,
    pill: Pill,
}

struct Scene {
    scr: Vec<Scr>,
    /// 接缝绿条（舞台坐标）
    edges: Vec<R>,
    edge_vertical: bool,
    ghost: Option<R>,
    name_local: Option<R>,
    name_peer: Option<R>,
    /// 名字输入框（编辑中）
    namebox: Option<R>,
    tip: Option<R>,
    layout_now: Layout,
}

/// 显示器矩形上与同组其它显示器相邻的边：(左, 上, 右, 下)。
fn adjacent(m: &Rect, all: &[Rect]) -> [bool; 4] {
    let mut e = [false; 4];
    for o in all {
        if o == m {
            continue;
        }
        let oy = o.t < m.b && o.b > m.t;
        let ox = o.l < m.r && o.r > m.l;
        e[0] |= o.r == m.l && oy;
        e[2] |= o.l == m.r && oy;
        e[1] |= o.b == m.t && ox;
        e[3] |= o.t == m.b && ox;
    }
    e
}

fn pill(a: &App, sr: R, mon: &Mon, peer: bool, offline: bool) -> Pill {
    let f = &a.fonts;
    let field = if peer { Field::PeerName } else { Field::LocalName };
    let editing = a.edit == Some(field);
    let hovered = !editing && ((peer && matches!(a.hover, Hot::NamePeer)) || (!peer && a.hover == Hot::NameLocal));
    let name = if peer { a.peer_name() } else { a.local_name() };
    let line2 = if editing {
        "回车保存".to_string()
    } else if peer && !a.peer_known {
        "—".to_string()
    } else {
        res_text(&mon.r, mon.dpi)
    };
    let glass = !offline;
    if !mon.primary {
        let w2 = a.measure(&f.res, &line2);
        let r = R::new(sr.cx() - (w2 + 22.0) / 2.0, sr.y + sr.h - 10.0 - 25.0, w2 + 22.0, 25.0);
        return Pill {
            r,
            primary: false,
            glass,
            name,
            name_r: R::default(),
            name_hit: R::default(),
            hovered: false,
            editing: false,
            tag_r: None,
            check: None,
            line1_cy: 0.0,
            line2,
            line2_cy: r.cy(),
            dim: offline,
        };
    }
    let line1_h = if editing { 24.0 } else { 19.0 };
    let name_w = if editing { a.measure(&f.name, &a.edit_text).max(24.0) + 3.0 + 16.0 } else { a.measure(&f.name, &name) };
    let pencil = 4.0 + 12.0;
    let mut l1w = name_w + if hovered { pencil } else { 0.0 };
    let tag_w = a.measure(&f.tag, "本机") + 10.0;
    if !peer {
        l1w += 5.0 + tag_w;
    }
    let fa = a.flash_alpha(field);
    if fa > 0.0 {
        l1w += 5.0 + 16.0;
    }
    let l2w = if editing { a.measure(&f.hint, &line2) } else { a.measure(&f.res, &line2) };
    let w = l1w.max(l2w) + 26.0;
    let h = 6.0 + line1_h + 15.0 + 6.0 + 2.0;
    let r = R::new(sr.cx() - w / 2.0, sr.y + sr.h - 10.0 - h, w, h);
    let line1_cy = r.y + 7.0 + line1_h / 2.0;
    let line2_cy = r.y + 7.0 + line1_h + 7.5;
    let mut x = r.cx() - l1w / 2.0;
    let name_r = R::new(x, line1_cy - if editing { 12.0 } else { 9.5 }, name_w + if hovered { pencil } else { 0.0 }, line1_h);
    let name_hit = R::new(x - 5.0, line1_cy - 10.0, name_w + pencil + 10.0, 20.0);
    x += name_r.w;
    let mut tag_r = None;
    if !peer {
        x += 5.0;
        tag_r = Some(R::new(x, line1_cy - 8.0, tag_w, 16.0));
        x += tag_w;
    }
    let check = if fa > 0.0 { Some((R::new(x + 5.0, line1_cy - 8.0, 16.0, 16.0), fa)) } else { None };
    Pill { r, primary: true, glass, name, name_r, name_hit, hovered, editing, tag_r, check, line1_cy, line2, line2_cy, dim: offline }
}

fn scene(a: &App) -> Scene {
    let peer_desk = a.peer_desk();
    let pb = peer_desk.bounds();
    let sc = sanitize_scale(a.layout.scale);
    let lay_pos = a.layout_pos(&a.layout);
    let (view, pos, lift) = match (&a.drag, &a.drop) {
        (Some(d), _) if d.active => (d.view, d.pos, 1.0f32),
        (_, Some(dr)) => {
            let t = a.drop_t().unwrap_or(1.0) as f64;
            let v1 = a.fit_view(lay_pos);
            let pos = (dr.from.0 + (lay_pos.0 - dr.from.0) * t, dr.from.1 + (lay_pos.1 - dr.from.1) * t);
            (dr.view0.lerp(&v1, t), pos, 1.0 - t as f32)
        }
        _ => (a.fit_view(lay_pos), lay_pos, 0.0),
    };
    let dragging = a.drag.as_ref().is_some_and(|d| d.active);
    let lay_now = a.projected();
    let tx = |x: f64| (view.ox + x * view.k) as f32;
    let ty = |y: f64| (view.oy + y * view.k) as f32;
    let (gx, gy) = match lay_now.side {
        Side::Left => (GAP, 0.0),
        Side::Right => (-GAP, 0.0),
        Side::Top => (0.0, GAP),
        Side::Bottom => (0.0, -GAP),
    };
    let mut out = Scene {
        scr: Vec::new(),
        edges: Vec::new(),
        edge_vertical: lay_now.side.horizontal(),
        ghost: None,
        name_local: None,
        name_peer: None,
        namebox: None,
        tip: None,
        layout_now: lay_now,
    };

    // 本机
    let mine_rects: Vec<Rect> = a.mine.iter().map(|m| m.r).collect();
    for m in &a.mine {
        let adj = adjacent(&m.r, &mine_rects);
        let i = |b: bool| if b { 2.0 } else { 0.0 };
        let (x0, y0) = (tx(m.r.l as f64) + gx + i(adj[0]), ty(m.r.t as f64) + gy + i(adj[1]));
        let (x1, y1) = (tx(m.r.r as f64) + gx - i(adj[2]), ty(m.r.b as f64) + gy - i(adj[3]));
        let r = R::new(x0, y0, x1 - x0, y1 - y0);
        let p = pill(a, r, m, false, false);
        if p.primary {
            out.name_local = Some(p.name_hit);
            if p.editing {
                out.namebox = Some(p.name_r);
            }
        }
        out.scr.push(Scr { r, peer: false, offline: false, lift: 0.0, pill: p });
    }

    // 对方
    let offline = !a.connected;
    let peer_rects: Vec<Rect> = a.peer.iter().map(|m| m.r).collect();
    let place = |m: &Rect, p: (f64, f64)| {
        let l = p.0 + (m.l - pb.l) as f64 * sc;
        let t = p.1 + (m.t - pb.t) as f64 * sc;
        (l, t, l + m.w() as f64 * sc, t + m.h() as f64 * sc)
    };
    let (pgx, pgy) = if lift > 0.0 { (0.0, 0.0) } else { (-gx, -gy) };
    for m in &a.peer {
        let adj = adjacent(&m.r, &peer_rects);
        let i = |b: bool| if b { 2.0 } else { 0.0 };
        let (l, t, rr, b) = place(&m.r, pos);
        let (x0, y0) = (tx(l) + pgx + i(adj[0]), ty(t) + pgy + i(adj[1]));
        let (x1, y1) = (tx(rr) + pgx - i(adj[2]), ty(b) + pgy - i(adj[3]));
        let mut r = R::new(x0, y0, x1 - x0, y1 - y0);
        if lift > 0.0 {
            let k = 1.0 + 0.025 * lift;
            r = R::new(r.cx() - r.w * k / 2.0, r.cy() - r.h * k / 2.0, r.w * k, r.h * k);
        }
        let p = pill(a, r, m, true, offline);
        if p.primary {
            out.name_peer = Some(p.name_hit);
            if p.editing {
                out.namebox = Some(p.name_r);
            }
            if a.hover == Hot::NamePeerOff && offline && !dragging {
                let tw = a.measure(&a.fonts.small, "连上后可改") + 20.0;
                out.tip = Some(R::new(p.name_r.cx() - tw / 2.0, p.name_r.y - 8.0 - 28.0, tw, 28.0));
            }
        }
        out.scr.push(Scr { r, peer: true, offline, lift, pill: p });
    }

    // 拖动中：吸附位置的虚线框
    if dragging {
        let g = a.layout_pos(&lay_now);
        let (l, t) = (tx(g.0) - gx, ty(g.1) - gy);
        let (pw, ph) = a.peer_wh();
        out.ghost = Some(R::new(l, t, (pw * view.k) as f32, (ph * view.k) as f32));
    }

    // 接缝：按吸附后的位置算
    let at = a.layout_pos(&lay_now);
    let placed: Vec<(f64, f64, f64, f64)> = a.peer.iter().map(|m| place(&m.r, at)).collect();
    let pbx = (at.0, at.1, at.0 + pb.w() as f64 * sc, at.1 + pb.h() as f64 * sc);
    let (line, segs) = contact_segments(&mine_rects, &placed, pbx, lay_now.side);
    for (lo, hi) in segs {
        let (a0, a1) = if lay_now.side.horizontal() { (ty(lo), ty(hi)) } else { (tx(lo), tx(hi)) };
        let inset = ((a1 - a0) * 0.25).min(6.0);
        let (a0, a1) = (a0 + inset, a1 - inset);
        if a1 - a0 < 2.0 {
            continue;
        }
        out.edges.push(if lay_now.side.horizontal() {
            R::new(tx(line) - 1.5, a0, 3.0, a1 - a0)
        } else {
            R::new(a0, ty(line) - 1.5, a1 - a0, 3.0)
        });
    }
    out
}

/// 设备条（连接 / 键盘 / 声音页）上的两台屏幕：左边 1 号、右边 2 号。(屏幕矩形, 是对方, 编号)。
fn strip_screens(a: &App) -> [(R, bool, u8); 2] {
    let aspect = |ms: &[Mon]| {
        let m = ms.iter().find(|m| m.primary).or(ms.first());
        m.map(|m| m.r.w() as f32 / m.r.h().max(1) as f32).unwrap_or(16.0 / 9.0)
    };
    let h = 96.0;
    let w_of = |ms: &[Mon]| (h * aspect(ms)).clamp(120.0, 190.0);
    let me_one = a.cfg.me_one();
    let (l_peer, r_peer) = if me_one { (false, true) } else { (true, false) };
    let side_w = |peer: bool| if peer { w_of(&a.peer) } else { w_of(&a.mine) };
    let (wl, wr) = (side_w(l_peer), side_w(r_peer));
    let x0 = STRIP.x + (STRIP.w - (wl + 120.0 + wr)) / 2.0;
    let top = STRIP.y + 18.0;
    [(R::new(x0, top, wl, h), l_peer, 1), (R::new(x0 + wl + 120.0, top, wr, h), r_peer, 2)]
}

fn hit(a: &App, sc: &Scene, x: f32, y: f32) -> Hot {
    if a.popover && a.page == Page::Conn {
        if POP_OK.contains(x, y) {
            return Hot::PopOk;
        }
        if POP_CANCEL.contains(x, y) {
            return Hot::PopCancel;
        }
        if POP.contains(x, y) {
            return Hot::Pop;
        }
    }
    if a.drag.is_some() || a.drop.is_some() || a.slide.is_some() {
        return Hot::None;
    }
    if SIDEBAR.contains(x, y) {
        return PAGES.iter().find(|p| nav_rect(**p).contains(x, y)).map_or(Hot::None, |p| Hot::Nav(*p));
    }
    if let Some(s) = a.page.sw()
        && R::new(HEAD_SW.x - 4.0, HEAD_SW.y - 4.0, HEAD_SW.w + 8.0, HEAD_SW.h + 8.0).contains(x, y)
    {
        return Hot::Switch(s);
    }
    match a.page {
        Page::Conn => {
            if a.daemon && pause_rect(a.pause_w()).contains(x, y) {
                return Hot::Pause;
            }
            if IP_BOX.contains(x, y) {
                return Hot::Ip;
            }
            if KEY_BOX.contains(x, y) {
                return Hot::Key;
            }
            if COPY_KEY.contains(x, y) {
                return Hot::CopyKey;
            }
            if NEW_KEY.contains(x, y) {
                return Hot::NewKey;
            }
            if !a.ips.is_empty() && COPY_IP.contains(x, y) {
                return Hot::CopyIp;
            }
        }
        Page::Mouse => {
            if MINUS.contains(x, y) {
                return Hot::Minus;
            }
            if PLUS.contains(x, y) {
                return Hot::Plus;
            }
            if slider_hit(row_rect(SPEED_CARD_Y, 0).y).contains(x, y) {
                return Hot::Speed;
            }
            if !STAGE.contains(x, y) {
                return Hot::None;
            }
            if a.edit != Some(Field::LocalName)
                && let Some(r) = sc.name_local
                && r.contains(x, y)
            {
                return Hot::NameLocal;
            }
            if a.edit != Some(Field::PeerName)
                && let Some(r) = sc.name_peer
                && r.contains(x, y)
            {
                return if a.connected { Hot::NamePeer } else { Hot::NamePeerOff };
            }
            if sc.scr.iter().any(|s| s.peer && s.r.contains(x, y)) {
                return Hot::Peer;
            }
        }
        Page::Kbd => {
            if row_rect(CARD_Y, 0).contains(x, y) && a.cfg.share.mouse {
                return Hot::Switch(Sw::Follow);
            }
        }
        Page::Audio => {
            if AUDIO_SEG.contains(x, y) {
                return Hot::Audio(audio_hit(x));
            }
            if slider_hit(row_rect(CARD_Y, 1).y).contains(x, y) {
                return Hot::Volume;
            }
            for (r, _, n) in strip_screens(a) {
                if r.contains(x, y) {
                    return Hot::Audio(n as usize - 1);
                }
            }
        }
        Page::Clip => {
            if row_rect(TOP_CARD_Y, 0).contains(x, y) {
                return Hot::Switch(Sw::Notify);
            }
        }
        Page::General => {
            if row_rect(TOP_CARD_Y, 0).contains(x, y) {
                return Hot::Switch(Sw::Autostart);
            }
            if a.upd_button().is_some_and(|(r, ..)| r.contains(x, y)) {
                return Hot::Upd;
            }
        }
    }
    Hot::None
}

// ---------------------------------------------------------------------------
// 绘制
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Copy,
    Refresh,
    Pencil,
    Check,
    Minus,
    Plus,
    Link,
    Mouse,
    Keyboard,
    Speaker,
    Clipboard,
    Gear,
    Pause,
    Play,
}

/// 图标（24 格的 viewBox，照设计稿的 SVG），中心 (cx, cy)，边长 size，线宽 sw（viewBox 单位）。
pub(crate) fn icon(p: &mut Painter, which: Icon, cx: f32, cy: f32, size: f32, sw: f32, c: Rgba) {
    use std::f32::consts::PI;
    let k = size / 24.0;
    let (x0, y0) = (cx - size / 2.0, cy - size / 2.0);
    let m = |x: f32, y: f32| (x0 + x * k, y0 + y * k);
    let ln = |a: (f32, f32), b: (f32, f32)| Seg::Line(a.0, a.1, b.0, b.1);
    let w = sw * k;
    match which {
        Icon::Copy => {
            let (rx, ry) = m(9.0, 9.0);
            p.ring(rx, ry, 12.0 * k, 12.0 * k, 2.5 * k, -w / 2.0, w / 2.0, c);
            let pts = [m(5.0, 15.0), m(4.0, 15.0), m(3.0, 14.0), m(3.0, 4.0), m(4.0, 3.0), m(14.0, 3.0), m(15.0, 4.0), m(15.0, 5.0)];
            p.polyline(&pts, w, c);
        }
        Icon::Refresh => {
            let (ccx, ccy) = m(12.0, 12.0);
            let d = |deg: f32| deg * PI / 180.0;
            let segs = [
                Seg::Arc(ccx, ccy, 9.0 * k, d(0.0), d(136.36)),
                ln(m(5.5, 18.2), m(3.0, 16.0)),
                Seg::Arc(ccx, ccy, 9.0 * k, d(180.0), d(316.36)),
                ln(m(18.5, 5.8), m(21.0, 8.0)),
                ln(m(21.0, 3.0), m(21.0, 8.0)),
                ln(m(21.0, 8.0), m(16.0, 8.0)),
                ln(m(3.0, 21.0), m(3.0, 16.0)),
                ln(m(3.0, 16.0), m(8.0, 16.0)),
            ];
            p.stroke(&segs, w, c);
        }
        Icon::Pencil => {
            let (acx, acy) = m(19.0, 5.0);
            let segs = [
                Seg::Arc(acx, acy, 2.83 * k, 225.0 * PI / 180.0, 405.0 * PI / 180.0),
                ln(m(21.0, 7.0), m(7.5, 20.5)),
                ln(m(7.5, 20.5), m(2.0, 22.0)),
                ln(m(2.0, 22.0), m(3.5, 16.5)),
                ln(m(3.5, 16.5), m(17.0, 3.0)),
            ];
            p.stroke(&segs, w, c);
        }
        Icon::Check => p.polyline(&[m(5.0, 12.5), m(9.5, 17.0), m(19.0, 7.5)], w, c),
        Icon::Minus => p.polyline(&[m(5.0, 12.0), m(19.0, 12.0)], w, c),
        Icon::Plus => p.stroke(&[ln(m(5.0, 12.0), m(19.0, 12.0)), ln(m(12.0, 5.0), m(12.0, 19.0))], w, c),
        Icon::Link => {
            let (l, r) = (m(7.0, 12.0), m(17.0, 12.0));
            let segs = [
                Seg::Arc(l.0, l.1, 5.0 * k, PI / 2.0, PI * 1.5),
                ln(m(7.0, 7.0), m(9.5, 7.0)),
                ln(m(7.0, 17.0), m(9.5, 17.0)),
                Seg::Arc(r.0, r.1, 5.0 * k, -PI / 2.0, PI / 2.0),
                ln(m(14.5, 7.0), m(17.0, 7.0)),
                ln(m(14.5, 17.0), m(17.0, 17.0)),
                ln(m(8.5, 12.0), m(15.5, 12.0)),
            ];
            p.stroke(&segs, w, c);
        }
        Icon::Mouse => {
            let (x, y) = m(6.0, 3.0);
            p.ring(x, y, 12.0 * k, 18.0 * k, 6.0 * k, -w / 2.0, w / 2.0, c);
            p.polyline(&[m(12.0, 7.0), m(12.0, 10.5)], w, c);
        }
        Icon::Keyboard => {
            let (x, y) = m(2.5, 5.5);
            p.ring(x, y, 19.0 * k, 13.0 * k, 2.5 * k, -w / 2.0, w / 2.0, c);
            for (dx, dy) in [(6.5, 9.5), (10.0, 9.5), (14.0, 9.5), (17.5, 9.5), (6.5, 12.8), (17.5, 12.8)] {
                let (px, py) = m(dx, dy);
                p.circle(px, py, 1.15 * k, c);
            }
            p.polyline(&[m(9.5, 15.0), m(14.5, 15.0)], w, c);
        }
        Icon::Speaker => {
            p.polyline(&[m(11.0, 5.0), m(6.0, 9.0), m(3.0, 9.0), m(3.0, 15.0), m(6.0, 15.0), m(11.0, 19.0), m(11.0, 5.0)], w, c);
            let (ccx, ccy) = m(12.0, 12.0);
            p.stroke(&[Seg::Arc(ccx, ccy, 4.9 * k, -PI / 4.0, PI / 4.0)], w, c);
            p.stroke(&[Seg::Arc(ccx, ccy, 9.2 * k, -PI / 4.0, PI / 4.0)], w, c);
        }
        Icon::Clipboard => {
            let (x, y) = m(8.0, 2.5);
            p.ring(x, y, 8.0 * k, 4.0 * k, 1.0 * k, -w / 2.0, w / 2.0, c);
            // 外框：顶边中间（夹子处）不画
            let segs = [
                ln(m(8.0, 4.5), m(6.0, 4.5)),
                Seg::Arc(x0 + 6.0 * k, y0 + 6.5 * k, 2.0 * k, PI, -PI / 2.0),
                ln(m(4.0, 6.5), m(4.0, 20.0)),
                Seg::Arc(x0 + 6.0 * k, y0 + 20.0 * k, 2.0 * k, PI / 2.0, PI),
                ln(m(6.0, 22.0), m(18.0, 22.0)),
                Seg::Arc(x0 + 18.0 * k, y0 + 20.0 * k, 2.0 * k, 0.0, PI / 2.0),
                ln(m(20.0, 20.0), m(20.0, 6.5)),
                Seg::Arc(x0 + 18.0 * k, y0 + 6.5 * k, 2.0 * k, -PI / 2.0, 0.0),
                ln(m(18.0, 4.5), m(16.0, 4.5)),
            ];
            p.stroke(&segs, w, c);
        }
        Icon::Gear => {
            let (ccx, ccy) = m(12.0, 12.0);
            for r in [7.0 * k, 2.6 * k] {
                p.ring(ccx - r, ccy - r, 2.0 * r, 2.0 * r, r, -w / 2.0, w / 2.0, c);
            }
            let mut segs = Vec::with_capacity(8);
            for i in 0..8 {
                let t = i as f32 * PI / 4.0;
                let (s, co) = t.sin_cos();
                segs.push(Seg::Line(ccx + co * 7.0 * k, ccy + s * 7.0 * k, ccx + co * 10.0 * k, ccy + s * 10.0 * k));
            }
            p.stroke(&segs, w * 1.1, c);
        }
        Icon::Pause => {
            for x in [6.5, 13.9] {
                let (px, py) = m(x, 5.0);
                p.rrect(px, py, 3.6 * k, 14.0 * k, 1.0 * k, c);
            }
        }
        Icon::Play => {
            p.polyline(&[m(7.0, 5.0), m(19.0, 12.0), m(7.0, 19.0), m(7.0, 5.0)], w, c);
        }
    }
}

/// 玻璃胶囊底：投影 + 半透明白 + 白描边 + 外细边 + 顶部内高光。
fn glass(p: &mut Painter, th: &Theme, r: R, rad: f32) {
    p.shadow(r.x, r.y, r.w, r.h, rad, 0.0, 6.0, 16.0, -6.0, hexa(0, 0.22));
    p.rrect(r.x, r.y, r.w, r.h, rad, th.glass);
    p.ring(r.x, r.y, r.w, r.h, rad, -1.0, 0.0, th.glassline);
    p.ring(r.x, r.y, r.w, r.h, rad, 0.0, 0.5, th.glassedge);
    if !th.dark {
        p.ring(r.x + 1.0, r.y + 1.0, r.w - 2.0, r.h - 2.0, rad - 1.0, -1.0, 0.0, hexa(0xFFFFFF, 0.2));
    }
}

/// 文字水平居中在 cx、竖直居中在 cy。
fn text_c(p: &mut Painter, cx: f32, cy: f32, t: &str, f: &Font, c: Rgba) {
    let w = p.measure(f, t);
    p.text_mid(cx - w / 2.0, cy, t, f, c);
}

/// 文字右对齐到 rx。
fn text_r(p: &mut Painter, rx: f32, cy: f32, t: &str, f: &Font, c: Rgba) {
    let w = p.measure(f, t);
    p.text_mid(rx - w, cy, t, f, c);
}

fn draw_pill(p: &mut Painter, a: &App, pl: &Pill) {
    let th = &a.th;
    let f = &a.fonts;
    let r = pl.r;
    if pl.glass {
        glass(p, th, r, 12.0);
    }
    let dimc = th.mutd;
    if !pl.primary {
        let c = if pl.dim { dimc } else { th.fg2 };
        text_c(p, r.cx(), pl.line2_cy, &pl.line2, &f.res, c);
        return;
    }
    let nr = pl.name_r;
    if pl.editing {
        let red = a.err_name;
        let (ring, halo) = if red { (th.red, th.redring) } else { (th.green, th.greenring) };
        p.ring(nr.x, nr.y, nr.w, nr.h, 7.0, 1.5, 4.5, halo);
        p.ring(nr.x, nr.y, nr.w, nr.h, 7.0, 0.0, 1.5, ring);
        p.rrect(nr.x, nr.y, nr.w, nr.h, 7.0, th.card);
    } else {
        if pl.hovered {
            p.rrect(nr.x - 5.0, nr.y, nr.w + 10.0, nr.h, 6.0, th.hov);
        }
        let c = if pl.dim { dimc } else { th.fg };
        let w = p.text_mid(nr.x, pl.line1_cy, &pl.name, &f.name, c);
        if pl.hovered {
            icon(p, Icon::Pencil, nr.x + w + 4.0 + 6.0, pl.line1_cy, 12.0, 2.1, th.mutd);
        }
    }
    if let Some(t) = pl.tag_r {
        p.rrect(t.x, t.y, t.w, t.h, 5.0, th.tag);
        text_c(p, t.cx(), t.cy(), "本机", &f.tag, th.fg2);
    }
    if let Some((c, al)) = pl.check {
        p.circle(c.cx(), c.cy(), 8.0, th.green.alpha(al));
        icon(p, Icon::Check, c.cx(), c.cy(), 10.0, 3.4, hexa(0xFFFFFF, al));
    }
    if pl.editing {
        text_c(p, r.cx(), pl.line2_cy, &pl.line2, &f.hint, th.mutd);
    } else {
        let c = if pl.dim { dimc } else { th.fg2.alpha(0.85) };
        text_c(p, r.cx(), pl.line2_cy, &pl.line2, &f.res, c);
    }
}

fn draw_screen(p: &mut Painter, a: &App, s: &Scr) {
    let th = &a.th;
    let r = s.r;
    if s.offline {
        p.rrect(r.x, r.y, r.w, r.h, 10.0, th.off);
        p.dashed(r.x, r.y, r.w, r.h, 10.0, 1.5, 5.0, 4.0, th.offline);
    } else {
        let l = s.lift;
        if l < 1.0 {
            p.shadow(r.x, r.y, r.w, r.h, 10.0, 0.0, 1.0, 2.0, 0.0, hexa(0, 0.08 * (1.0 - l)));
            p.shadow(r.x, r.y, r.w, r.h, 10.0, 0.0, 10.0, 24.0, -10.0, hexa(0, 0.30 * (1.0 - l)));
        }
        if l > 0.0 {
            p.shadow(r.x, r.y, r.w, r.h, 10.0, 0.0, 2.0, 4.0, 0.0, hexa(0, 0.10 * l));
            p.shadow(r.x, r.y, r.w, r.h, 10.0, 0.0, 22.0, 40.0, -12.0, hexa(0, 0.42 * l));
        }
        let wall = if s.peer { th.wb } else { th.wa };
        // 鼠标关着：壁纸去色（排布照样能拖）
        let gray = if a.is_on(Sw::Mouse) { 0.0 } else { 0.85 };
        p.rrect_with(r.x, r.y, r.w, r.h, 10.0, |x, y| desat(wall.at(x - r.x, y - r.y, r.w, r.h), gray));
        p.ring(r.x, r.y, r.w, r.h, 10.0, -1.0, 0.0, th.scrline);
    }
    if s.pill.primary {
        // 编号：主显示器左上角一颗玻璃小圆
        let n = if a.cfg.me_one() != s.peer { 1 } else { 2 };
        num_badge(p, a, r.x + 7.0, r.y + 7.0, n, false, s.offline);
    }
    draw_pill(p, a, &s.pill);
}

/// 去色：和自己的灰度混 t。
fn desat(c: Rgba, t: f32) -> Rgba {
    if t <= 0.0 {
        return c;
    }
    let l = 0.3 * c.r + 0.59 * c.g + 0.11 * c.b;
    c.lerp(Rgba { r: l, g: l, b: l, a: c.a }, t)
}

/// 屏幕左上角的编号圆（直径 22，左上角在 x, y）。solid：实心深色（声音页正在放的那台）。
fn num_badge(p: &mut Painter, a: &App, x: f32, y: f32, n: u8, solid: bool, dim: bool) {
    let th = &a.th;
    let r = R::new(x, y, 22.0, 22.0);
    let t = if n == 1 { "1" } else { "2" };
    if solid {
        let ink = if th.dark { hex(0xF2F2F7) } else { hex(0x1C1C1E) };
        let ink_fg = if th.dark { hex(0x1C1C1E) } else { hex(0xFFFFFF) };
        p.shadow(r.x, r.y, r.w, r.h, 11.0, 0.0, 1.0, 3.0, 0.0, hexa(0, 0.18));
        p.circle(r.cx(), r.cy(), 11.0, ink);
        text_c(p, r.cx(), r.cy(), t, &a.fonts.chip, ink_fg);
    } else {
        glass(p, th, r, 11.0);
        text_c(p, r.cx(), r.cy(), t, &a.fonts.chip, if dim { th.mutd } else { th.fg });
    }
}

/// 设备条上的一块屏幕（壁纸 + 投影；对方没连上时虚线框）。dim：整块变淡（这一项关着）。
fn strip_screen(p: &mut Painter, a: &App, r: R, peer: bool, offline: bool) {
    let th = &a.th;
    if offline {
        p.rrect(r.x, r.y, r.w, r.h, 10.0, th.off);
        p.dashed(r.x, r.y, r.w, r.h, 10.0, 1.5, 5.0, 4.0, th.offline);
        return;
    }
    p.shadow(r.x, r.y, r.w, r.h, 10.0, 0.0, 1.0, 2.0, 0.0, hexa(0, 0.08));
    p.shadow(r.x, r.y, r.w, r.h, 10.0, 0.0, 8.0, 18.0, -8.0, hexa(0, 0.22));
    let wall = if peer { th.wb } else { th.wa };
    p.rrect_with(r.x, r.y, r.w, r.h, 10.0, |x, y| wall.at(x - r.x, y - r.y, r.w, r.h));
    p.ring(r.x, r.y, r.w, r.h, 10.0, -1.0, 0.0, th.scrline);
}

/// 卡片里一个输入框的底和描边（focus：绿圈；err：红圈；hover：深一点）。
fn field_box(p: &mut Painter, th: &Theme, r: R, focus: bool, err: bool, hover: bool) {
    if err {
        p.ring(r.x, r.y, r.w, r.h, 8.0, 1.5, 4.0, th.redring);
        p.ring(r.x, r.y, r.w, r.h, 8.0, 0.0, 1.5, th.red);
    } else if focus {
        p.ring(r.x, r.y, r.w, r.h, 8.0, 1.5, 4.0, th.greenring);
        p.ring(r.x, r.y, r.w, r.h, 8.0, 0.0, 1.5, th.green);
    }
    p.rrect(r.x, r.y, r.w, r.h, 8.0, if hover && !focus { th.fillh } else { th.fill });
}

/// 卡片底（白 / 深灰，圆角 14，细描边 + 淡阴影）。
fn card(p: &mut Painter, th: &Theme, r: R) {
    p.shadow(r.x, r.y, r.w, r.h, 14.0, 0.0, 1.0, 3.0, 0.0, hexa(0, th.card_shadow));
    p.rrect(r.x, r.y, r.w, r.h, 14.0, th.card);
    p.ring(r.x, r.y, r.w, r.h, 14.0, 0.0, 1.0, th.cardline);
}

/// 舞台 / 设备条的底：圆角 16，竖直渐变 + 内描边。
fn stage_bg(p: &mut Painter, th: &Theme, r: R) {
    let (top, bot) = if th.dark { (hex(0x1D2129), hex(0x232427)) } else { (hex(0xE9EEF6), hex(0xF1F3F7)) };
    p.rrect_with(r.x, r.y, r.w, r.h, 16.0, |_, y| top.lerp(bot, ((y - r.y) / r.h).clamp(0.0, 1.0)));
    p.ring(r.x, r.y, r.w, r.h, 16.0, -1.0, 0.0, th.cardline);
}

/// 开关（t：0 关 ～ 1 开，带动画）。圆钮直径 = 高 - 4。
fn draw_switch(p: &mut Painter, th: &Theme, r: R, t: f32) {
    let rad = r.h / 2.0;
    p.rrect(r.x, r.y, r.w, r.h, rad, th.swoff.lerp(th.green, t));
    let d = r.h - 4.0;
    let kx = r.x + 2.0 + d / 2.0 + (r.w - 4.0 - d) * t;
    p.shadow(kx - d / 2.0, r.y + 2.0, d, d, d / 2.0, 0.0, 1.0, 3.0, 0.0, hexa(0, 0.22));
    p.ring(kx - d / 2.0, r.y + 2.0, d, d, d / 2.0, 0.0, 0.5, hexa(0, 0.04));
    p.circle(kx, r.cy(), d / 2.0, th.knob);
}

/// 滑杆：轨道 + 已填部分 + 圆钮，右边写数值。
fn draw_slider(p: &mut Painter, a: &App, row: R, frac: f32, val: &str, txt: Rgba) {
    let th = &a.th;
    let cy = row.cy();
    p.rrect(SLIDER_X, cy - 2.0, SLIDER_W, 4.0, 2.0, th.swoff);
    let fx = SLIDER_X + SLIDER_W * frac.clamp(0.0, 1.0);
    p.rrect(SLIDER_X, cy - 2.0, (fx - SLIDER_X).max(4.0), 4.0, 2.0, th.green);
    p.shadow(fx - 11.0, cy - 11.0, 22.0, 22.0, 11.0, 0.0, 1.0, 3.0, 0.0, hexa(0, 0.22));
    p.ring(fx - 11.0, cy - 11.0, 22.0, 22.0, 11.0, 0.0, 0.5, hexa(0, 0.06));
    p.circle(fx, cy, 11.0, th.knob);
    text_r(p, CTL_R, cy, val, &a.fonts.label, txt);
}

/// 侧栏 / 页头的图标块：开 = 绿，关 = 灰（图标也灰），连接 / 通用 = 中性灰。
fn icon_block(p: &mut Painter, th: &Theme, r: R, pg: Page, on: bool, rad: f32, size: f32) {
    let (bg, fg) = match (pg.sw(), on) {
        (None, _) => (if th.dark { hex(0x636366) } else { hex(0x8E8E93) }, hex(0xFFFFFF)),
        (Some(_), true) => (th.green, hex(0xFFFFFF)),
        (Some(_), false) => {
            if th.dark {
                (hex(0x3A3A3C), hex(0x7C7C80))
            } else {
                (hex(0xE6E6EA), hex(0xA1A1A6))
            }
        }
    };
    p.rrect(r.x, r.y, r.w, r.h, rad, bg);
    icon(p, pg.icon(), r.cx(), r.cy(), size, 2.0, fg);
}

/// 卡片里的一行：名字 + 灰字说明（没有说明时名字竖直居中）。
fn row_text(p: &mut Painter, a: &App, row: R, name: &str, hint: &str, name_c: Rgba, hint_c: Rgba) {
    let f = &a.fonts;
    if hint.is_empty() {
        p.text_mid(row.x, row.cy(), name, &f.label, name_c);
    } else {
        p.text_mid(row.x, row.y + 16.5, name, &f.label, name_c);
        p.text_mid(row.x, row.y + 34.0, hint, &f.hint, hint_c);
    }
}

/// 卡片里 n 行之间的细线。
fn row_lines(p: &mut Painter, th: &Theme, top: f32, n: usize) {
    for i in 1..n {
        let r = row_rect(top, i);
        p.rrect(r.x, r.y, r.w, 1.0, 0.0, th.hair);
    }
}

/// 设备条：两台屏幕 + 编号 + 屏下名字。dim：整条变淡（这一项关着）。返回两块屏幕。
fn draw_strip(p: &mut Painter, a: &App, dim: bool) -> [(R, bool, u8); 2] {
    let th = &a.th;
    let f = &a.fonts;
    stage_bg(p, th, STRIP);
    let ss = strip_screens(a);
    for (r, peer, n) in ss {
        let off = peer && !a.connected;
        p.alpha = if dim { 0.45 } else { 1.0 };
        strip_screen(p, a, r, peer, off);
        num_badge(p, a, r.x + 7.0, r.y + 7.0, n, false, off || dim);
        p.alpha = 1.0;
        let name = if peer { a.peer_name() } else { a.local_name() };
        let c = if off || dim { th.mutd } else { th.fg };
        let cy = r.y + r.h + 20.0;
        let nw = p.measure(&f.name, &name);
        let tag_w = if peer { 0.0 } else { a.measure(&f.tag, "本机") + 10.0 };
        let total = nw + if peer { 0.0 } else { 6.0 + tag_w };
        let x = r.cx() - total / 2.0;
        p.text_mid(x, cy, &name, &f.name, c);
        if !peer {
            let t = R::new(x + nw + 6.0, cy - 8.0, tag_w, 16.0);
            p.rrect(t.x, t.y, t.w, t.h, 5.0, th.tag);
            text_c(p, t.cx(), t.cy(), "本机", &f.tag, th.fg2);
        }
    }
    ss
}

/// 屏幕中间的玻璃圆 + 图标（声音页的喇叭、连接线中间的链接）。
fn glass_icon(p: &mut Painter, th: &Theme, cx: f32, cy: f32, d: f32, ic: Icon, size: f32, c: Rgba) {
    glass(p, th, R::new(cx - d / 2.0, cy - d / 2.0, d, d), d / 2.0);
    icon(p, ic, cx, cy, size, 2.0, c);
}

fn hotkey_name(vk: u32) -> String {
    match vk & 0xFF {
        0x91 => "Scroll Lock".into(),
        0x13 => "Pause".into(),
        0x14 => "Caps Lock".into(),
        v @ 0x70..=0x87 => format!("F{}", v - 0x6F),
        v => format!("键 0x{v:02X}"),
    }
}

fn render(p: &mut Painter, a: &App, sc: &Scene) {
    let th = &a.th;
    let f = &a.fonts;
    p.clear(th.bg);
    p.vgrad(0.0, 0.0, CLIENT_W, 324.0, th.stage_top, th.bg);
    // 两团很淡的光（给侧栏的玻璃板透出点颜色）
    let glows: [(f32, f32, f32, f32, Rgba, f32); 2] = if th.dark {
        [(60.0, 40.0, 420.0, 320.0, hex(0x3A4F7A), 0.45), (120.0, 520.0, 380.0, 300.0, hex(0x2C5A41), 0.35)]
    } else {
        [(60.0, 40.0, 420.0, 320.0, hex(0xA6C4FB), 0.35), (120.0, 520.0, 380.0, 300.0, hex(0x9FDFBB), 0.30)]
    };
    for (gx, gy, rx, ry, c, al) in glows {
        let (x0, y0) = ((gx - rx).max(0.0), (gy - ry).max(0.0));
        let (x1, y1) = ((gx + rx).min(CLIENT_W), (gy + ry).min(CLIENT_H));
        p.rrect_with(x0, y0, x1 - x0, y1 - y0, 0.0, |x, y| {
            let (dx, dy) = ((x - gx) / rx, (y - gy) / ry);
            let t = (1.0 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            c.alpha(al * t * t)
        });
    }

    // ── 侧栏 ──
    let sb = SIDEBAR;
    p.shadow(sb.x, sb.y, sb.w, sb.h, 16.0, 0.0, 10.0, 28.0, -14.0, hexa(0, 0.22));
    p.rrect(sb.x, sb.y, sb.w, sb.h, 16.0, if th.dark { hexa(0x2C2C30, 0.55) } else { hexa(0xFFFFFF, 0.52) });
    p.ring(sb.x, sb.y, sb.w, sb.h, 16.0, -1.0, 0.0, if th.dark { hexa(0xFFFFFF, 0.10) } else { hexa(0xFFFFFF, 0.95) });
    p.ring(sb.x, sb.y, sb.w, sb.h, 16.0, 0.0, 0.5, if th.dark { hexa(0, 0.40) } else { hexa(0, 0.07) });
    let paused = a.daemon && a.paused_now();
    let orange = hex(0xFF9F0A);
    let (st, dot) = if !a.connected {
        ("未连接", th.mut2)
    } else if paused {
        ("已暂停", orange)
    } else {
        ("已连接", th.green)
    };
    if a.connected && !paused {
        p.circle(30.0, 36.0, 6.5, th.greenring);
    }
    p.circle(30.0, 36.0, 4.0, dot);
    p.text_mid(42.0, 36.0, st, &f.side_b, th.fg);
    let sub = if a.connected { a.peer_name() } else { "对方未开或不在同网".to_string() };
    p.text_mid(42.0, 55.0, &sub, &f.small, th.mutd);
    p.rrect(30.0, 121.0, 160.0, 1.0, 0.0, th.hair);
    let sy = a.nav_y();
    if th.dark {
        p.rrect(18.0, sy, 182.0, 40.0, 11.0, hexa(0xFFFFFF, 0.11));
    } else {
        p.shadow(18.0, sy, 182.0, 40.0, 11.0, 0.0, 1.0, 3.0, 0.0, hexa(0, 0.10));
        p.rrect(18.0, sy, 182.0, 40.0, 11.0, hex(0xFFFFFF));
        p.ring(18.0, sy, 182.0, 40.0, 11.0, 0.0, 0.5, hexa(0, 0.08));
    }
    for pg in PAGES {
        let r = nav_rect(pg);
        if a.hover == Hot::Nav(pg) && pg != a.page {
            p.rrect(r.x, r.y, r.w, r.h, 11.0, th.hov);
        }
        let on = pg.sw().is_none_or(|s| a.is_on(s));
        icon_block(p, th, R::new(25.0, r.y + 6.0, 28.0, 28.0), pg, on, 8.0, 17.0);
        let font = if pg == a.page { &f.nav_b } else { &f.nav };
        p.text_mid(63.0, r.cy(), pg.title(), font, if on { th.fg } else { th.mutd });
        if !on {
            text_r(p, 190.0, r.cy(), "关", &f.small, th.mutd);
        }
    }

    // ── 页头 ──
    let pg = a.page;
    let on = pg.sw().is_none_or(|s| a.is_on(s));
    icon_block(p, th, HEAD_ICON, pg, on, 11.0, 22.0);
    let desc = match pg {
        Page::Conn if a.connected => format!("已连接 · {}", a.peer_name()),
        Page::Conn => "未连接".to_string(),
        _ => pg.desc().to_string(),
    };
    if desc.is_empty() {
        p.text_mid(288.0, 44.0, pg.title(), &f.title, th.fg);
    } else {
        p.text_mid(288.0, 36.0, pg.title(), &f.title, th.fg);
        p.text_mid(288.0, 57.0, &desc, &f.stat, th.mutd);
    }
    if let Some(s) = pg.sw() {
        draw_switch(p, th, HEAD_SW, a.switch_t(s));
        if !a.connected && s != Sw::Clip {
            let t = "连上后生效";
            let w = a.measure(&f.upchip, t) + 20.0;
            let r = R::new(HEAD_SW.x - 12.0 - w, HEAD_SW.cy() - 11.0, w, 22.0);
            p.rrect(r.x, r.y, r.w, r.h, 11.0, th.fill);
            text_c(p, r.cx(), r.cy(), t, &f.upchip, th.mutd);
        }
    }
    if pg == Page::Conn && a.daemon {
        let r = pause_rect(a.pause_w());
        let hov = a.hover == Hot::Pause;
        let ink = if th.dark { hex(0xF2F2F7) } else { hex(0x1C1C1E) };
        let ink_fg = if th.dark { hex(0x1C1C1E) } else { hex(0xFFFFFF) };
        let (t, ic, fg) = if paused {
            p.rrect(r.x, r.y, r.w, r.h, 15.0, if hov { ink.lerp(th.bg, 0.15) } else { ink });
            ("继续", Icon::Play, ink_fg)
        } else {
            glass(p, th, r, 15.0);
            if hov {
                p.rrect(r.x, r.y, r.w, r.h, 15.0, th.hov);
            }
            ("暂停", Icon::Pause, th.fg)
        };
        icon(p, ic, r.x + 14.0 + 6.0, r.cy(), 12.0, 2.4, fg);
        p.text_mid(r.x + 14.0 + 12.0 + 6.0, r.cy(), t, &f.fbtn_ink, fg);
    }

    // 这一项关着：行内容变淡（文字和卡片底色混，形状降不透明度），仍可改
    let tc = |c: Rgba| if on { c } else { c.lerp(th.card, 0.55) };

    match pg {
        Page::Conn => render_conn(p, a),
        Page::Mouse => render_mouse(p, a, sc),
        Page::Kbd => {
            draw_strip(p, a, !on);
            card(p, th, R::new(CX, CARD_Y, CW, 66.0));
            let row = row_rect(CARD_Y, 0);
            let mouse = a.cfg.share.mouse;
            let hint = if mouse { "跟最后用的鼠标" } else { "鼠标已关，靠它过去" };
            row_text(p, a, row, "跟随鼠标", hint, tc(th.fg), tc(th.mutd));
            p.alpha = if on && mouse { 1.0 } else { 0.45 };
            draw_switch(p, th, row_switch(row), if mouse { a.switch_t(Sw::Follow) } else { 1.0 });
            p.alpha = 1.0;
        }
        Page::Audio => {
            let ss = draw_strip(p, a, !on);
            if on {
                for (r, peer, n) in ss {
                    let playing = a.audio == n as usize - 1 || a.audio == 2;
                    if playing {
                        num_badge(p, a, r.x + 7.0, r.y + 7.0, n, true, false);
                        let c = if peer && !a.connected { th.mutd } else { th.fg };
                        glass_icon(p, th, r.cx(), r.cy(), 38.0, Icon::Speaker, 18.0, c);
                    }
                }
            }
            card(p, th, R::new(CX, CARD_Y, CW, 116.0));
            row_lines(p, th, CARD_Y, 2);
            let r0 = row_rect(CARD_Y, 0);
            row_text(p, a, r0, "在哪放", AUDIO_HINTS[a.audio.min(2)], tc(th.fg), tc(th.mutd));
            p.alpha = if on { 1.0 } else { 0.45 };
            let g = AUDIO_SEG;
            p.rrect(g.x, g.y, g.w, g.h, 8.0, th.seg_track);
            let th_r = audio_seg(0);
            let tx = th_r.x + 52.0 * a.audio_pos();
            p.shadow(tx, th_r.y, th_r.w, th_r.h, 6.0, 0.0, 1.0, 2.0, 0.0, hexa(0, if th.dark { 0.35 } else { 0.12 }));
            p.rrect(tx, th_r.y, th_r.w, th_r.h, 6.0, th.seg_thumb);
            p.alpha = 1.0;
            for (k, label) in AUDIO_LABELS.iter().enumerate() {
                let r = audio_seg(k);
                let sel = k == a.audio;
                let col = if sel || a.hover == Hot::Audio(k) { th.fg } else { th.fg2 };
                text_c(p, r.cx(), r.cy(), label, if sel { &f.seg_on } else { &f.btn }, tc(col));
            }
            let r1 = row_rect(CARD_Y, 1);
            row_text(p, a, r1, "音量", "对方的声音", tc(th.fg), tc(th.mutd));
            let v = a.cfg.audio.volume.clamp(0.0, 1.0);
            p.alpha = if on { 1.0 } else { 0.45 };
            draw_slider(p, a, r1, v, &format!("{:.0}%", v * 100.0), tc(th.fg2));
            p.alpha = 1.0;
        }
        Page::Clip => {
            card(p, th, R::new(CX, TOP_CARD_Y, CW, 66.0));
            let row = row_rect(TOP_CARD_Y, 0);
            row_text(p, a, row, "复制提示", "复制后弹提示", tc(th.fg), tc(th.mutd));
            p.alpha = if on { 1.0 } else { 0.45 };
            draw_switch(p, th, row_switch(row), a.switch_t(Sw::Notify));
            p.alpha = 1.0;
        }
        Page::General => {
            card(p, th, R::new(CX, TOP_CARD_Y, CW, 116.0));
            row_lines(p, th, TOP_CARD_Y, 2);
            let r0 = row_rect(TOP_CARD_Y, 0);
            row_text(p, a, r0, "开机自启", "", th.fg, th.mutd);
            draw_switch(p, th, row_switch(r0), a.switch_t(Sw::Autostart));
            let r1 = row_rect(TOP_CARD_Y, 1);
            row_text(p, a, r1, "暂停热键", "暂停全部共享", th.fg, th.mutd);
            let t = hotkey_name(a.cfg.input.hotkey_vk);
            let w = a.measure(&f.fbtn, &t) + 20.0;
            let k = R::new(CTL_R - w, r1.cy() - 12.0, w, 24.0);
            p.rrect(k.x, k.y, k.w, k.h, 7.0, th.fill);
            p.ring(k.x, k.y, k.w, k.h, 7.0, -1.0, 0.0, th.hair);
            text_c(p, k.cx(), k.cy(), &t, &f.fbtn, th.fg2);
            card(p, th, R::new(CX, VER_CARD_Y, CW, 66.0));
            draw_footer(p, a);
        }
    }

    // 提示 / 确认框
    if let Some(t) = sc.tip.filter(|_| pg == Page::Mouse) {
        p.shadow(t.x, t.y, t.w, t.h, 7.0, 0.0, 6.0, 16.0, -6.0, hexa(0, 0.25));
        p.rrect(t.x, t.y, t.w, t.h, 7.0, th.tip);
        p.ring(t.x, t.y, t.w, t.h, 7.0, -1.0, 0.0, th.tipline);
        text_c(p, t.cx(), t.cy(), "连上后可改", &f.small, th.tipfg);
    }
    if a.popover && pg == Page::Conn {
        let r = POP;
        p.shadow(r.x, r.y, r.w, r.h, 10.0, 0.0, 10.0, 28.0, -6.0, hexa(0, 0.30));
        p.rrect(r.x, r.y, r.w, r.h, 10.0, th.tip);
        p.ring(r.x, r.y, r.w, r.h, 10.0, -1.0, 0.0, th.tipline);
        p.text_mid(r.x + 14.0, r.y + 22.0, "换新密钥后两台都要重新粘贴", &f.btn, th.tipfg);
        let cb = POP_CANCEL;
        p.rrect(cb.x, cb.y, cb.w, cb.h, 7.0, if a.hover == Hot::PopCancel { th.fillh } else { th.fill });
        text_c(p, cb.cx(), cb.cy(), "取消", &f.btn, th.fg);
        let ok = POP_OK;
        let g = if a.hover == Hot::PopOk { th.green.lerp(hex(0), 0.08) } else { th.green };
        p.rrect(ok.x, ok.y, ok.w, ok.h, 7.0, g);
        text_c(p, ok.cx(), ok.cy(), "更换", &f.btn, hex(0xFFFFFF));
    }
}

/// 连接页：设备条（两台之间一条连线）+ 对方 IP / 配对密钥 / 本机 IP。
fn render_conn(p: &mut Painter, a: &App) {
    let th = &a.th;
    let f = &a.fonts;
    let ss = draw_strip(p, a, false);
    let (l, r) = (ss[0].0, ss[1].0);
    let y = l.cy();
    let (x0, x1) = (l.x + l.w + 10.0, r.x - 10.0);
    let mid = (x0 + x1) / 2.0;
    if a.connected {
        p.glow(x0, y - 1.0, x1 - x0, 2.0, 1.0, 6.0, th.green.alpha(0.5));
        p.rrect(x0, y - 1.0, x1 - x0, 2.0, 1.0, th.green);
    } else {
        let n = ((x1 - x0) / 6.0).floor().max(1.0) as i32;
        let step = (x1 - x0) / n as f32;
        for i in 0..=n {
            p.circle(x0 + step * i as f32, y, 1.3, th.offline);
        }
    }
    if a.connected {
        p.ring(mid - 15.0, y - 15.0, 30.0, 30.0, 15.0, 0.0, 3.0, th.greenring);
    }
    glass_icon(p, th, mid, y, 30.0, Icon::Link, 16.0, if a.connected { th.green } else { th.mutd });

    card(p, th, R::new(CX, CARD_Y, CW, 166.0));
    row_lines(p, th, CARD_Y, 3);
    // 对方 IP
    let r0 = row_rect(CARD_Y, 0);
    row_text(p, a, r0, "对方 IP", a.err_ip.unwrap_or("填一台即可"), th.fg, if a.err_ip.is_some() { th.red } else { th.mutd });
    let editing_ip = a.edit == Some(Field::Ip);
    field_box(p, th, IP_BOX, editing_ip, a.err_ip.is_some(), a.hover == Hot::Ip);
    if !editing_ip {
        let v = a.draft_ip.clone().unwrap_or_else(|| a.cfg.peer.trim().to_string());
        if v.is_empty() {
            p.text_mid(IP_BOX.x + 10.0, IP_BOX.cy(), "未填", &f.label, th.mut2);
        } else {
            p.text_mid(IP_BOX.x + 10.0, IP_BOX.cy(), &v, &f.input, th.fg);
        }
    }
    // 配对密钥
    let r1 = row_rect(CARD_Y, 1);
    row_text(p, a, r1, "配对密钥", a.err_key.unwrap_or("两台须相同"), th.fg, if a.err_key.is_some() { th.red } else { th.mutd });
    let editing_key = a.edit == Some(Field::Key);
    field_box(p, th, KEY_BOX, editing_key, a.err_key.is_some(), a.hover == Hot::Key);
    if !editing_key {
        let v = a.draft_key.clone().unwrap_or_else(|| a.cfg.key.clone());
        p.text_mid(KEY_BOX.x + 10.0, KEY_BOX.cy(), &short_key(&v), &f.mono, th.fg);
    }
    for (r, h, ic) in [(COPY_KEY, Hot::CopyKey, Icon::Copy), (NEW_KEY, Hot::NewKey, Icon::Refresh)] {
        let on = a.hover == h || (h == Hot::NewKey && a.popover);
        p.rrect(r.x, r.y, r.w, r.h, 8.0, if on { th.fillh } else { th.fill });
        if a.copied_now(h) {
            icon(p, Icon::Check, r.cx(), r.cy(), 15.0, 2.4, th.green);
        } else {
            icon(p, ic, r.cx(), r.cy(), 15.0, 1.9, th.fg2);
        }
    }
    // 本机 IP
    let r2 = row_rect(CARD_Y, 2);
    row_text(p, a, r2, "本机 IP", "", th.fg, th.mutd);
    match a.ips.first() {
        Some(ip) => {
            text_r(p, COPY_IP.x - 6.0, r2.cy(), ip, &f.input, th.fg2);
            if a.hover == Hot::CopyIp {
                p.rrect(COPY_IP.x, COPY_IP.y, COPY_IP.w, COPY_IP.h, 8.0, th.fill);
            }
            if a.copied_now(Hot::CopyIp) {
                icon(p, Icon::Check, COPY_IP.cx(), COPY_IP.cy(), 15.0, 2.4, th.green);
            } else {
                icon(p, Icon::Copy, COPY_IP.cx(), COPY_IP.cy(), 15.0, 1.9, th.mutd);
            }
        }
        None => text_r(p, CTL_R, r2.cy(), "无", &f.label, th.mutd),
    }
}

/// 鼠标页：舞台（拖动摆放）+ 移动速度。
fn render_mouse(p: &mut Painter, a: &App, sc: &Scene) {
    let th = &a.th;
    let f = &a.fonts;
    let on = a.is_on(Sw::Mouse);
    stage_bg(p, th, STAGE);
    text_r(p, STAGE.x + STAGE.w - 16.0, STAGE.y + 20.0, "拖动摆放", &f.small, th.mutd);
    // 接缝：连着且鼠标开着是绿条，否则虚点
    for e in &sc.edges {
        if a.connected && on {
            p.glow(e.x, e.y, e.w, e.h, 1.5, 8.0, th.green.alpha(0.55));
            p.rrect(e.x, e.y, e.w, e.h, 1.5, th.green);
        } else {
            let (len, along) = if sc.edge_vertical { (e.h, e.y) } else { (e.w, e.x) };
            let n = (len / 6.0).floor().max(1.0) as i32;
            let step = len / n as f32;
            for i in 0..=n {
                let t = along + step * i as f32;
                let (cx, cy) = if sc.edge_vertical { (e.cx(), t) } else { (t, e.cy()) };
                p.circle(cx, cy, 1.3, th.offline);
            }
        }
    }
    for s in sc.scr.iter().filter(|s| !s.peer) {
        draw_screen(p, a, s);
    }
    if let Some(g) = sc.ghost {
        p.rrect(g.x, g.y, g.w, g.h, 10.0, th.green.alpha(0.10));
        p.dashed(g.x, g.y, g.w, g.h, 10.0, 1.5, 5.0, 4.0, th.green);
    }
    for s in sc.scr.iter().filter(|s| s.peer) {
        p.alpha = 1.0 - 0.06 * s.lift;
        draw_screen(p, a, s);
        p.alpha = 1.0;
    }
    // 底行：说明 + 尺寸比例
    let cy = STEPPER.cy();
    let nw = p.text_mid(STAGE.x + 18.0, cy, &a.peer_name(), &f.stat_b, th.fg);
    p.text_mid(STAGE.x + 18.0 + nw + 7.0, cy, &status_text(&sc.layout_now), &f.stat, th.fg2);
    text_r(p, STEPPER.x - 8.0, cy, "尺寸比例", &f.small, th.mutd);
    glass(p, th, STEPPER, 14.0);
    for (r, h, ic) in [(MINUS, Hot::Minus, Icon::Minus), (PLUS, Hot::Plus, Icon::Plus)] {
        if a.hover == h {
            p.circle(r.cx(), r.cy(), 11.0, th.hov);
        }
        icon(p, ic, r.cx(), r.cy(), 12.0, 2.4, th.fg2);
    }
    text_c(p, (MINUS.x + MINUS.w + PLUS.x) / 2.0, cy, &format!("{:.2}", sanitize_scale(a.layout.scale)), &f.step, th.fg);
    // 移动速度
    card(p, th, R::new(CX, SPEED_CARD_Y, CW, 66.0));
    let row = row_rect(SPEED_CARD_Y, 0);
    let tc = |c: Rgba| if on { c } else { c.lerp(th.card, 0.55) };
    row_text(p, a, row, "移动速度", "在对方屏幕上", tc(th.fg), tc(th.mutd));
    let s = a.cfg.input.speed;
    p.alpha = if on { 1.0 } else { 0.45 };
    draw_slider(p, a, row, speed_frac(s), &format!("{:.1}×", s.clamp(SPEED_MIN, SPEED_MAX)), tc(th.fg2));
    p.alpha = 1.0;
}

/// 底部一行：版本 + 更新（设计稿 设计\检查更新\settings.html）。
fn draw_footer(p: &mut Painter, a: &App) {
    let th = &a.th;
    let f = &a.fonts;
    let cy = FOOT_CY;
    let view = a.upd_view();
    let ink = if th.dark { hex(0xF2F2F7) } else { hex(0x1C1C1E) };
    let ink_fg = if th.dark { hex(0x1C1C1E) } else { hex(0xFFFFFF) };
    let orange = hex(0xFF9F0A);

    // 左：kmlink 版本（· 对方版本）（新版标签）
    let shown = match &view {
        UpdView::Done(v) if !v.is_empty() => v.clone(),
        _ => a.upd.app.clone(),
    };
    let shown = if shown.is_empty() { crate::update::VERSION.to_string() } else { shown };
    let mut x = CX + 20.0;
    x += p.text_mid(x, cy, "kmlink", &f.foot_b, th.fg2) + 5.0;
    let mut ver = shown;
    if matches!(view, UpdView::AvailSolo(_)) && !a.upd.peer_app.is_empty() {
        ver = format!("{ver} · 对方 {}", a.upd.peer_app);
    }
    x += p.text_mid(x, cy, &ver, &f.foot, th.mutd) + 10.0;
    if let UpdView::Avail(v) | UpdView::AvailSolo(v) = &view {
        let t = format!("新版 {v}");
        let w = 6.0 + 10.0 + 4.0 + p.measure(&f.upchip, &t) + 8.0;
        let (bg, fg) = if th.dark { (hexa(0x30D158, 0.18), hex(0x30D158)) } else { (hexa(0x34C759, 0.14), hex(0x248A3D)) };
        p.rrect(x, cy - 11.0, w, 22.0, 7.0, bg);
        arrow_up(p, x + 11.0, cy, 9.0, fg);
        p.text_mid(x + 20.0, cy, &t, &f.upchip, fg);
    }

    // 右：按钮 / 状态
    let mut right = CTL_R;
    if let Some((r, t, is_ink)) = a.upd_button() {
        let hov = a.hover == Hot::Upd;
        if is_ink {
            p.rrect(r.x, r.y, r.w, r.h, 14.0, if hov { ink.lerp(th.bg, 0.15) } else { ink });
            text_c(p, r.cx(), r.cy(), t, &f.fbtn_ink, ink_fg);
        } else {
            glass(p, th, r, 14.0);
            if hov {
                p.rrect(r.x, r.y, r.w, r.h, 14.0, th.hov);
            }
            text_c(p, r.cx(), r.cy(), t, &f.fbtn, th.fg);
        }
        right = r.x - 10.0;
    }
    let spin = |p: &mut Painter, rx: f32| {
        let a0 = a.spin0.elapsed().as_secs_f32() * std::f32::consts::TAU * 1.2;
        p.stroke(&[Seg::Arc(rx, cy, 5.5, a0, a0 + 4.2)], 1.6, th.mutd);
    };
    match &view {
        UpdView::Checking | UpdView::Restarting => {
            let t = if view == UpdView::Checking { "正在检查…" } else { "正在重启…" };
            let w = p.measure(&f.foot, t);
            p.text_mid(right - w, cy, t, &f.foot, th.mutd);
            spin(p, right - w - 12.0);
        }
        UpdView::Latest | UpdView::Done(_) => {
            let t = match &view {
                UpdView::Done(v) => format!("已更新到 {v}"),
                _ => "已是最新".to_string(),
            };
            let w = p.measure(&f.foot, &t);
            p.text_mid(right - w, cy, &t, &f.foot, th.fg2);
            icon(p, Icon::Check, right - w - 13.0, cy, 14.0, 2.4, th.green);
        }
        UpdView::AvailSolo(_) => text_r(p, right, cy, "对方需手动更新", &f.foot, th.mutd),
        UpdView::Down(pct) => {
            let bar = R::new(right - 120.0, cy - 2.0, 120.0, 4.0);
            p.rrect(bar.x, bar.y, bar.w, bar.h, 2.0, th.fill);
            let fw = (bar.w * (*pct as f32 / 100.0)).max(4.0);
            p.rrect(bar.x, bar.y, fw, bar.h, 2.0, th.green);
            text_r(p, bar.x - 10.0, cy, &format!("正在更新 {pct}%"), &f.foot, th.fg2);
        }
        UpdView::Failed(e) => {
            // 括号里的细节（哪一步连不上）不上界面
            let e = e.split('（').next().unwrap_or("").trim();
            let t = if e.is_empty() { "更新失败".to_string() } else { format!("更新失败 · {e}") };
            let w = p.measure(&f.foot, &t);
            p.text_mid(right - w, cy, &t, &f.foot, th.fg2);
            p.circle(right - w - 9.0, cy, 3.5, orange);
        }
        _ => {}
    }
}

/// 向上的小箭头（新版标签里），中心 (cx, cy)，高 h。
fn arrow_up(p: &mut Painter, cx: f32, cy: f32, h: f32, c: Rgba) {
    let t = cy - h / 2.0;
    let b = cy + h / 2.0;
    p.stroke(&[Seg::Line(cx, b, cx, t + 1.0), Seg::Line(cx - 3.5, t + 3.5, cx, t), Seg::Line(cx + 3.5, t + 3.5, cx, t)], 1.6, c);
}

// ---------------------------------------------------------------------------
// 数据来源
// ---------------------------------------------------------------------------

/// 本机显示器 + 每块的 DPI（EnumDisplayMonitors + GetDpiForMonitor）。
fn local_monitors() -> Vec<Mon> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe {
            let v = &mut *(data.0 as *mut Vec<Mon>);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetMonitorInfoW(h, &mut mi).as_bool() {
                let r = mi.rcMonitor;
                let (mut dx, mut dy) = (0u32, 0u32);
                let dpi = GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).ok().map(|_| dx).filter(|d| *d > 0);
                v.push(Mon { r: Rect::new(r.left, r.top, r.right, r.bottom), dpi, primary: mi.dwFlags & 1 != 0 });
            }
        }
        BOOL(1)
    }
    let mut v: Vec<Mon> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v.retain(|m| m.r.w() > 0 && m.r.h() > 0);
    v.sort_by_key(|m| (m.r.l, m.r.t));
    if v.is_empty() {
        v.push(Mon { r: Rect::new(0, 0, 1920, 1080), dpi: None, primary: true });
    }
    fix_primary(&mut v);
    v
}

fn placeholder_peer() -> Vec<Mon> {
    vec![Mon { r: Rect::new(0, 0, 1920, 1080), dpi: None, primary: true }]
}

/// 进程还在不在（主程序以管理员身份运行时打不开也算在）。
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return false;
    }
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let mut code = 0u32;
                let ok = GetExitCodeProcess(h, &mut code).is_ok();
                let _ = CloseHandle(h);
                !ok || code == 259 // STILL_ACTIVE
            }
            Err(e) => e.code() == E_ACCESSDENIED,
        }
    }
}

/// 读 status.json：(已连接, 对方名字)。主程序的互斥量还在、且写文件的 pid 还活着才算已连接
/// （主程序异常退出留下的旧文件 + pid 被别的进程复用时，单看 pid 会误报）。
fn read_status() -> (bool, String) {
    let Ok(s) = std::fs::read_to_string(config::dir().join("status.json")) else { return (false, String::new()) };
    match parse_status(&s) {
        Some((pid, c, name)) => (c && sys::daemon_running() && pid_alive(pid), name),
        None => (false, String::new()),
    }
}

/// 原子写一个小文件（先写临时文件再改名）。
fn write_atomic(name: &str, body: &str) -> std::io::Result<()> {
    let dir = config::dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body)?;
    let r = std::fs::rename(&tmp, dir.join(name));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// 改对方的名字：写 rename_peer.json，主程序取走后发给对方。
fn write_rename_peer(name: &str) -> bool {
    let body = serde_json::json!({ "name": name, "t": config::now_ms() }).to_string();
    match write_atomic("rename_peer.json", &body) {
        Ok(()) => true,
        Err(e) => {
            crate::log!("设置窗口：写 rename_peer.json 失败 {e}");
            false
        }
    }
}

/// 读 peer.json；带 DPI 的新格式解析不了时退回老的读法（只有名字和显示器）。
fn read_peer() -> Option<(String, Vec<Mon>)> {
    if let Some(p) = std::fs::read_to_string(config::peer_desk_path()).ok().and_then(|s| parse_peer(&s)) {
        return Some(p);
    }
    let (name, desk) = config::load_peer_desk()?;
    let mut ms: Vec<Mon> = desk.monitors.into_iter().filter(|r| r.w() > 0 && r.h() > 0).map(|r| Mon { r, dpi: None, primary: false }).collect();
    if ms.is_empty() {
        return None;
    }
    fix_primary(&mut ms);
    Some((name, ms))
}

fn cfg_mtime() -> Option<SystemTime> {
    std::fs::metadata(config::path()).and_then(|m| m.modified()).ok()
}

/// 本机局域网 IPv4：已连接的非回环网卡上的单播地址，过滤见 usable_lan_ip；有默认网关的网卡排前面。
fn lan_ips() -> Vec<String> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GET_ADAPTERS_ADDRESSES_FLAGS,
        GetAdaptersAddresses, IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_UNICAST_ADDRESS_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    let flags = GET_ADAPTERS_ADDRESSES_FLAGS(
        GAA_FLAG_SKIP_ANYCAST.0 | GAA_FLAG_SKIP_MULTICAST.0 | GAA_FLAG_SKIP_DNS_SERVER.0 | GAA_FLAG_INCLUDE_GATEWAYS.0,
    );
    let mut found: Vec<(Ipv4Addr, bool)> = Vec::new();
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
            break;
        }
        let mut a = p as *const IP_ADAPTER_ADDRESSES_LH;
        while !a.is_null() {
            let ad = unsafe { &*a };
            if ad.OperStatus == IfOperStatusUp && ad.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let gw = !ad.FirstGatewayAddress.is_null();
                let mut u = ad.FirstUnicastAddress as *const IP_ADAPTER_UNICAST_ADDRESS_LH;
                while !u.is_null() {
                    let ua = unsafe { &*u };
                    let sa = ua.Address.lpSockaddr;
                    if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
                        let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
                        let ip = Ipv4Addr::from(unsafe { sin.sin_addr.S_un.S_addr }.to_ne_bytes());
                        if usable_lan_ip(ip) && !found.iter().any(|(x, _)| *x == ip) {
                            found.push((ip, gw));
                        }
                    }
                    u = ua.Next;
                }
            }
            a = ad.Next;
        }
        break;
    }
    found.sort_by_key(|(_, gw)| !*gw); // 稳定排序：有网关的在前
    found.into_iter().map(|(ip, _)| ip.to_string()).collect()
}

// ---------------------------------------------------------------------------
// 窗口
// ---------------------------------------------------------------------------

const MAIN_CLASS: PCWSTR = w!("kmlink_settings");
const EDIT_IDS: [i32; 3] = [201, 202, 203];

// 样式位 / 消息（部分常量在未启用的 feature 里，直接写数值）
const ES_AUTOHSCROLL: u32 = 0x80;
const EM_GETSEL: u32 = 0xB0;
const EM_SETSEL: u32 = 0xB1;
const EM_SETLIMITTEXT: u32 = 0xC5;
const EM_SETMARGINS: u32 = 0xD3;
const EN_CHANGE: u32 = 0x300;
const EN_KILLFOCUS: u32 = 0x200;
const HTCLIENT: usize = 1;
const CF_UNICODETEXT: u32 = 13;
const SIZE_MINIMIZED: usize = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;

fn scale_px(v: f32, s: f32) -> i32 {
    (v * s).round() as i32
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

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 状态变了：摆好输入框、重画。不能在借用 APP 时调用。
fn refresh() {
    sync_edits();
    if let Some(h) = with_app(|a| a.hwnd) {
        invalidate(h);
    }
    ensure_anim();
}

/// 有动画就开 16ms 的计时器。
fn ensure_anim() {
    if let Some((h, true)) = with_app(|a| (a.hwnd, a.animating())) {
        unsafe {
            SetTimer(Some(h), TIMER_ANIM, 16, None);
        }
    }
}

/// 输入框该放哪（物理像素），None = 隐藏。
fn edit_rect(a: &App, sc: &Scene, slot: usize) -> Option<(i32, i32, i32, i32)> {
    let f = a.edit?;
    if f.slot() != slot {
        return None;
    }
    let (r, font) = match f {
        Field::Ip => (R::new(IP_BOX.x + 10.0, IP_BOX.cy(), IP_BOX.w - 20.0, 0.0), &a.fonts.input),
        Field::Key => (R::new(KEY_BOX.x + 10.0, KEY_BOX.cy(), KEY_BOX.w - 20.0, 0.0), &a.fonts.mono),
        Field::LocalName | Field::PeerName => {
            let b = sc.namebox?;
            (R::new(b.x + 8.0, b.cy(), b.w - 16.0, 0.0), &a.fonts.name)
        }
    };
    let s = a.s;
    let hh = (font.cell * s).ceil() as i32 + 1;
    let x = scale_px(r.x, s);
    let y = (r.y * s - hh as f32 / 2.0).round() as i32;
    Some((x, y, scale_px(r.x + r.w, s) - x, hh))
}

fn sync_edits() {
    let Some(plan) = with_app(|a| {
        let sc = scene(a);
        let mut out = Vec::new();
        for slot in 0..3 {
            let want = edit_rect(a, &sc, slot);
            if want != a.edit_pos[slot] {
                let resized = matches!((a.edit_pos[slot], want), (Some(o), Some(n)) if o.2 != n.2);
                a.edit_pos[slot] = want;
                out.push((a.edits[slot], want, resized));
            }
        }
        out
    }) else {
        return;
    };
    for (h, want, resized) in plan {
        unsafe {
            match want {
                Some((x, y, w, hh)) => {
                    let _ = MoveWindow(h, x, y, w, hh, true);
                    let _ = ShowWindow(h, SW_SHOW);
                    if resized {
                        // 变宽后把横向滚动复位（先选到开头再还原选区），不然字会被卷走一截
                        let r = SendMessageW(h, EM_GETSEL, None, None).0 as u32;
                        let (s0, s1) = (r & 0xFFFF, r >> 16);
                        SendMessageW(h, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(0)));
                        SendMessageW(h, EM_SETSEL, Some(WPARAM(s0 as usize)), Some(LPARAM(s1 as isize)));
                    }
                }
                None => {
                    let _ = ShowWindow(h, SW_HIDE);
                }
            }
        }
    }
}

/// 已有设置窗口：提到前面，返回 true（本进程该退出）。已有的那个以管理员身份运行时也算。
fn already_running() -> bool {
    // 调试截图（KMLINK_UPD）：不管已开着的设置窗口
    if std::env::var_os("KMLINK_UPD").is_some() {
        return false;
    }
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

/// DwmSetWindowAttribute（运行时取函数，不额外链接 dwmapi）。
fn dwm_set(hwnd: HWND, attr: u32, val: u32) {
    type F = unsafe extern "system" fn(HWND, u32, *const core::ffi::c_void, u32) -> i32;
    unsafe {
        let Ok(m) = LoadLibraryW(w!("dwmapi.dll")) else { return };
        if let Some(f) = GetProcAddress(m, s!("DwmSetWindowAttribute")) {
            let f: F = std::mem::transmute(f);
            f(hwnd, attr, &val as *const u32 as *const _, 4);
        }
    }
}

/// 标题栏跟主题：深色模式开关 + 标题栏底色 = 舞台顶色 + 标题文字色。
fn apply_caption(hwnd: HWND, th: &Theme) {
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const DWMWA_CAPTION_COLOR: u32 = 35;
    const DWMWA_TEXT_COLOR: u32 = 36;
    dwm_set(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, th.dark as u32);
    dwm_set(hwnd, DWMWA_CAPTION_COLOR, th.stage_top.colorref().0);
    dwm_set(hwnd, DWMWA_TEXT_COLOR, th.fg.colorref().0);
}

/// 窗口图标（设计稿里的绿色小方块），边长 px。
fn app_icon(px: i32) -> Option<HICON> {
    use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS};
    let n = px.max(8) as usize;
    let k = n as f32 / 16.0;
    let cov = |d: f32| (0.5 - d * k).clamp(0.0, 1.0);
    let mut argb = vec![0u32; n * n];
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = ((x as f32 + 0.5) / k, (y as f32 + 0.5) / k);
            let bg = cov(ui::sd_rrect(fx, fy, 8.0, 8.0, 7.5, 7.5, 4.0));
            let ring = cov(ui::sd_rrect(fx, fy, 5.5, 7.0, 2.5, 2.5, 1.0).abs() - 0.65);
            let fill = cov(ui::sd_rrect(fx, fy, 10.75, 8.75, 2.25, 2.25, 1.0));
            let wht = ring.max(fill);
            let c = hex(0x34C759).lerp(hex(0xFFFFFF), wht);
            let q = |v: f32| (v * 255.0 + 0.5) as u32;
            argb[y * n + x] = q(bg) << 24 | q(c.r) << 16 | q(c.g) << 8 | q(c.b);
        }
    }
    unsafe {
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n as i32,
                biHeight: -(n as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(color.into());
            return None;
        }
        std::ptr::copy_nonoverlapping(argb.as_ptr(), bits as *mut u32, n * n);
        let mask_bits = vec![0u8; n.div_ceil(16) * 2 * n];
        let mask = CreateBitmap(n as i32, n as i32, 1, 1, Some(mask_bits.as_ptr() as *const _));
        let ii = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&ii).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

pub fn run() {
    if already_running() {
        return;
    }
    // 配置：缺失就新建；解析失败由 load_or_create 备份并保留密钥，不会悄悄换钥
    let (cfg, _) = Config::load_or_create();
    let mut layout = cfg.layout.layout;
    layout.scale = sanitize_scale(layout.scale);
    let th = if system_dark() { DARK } else { LIGHT };

    unsafe {
        let hinst: HINSTANCE = match GetModuleHandleW(None) {
            Ok(m) => m.into(),
            Err(e) => {
                crate::log!("设置窗口：GetModuleHandleW 失败 {e}");
                return;
            }
        };
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(main_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: MAIN_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            crate::log!("设置窗口：注册窗口类失败");
            return;
        }
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let title = sys::wide("kmlink");
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            MAIN_CLASS,
            PCWSTR(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            820,
            600,
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
        let dpi = match GetDpiForWindow(hwnd) {
            0 => 96,
            d => d,
        };
        let s = dpi as f32 / 96.0;
        apply_caption(hwnd, &th);
        let icons = [app_icon((16.0 * s).round() as i32), app_icon((32.0 * s).round() as i32)];
        for (i, ic) in icons.iter().enumerate() {
            if let Some(ic) = ic {
                SendMessageW(hwnd, WM_SETICON, Some(WPARAM(i)), Some(LPARAM(ic.0 as isize)));
            }
        }

        // 输入框：borderless EDIT，平时隐藏
        let mut edits = [HWND::default(); 3];
        for (i, id) in EDIT_IDS.iter().enumerate() {
            match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                w!(""),
                WS_CHILD | WINDOW_STYLE(ES_AUTOHSCROLL),
                0,
                0,
                10,
                10,
                Some(hwnd),
                Some(HMENU(*id as isize as *mut _)),
                Some(hinst),
                None,
            ) {
                Ok(h) => {
                    edits[i] = h;
                    let limit = [200usize, 128, NAME_MAX][i];
                    SendMessageW(h, EM_SETLIMITTEXT, Some(WPARAM(limit)), None);
                    SendMessageW(h, EM_SETMARGINS, Some(WPARAM(3)), Some(LPARAM(0)));
                }
                Err(e) => crate::log!("设置窗口：输入框创建失败 {e}"),
            }
        }

        let fonts = Fonts::new(s);
        for (i, h) in edits.iter().enumerate() {
            let f = [&fonts.input, &fonts.mono, &fonts.name][i];
            SendMessageW(*h, WM_SETFONT, Some(WPARAM(f.h.0 as usize)), Some(LPARAM(0)));
        }
        let mine = local_monitors();
        // 开机自启以注册表为准（主程序没在跑时 config 里的值可能没生效）
        let sw = App::sw_from(&cfg, sys::autostart_get().is_some());
        let audio = audio_seg_index(&cfg);
        // 第一次（还没填对方 IP）先到连接页；KMLINK_PAGE=mouse|kbd|audio|clip|general|conn 调试截图用
        let first = if cfg.peer.trim().is_empty() { Page::Conn } else { Page::Mouse };
        let page = std::env::var("KMLINK_PAGE").ok().and_then(|v| Page::from_env(&v)).unwrap_or(first);
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                hwnd,
                s,
                th,
                fonts,
                surf: None,
                mdc: CreateCompatibleDC(None),
                edits,
                edit_pos: [None; 3],
                brush_fill: CreateSolidBrush(th.fill.colorref()),
                brush_card: CreateSolidBrush(th.card.colorref()),
                cfg_mtime: cfg_mtime(),
                cfg,
                mine,
                peer: placeholder_peer(),
                peer_file_name: String::new(),
                peer_known: false,
                connected: false,
                status_name: String::new(),
                rename_pending: None,
                layout,
                drag: None,
                drop: None,
                hover: Hot::None,
                press: Hot::None,
                tracking: false,
                edit: None,
                edit_text: String::new(),
                edit_orig: String::new(),
                active: false,
                draft_ip: None,
                draft_key: None,
                err_ip: None,
                err_key: None,
                err_name: false,
                sw,
                sw_t: [None; SW_N],
                page,
                page_from: page.nav_top(),
                page_t: None,
                slide: None,
                paused: false,
                pause_click: None,
                audio,
                audio_from: audio as f32,
                audio_t: None,
                flash: None,
                copied: None,
                popover: false,
                ips: lan_ips(),
                frozen: false,
                upd: config::StatusFile::default(),
                daemon: false,
                upd_click: None,
                spin0: Instant::now(),
            })
        });
        poll();

        // 窗口大小：客户区 820 x 568 按 DPI 缩放，居中到所在显示器的工作区
        let mut rc = RECT { left: 0, top: 0, right: scale_px(CLIENT_W, s), bottom: scale_px(CLIENT_H, s) };
        let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), dpi);
        let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let (mut x, mut y) = (100, 100);
        if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            let wa = mi.rcWork;
            x = wa.left + ((wa.right - wa.left - ww) / 2).max(0);
            y = wa.top + ((wa.bottom - wa.top - wh) / 2).max(0);
        }
        let _ = SetWindowPos(hwnd, None, x, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
        SetTimer(Some(hwnd), TIMER_POLL, 500, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        debug_state();
        refresh();

        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            if msg.message == WM_KEYDOWN && on_key(msg.hwnd, msg.wParam.0 as u16) {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if let Some(app) = APP.with(|a| a.borrow_mut().take()) {
            let _ = DeleteDC(app.mdc);
            let _ = DeleteObject(app.brush_fill.into());
            let _ = DeleteObject(app.brush_card.into());
        }
        for ic in icons.into_iter().flatten() {
            let _ = DestroyIcon(ic);
        }
    }
}

/// KMLINK_UI：直接呈现某个状态（调试截图用）。
fn debug_state() {
    let Ok(v) = std::env::var("KMLINK_UI") else { return };
    // 这些状态在鼠标页（舞台），换新密钥确认框在连接页
    let page = if v == "popover" { Page::Conn } else { Page::Mouse };
    with_app(|a| {
        a.page = page;
        a.page_from = page.nav_top();
    });
    match v.as_str() {
        "hover" => {
            with_app(|a| {
                a.hover = Hot::NameLocal;
                a.frozen = true;
            });
        }
        "offhover" => {
            with_app(|a| {
                a.hover = Hot::NamePeerOff;
                a.frozen = true;
            });
        }
        "popover" => {
            with_app(|a| a.popover = true);
        }
        "drag" => {
            with_app(|a| {
                let g = a.layout_pos(&Layout { side: Side::Left, offset: 0, scale: a.layout.scale });
                let view = a.fit_view(a.layout_pos(&a.layout));
                let pos = (g.0 + 190.0, g.1 - 130.0);
                // m0 = NaN：鼠标移动不改它（截图期间保持这个画面）
                a.drag = Some(Drag { m0: (f32::NAN, f32::NAN), p0: pos, pos, view, active: true });
            });
        }
        "edit" => {
            activate(Field::LocalName);
            if let Some(h) = with_app(|a| a.edits[2]) {
                set_text(h, "书房台式");
                unsafe {
                    SendMessageW(h, EM_SETSEL, Some(WPARAM(4)), Some(LPARAM(4)));
                }
            }
        }
        _ => {}
    }
}

/// 输入框里的按键：回车提交、Esc 取消、Tab 换框、Ctrl+A 全选。处理了返回 true（不再分发）。
fn on_key(target: HWND, vk: u16) -> bool {
    let Some((main, edits, editing, popover)) = with_app(|a| (a.hwnd, a.edits, a.edit, a.popover)) else { return false };
    let slot = edits.iter().position(|h| *h == target);
    if slot.is_none() {
        if target == main {
            if vk == VK_ESCAPE.0 && popover {
                with_app(|a| a.popover = false);
                refresh();
                return true;
            }
            if vk == VK_TAB.0 {
                activate(Field::Ip);
                return true;
            }
        }
        return false;
    }
    let Some(f) = editing else { return false };
    unsafe {
        if vk == VK_RETURN.0 {
            finish(f, true, true);
            return true;
        }
        if vk == VK_ESCAPE.0 {
            finish(f, false, false);
            return true;
        }
        if vk == VK_TAB.0 {
            match f {
                // 只有两个框：Tab / Shift+Tab 都是在两者之间来回
                Field::Ip => activate(Field::Key),
                Field::Key => activate(Field::Ip),
                _ => finish(f, true, false),
            }
            return true;
        }
        if vk == b'A' as u16 && GetKeyState(VK_CONTROL.0 as i32) < 0 {
            SendMessageW(target, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            return true;
        }
    }
    false
}

/// 打开某个输入框（先把正在编辑的那个提交掉）。
fn activate(f: Field) {
    if let Some(cur) = with_app(|a| a.edit).flatten() {
        if cur == f {
            return;
        }
        finish(cur, true, false);
    }
    let Some(Some((h, text))) = with_app(|a| {
        if f == Field::PeerName && !a.connected {
            return None;
        }
        a.popover = false;
        let text = match f {
            Field::Ip => a.draft_ip.clone().unwrap_or_else(|| a.cfg.peer.trim().to_string()),
            Field::Key => a.draft_key.clone().unwrap_or_else(|| a.cfg.key.clone()),
            Field::LocalName => a.local_name(),
            Field::PeerName => {
                let n = a.peer_name();
                if a.peer_known || !a.status_name.is_empty() { n } else { String::new() }
            }
        };
        a.edit = Some(f);
        a.err_name = false;
        a.edit_text = text.clone();
        a.edit_orig = text.clone();
        Some((a.edits[f.slot()], text))
    }) else {
        return;
    };
    set_text(h, &text);
    refresh();
    unsafe {
        let _ = SetFocus(Some(h));
        SendMessageW(h, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
}

/// 写配置（加锁读-改-写，只动自己的字段）。
fn write_cfg(f: impl FnOnce(&mut Config)) -> bool {
    let Some(fallback) = with_app(|a| a.cfg.clone()) else { return false };
    match config::update(&fallback, f) {
        Ok(c) => {
            with_app(|a| a.cfg = c);
            true
        }
        Err(e) => {
            crate::log!("设置窗口：保存失败 {e}");
            false
        }
    }
}

enum Outcome {
    Keep,
    Done,
    SetPeer(String),
    SetKey(String),
    SetName(String),
    Rename(String),
}

/// 结束编辑。commit：提交（否则取消）；enter：回车提交（不对就留在框里改）。
fn finish(f: Field, commit: bool, enter: bool) {
    let Some((h, main, ips)) = with_app(|a| (a.edits[f.slot()], a.hwnd, a.ips.clone())) else { return };
    let text = get_text(h);
    let Some(out) = with_app(|a| {
        if a.edit != Some(f) {
            return None;
        }
        // 没改过的框提交时什么都不写（不合法的草稿照旧留着红字）
        let unchanged = edit_unchanged(f, &text, &a.edit_orig);
        let out = if !commit {
            match f {
                Field::Ip => {
                    a.draft_ip = None;
                    a.err_ip = None;
                }
                Field::Key => {
                    a.draft_key = None;
                    a.err_key = None;
                }
                _ => {}
            }
            Outcome::Done
        } else {
            match f {
                Field::Ip => {
                    let bad = if peer_ip(&text).is_err() {
                        Some("格式不对")
                    } else if is_own_addr(&text, &ips) {
                        Some("这是本机地址")
                    } else {
                        None
                    };
                    match bad {
                        None => {
                            a.draft_ip = None;
                            a.err_ip = None;
                            if unchanged { Outcome::Done } else { Outcome::SetPeer(text.trim().to_string()) }
                        }
                        Some(e) => {
                            a.err_ip = Some(e);
                            if enter {
                                Outcome::Keep
                            } else {
                                a.draft_ip = Some(text.trim().to_string());
                                Outcome::Done
                            }
                        }
                    }
                }
                Field::Key => {
                    let k: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                    if config::parse_key(&k).is_some() {
                        a.draft_key = None;
                        a.err_key = None;
                        if unchanged { Outcome::Done } else { Outcome::SetKey(k.to_ascii_lowercase()) }
                    } else {
                        a.err_key = Some("须为 64 位十六进制");
                        if enter {
                            Outcome::Keep
                        } else {
                            a.draft_key = Some(k);
                            Outcome::Done
                        }
                    }
                }
                Field::LocalName | Field::PeerName => match clean_name(&text) {
                    Some(n) if f == Field::LocalName => {
                        if unchanged || n == a.local_name() {
                            Outcome::Done
                        } else {
                            Outcome::SetName(n)
                        }
                    }
                    Some(n) => {
                        if !a.connected || unchanged || n == a.peer_name() {
                            Outcome::Done
                        } else {
                            Outcome::Rename(n)
                        }
                    }
                    None if enter => {
                        a.err_name = true;
                        Outcome::Keep
                    }
                    None => Outcome::Done,
                },
            }
        };
        if !matches!(out, Outcome::Keep) {
            a.edit = None;
            a.err_name = false;
        }
        Some(out)
    })
    .flatten() else {
        return;
    };
    match out {
        Outcome::Keep => {
            refresh();
            return;
        }
        Outcome::Done => {}
        Outcome::SetPeer(p) => {
            let same = with_app(|a| a.cfg.peer.trim() == p).unwrap_or(false);
            if !same {
                write_cfg(|c| c.peer = p);
            }
        }
        Outcome::SetKey(k) => {
            write_cfg(|c| c.key = k);
        }
        Outcome::SetName(n) => {
            if write_cfg(|c| c.name = n) {
                with_app(|a| a.flash = Some((Field::LocalName, Instant::now())));
            }
        }
        Outcome::Rename(n) => {
            if write_rename_peer(&n) {
                with_app(|a| {
                    a.rename_pending = Some((n, Instant::now()));
                    a.flash = Some((Field::PeerName, Instant::now()));
                });
            }
        }
    }
    // 先把焦点交回主窗口（此时 a.edit 已清空，EN_KILLFOCUS 不会再提交一次），再隐藏
    unsafe {
        if windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() == h {
            let _ = SetFocus(Some(main));
        }
    }
    refresh();
}

/// 每 500ms：连接状态、对方屏幕、配置文件。
fn poll() {
    let (connected, status_name) = read_status();
    let mut daemon = sys::daemon_running();
    let mut upd = if daemon { config::load_status().unwrap_or_default() } else { config::StatusFile::default() };
    // 调试截图：KMLINK_UPD=状态[:版本或进度或原因]，如 avail:0.2.0、solo:0.2.0、down:45、fail:连不上 GitHub
    if let Ok(v) = std::env::var("KMLINK_UPD") {
        let (k, arg) = v.split_once(':').unwrap_or((v.as_str(), ""));
        daemon = true;
        upd.app = "0.1.1".into();
        upd.peer_app = "0.1.0".into();
        upd.peer_upd = k != "solo";
        upd.upd = if k == "solo" {
            "avail".into()
        } else if k == "idle" {
            String::new()
        } else {
            k.into()
        };
        match k {
            "down" => upd.upd_pct = arg.parse().unwrap_or(0),
            "fail" => upd.upd_err = arg.into(),
            _ => upd.upd_ver = arg.into(),
        }
    }
    let peer = read_peer();
    let mt = cfg_mtime();
    // 拖动 / 落位动画中先不重新读入（也不记下新的修改时间），结束后第一次轮询再读，免得丢掉对方改的排布
    let reload = with_app(|a| a.cfg_mtime != mt && a.drag.is_none() && a.drop.is_none()).unwrap_or(false);
    let new_cfg = if reload { Config::load() } else { None };
    let r = with_app(|a| {
        let before = (a.connected, a.status_name.clone(), a.peer_file_name.clone(), a.peer.clone(), a.rename_pending.is_some());
        let upd_before = (a.daemon, a.upd.clone(), a.upd_view());
        a.connected = connected;
        a.status_name = status_name;
        a.daemon = daemon;
        let was_paused = a.paused;
        a.paused = daemon && upd.paused;
        if a.pause_click.is_some_and(|(v, t)| v == a.paused || t.elapsed() > Duration::from_secs(3)) {
            a.pause_click = None;
        }
        let pause_changed = was_paused != a.paused;
        if upd.upd != a.upd.upd {
            a.upd_click = None; // 主程序已经接手（状态变了）
        }
        a.upd = upd;
        let upd_changed = upd_before != (a.daemon, a.upd.clone(), a.upd_view());
        let busy = a.drag.is_some() || a.drop.is_some();
        if !busy {
            match peer {
                Some((n, ms)) => {
                    a.peer_file_name = n;
                    a.peer = ms;
                    a.peer_known = true;
                }
                None => {
                    a.peer_known = false;
                    a.peer = placeholder_peer();
                }
            }
        }
        if let Some((n, t0)) = &a.rename_pending
            && (t0.elapsed() > Duration::from_secs(10) || (a.connected && a.status_name == *n) || a.peer_file_name == *n)
        {
            a.rename_pending = None;
        }
        if reload {
            a.cfg_mtime = mt;
            if let Some(c) = new_cfg {
                let sw = App::sw_from(&c, c.autostart);
                a.set_audio(audio_seg_index(&c));
                for (i, s) in [Sw::Mouse, Sw::Kbd, Sw::Audio, Sw::Clip, Sw::Follow, Sw::Notify, Sw::Autostart].into_iter().enumerate() {
                    a.set_sw(s, sw[i]);
                }
                // 正在拖的滑杆不被重新读入盖掉
                let (speed, vol) = (a.cfg.input.speed, a.cfg.audio.volume);
                let slide = a.slide;
                // reload 只在不拖动时发生
                let mut l = c.layout.layout;
                l.scale = sanitize_scale(l.scale);
                a.layout = l;
                a.cfg = c;
                match slide {
                    Some(Hot::Speed) => a.cfg.input.speed = speed,
                    Some(Hot::Volume) => a.cfg.audio.volume = vol,
                    _ => {}
                }
            }
        }
        let after = (a.connected, a.status_name.clone(), a.peer_file_name.clone(), a.peer.clone(), a.rename_pending.is_some());
        (a.edit == Some(Field::PeerName) && !a.connected, reload || before != after || upd_changed || pause_changed)
    });
    let Some((cancel, changed)) = r else { return };
    if cancel {
        finish(Field::PeerName, false, false);
    }
    if changed {
        refresh();
        ensure_anim();
    }
}

fn write_layout(l: Layout) {
    write_cfg(|c| apply_layout(c, &l));
}

fn on_scale(steps: i32) {
    let l = with_app(|a| {
        let mut l = a.layout;
        l.scale = step_scale(l.scale, steps);
        let pb = a.peer_desk().bounds();
        let (pw, ph) = (pb.w() as f64 * l.scale, pb.h() as f64 * l.scale);
        l.offset = clamp_offset(a.mine_desk().bounds(), l.side, pw, ph, l.offset);
        a.layout = l;
        l
    });
    if let Some(l) = l {
        write_layout(l);
    }
    refresh();
}

/// 复制文字到剪贴板（不进 Win+V 历史、不上云剪贴板）。先分配好内存再打开剪贴板；
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
                std::thread::sleep(Duration::from_millis(20));
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

fn mouse_xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xFFFF) as u16 as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32)
}

/// 改声音模式：新 rev（rev_mode 跟着记），主程序重新读配置后发给对方，对方换成它的视角。
fn set_audio_mode(m: config::AudioMode) {
    write_cfg(|c| {
        c.audio.set_mode(m);
        c.audio.rev = config::next_rev(c.audio.rev);
        c.audio.rev_mode = Some(m);
    });
}

/// 拖滑杆：内存里先改（立刻重画），松手时写配置（commit）。
fn on_slide(h: Hot, x: f32, commit: bool) {
    let frac = slider_frac(x);
    let v = with_app(|a| {
        match h {
            Hot::Speed => a.cfg.input.speed = speed_at(frac),
            Hot::Volume => a.cfg.audio.volume = volume_at(frac),
            _ => {}
        }
        (a.cfg.input.speed, a.cfg.audio.volume)
    });
    if let (Some((speed, vol)), true) = (v, commit) {
        match h {
            Hot::Speed => {
                write_cfg(|c| c.input.speed = speed);
            }
            Hot::Volume => {
                write_cfg(|c| c.audio.volume = vol);
            }
            _ => {}
        }
    }
    refresh();
}

fn on_click(hot: Hot) {
    let Some(main) = with_app(|a| a.hwnd) else { return };
    match hot {
        Hot::Switch(s) => {
            let v = with_app(|a| {
                let v = !a.is_on(s);
                a.set_sw(s, v);
                (v, config::audio_slot_mode(a.audio + 1, a.cfg.me_one()))
            });
            if let Some((v, on_mode)) = v {
                match s {
                    // 鼠标 / 键盘：两台同步（新 rev，主程序重新读配置后发给对方）
                    Sw::Mouse => {
                        write_cfg(|c| {
                            c.share.mouse = v;
                            c.share.rev = config::next_rev(c.share.rev);
                        });
                    }
                    Sw::Kbd => {
                        write_cfg(|c| {
                            c.share.keyboard = v;
                            c.share.rev = config::next_rev(c.share.rev);
                        });
                    }
                    Sw::Audio => {
                        let m = if v { on_mode } else { config::AudioMode::Off };
                        set_audio_mode(m);
                    }
                    Sw::Clip => {
                        write_cfg(|c| c.clipboard.enabled = v);
                    }
                    Sw::Follow => {
                        write_cfg(|c| c.input.kbd_follow = v);
                    }
                    Sw::Notify => {
                        write_cfg(|c| c.clipboard.notify = v);
                    }
                    Sw::Autostart => {
                        // 开机自启：直接改注册表（主程序没在跑也要生效），开关和配置都按实际结果来
                        sys::autostart_set(v);
                        let actual = sys::autostart_get().is_some();
                        if actual != v {
                            with_app(|a| a.set_sw(Sw::Autostart, actual));
                        }
                        write_cfg(|c| c.autostart = actual);
                    }
                }
            }
        }
        Hot::Nav(p) => {
            with_app(|a| a.go_page(p));
            // 换页：正在编辑的输入框不在新页上
            if let Some(Some(f)) = with_app(|a| a.edit) {
                finish(f, true, false);
            }
        }
        Hot::Audio(k) => {
            let r = with_app(|a| {
                let changed = a.audio != k;
                a.set_audio(k);
                (changed, a.is_on(Sw::Audio), config::audio_slot_mode(k + 1, a.cfg.me_one()))
            });
            match r {
                Some((true, true, m)) => set_audio_mode(m),
                // 声音关着：只记下打开时用哪档
                Some((true, false, m)) => {
                    write_cfg(|c| c.audio.last = m);
                }
                _ => {}
            }
        }
        Hot::Pause => {
            let v = with_app(|a| {
                let v = !a.paused_now();
                a.pause_click = Some((v, Instant::now()));
                v
            });
            if let Some(v) = v {
                let cmd = if v { "pause" } else { "resume" };
                if let Err(e) = write_atomic("update_cmd.json", &config::update_cmd_json(cmd, config::now_ms())) {
                    crate::log!("设置窗口：写 update_cmd.json 失败 {e}");
                }
            }
        }
        Hot::Upd => {
            let cmd = with_app(|a| match a.upd_view() {
                UpdView::Avail(_) | UpdView::AvailSolo(_) => "update",
                _ => {
                    a.upd_click = Some(Instant::now());
                    "check"
                }
            });
            if let Some(cmd) = cmd {
                if let Err(e) = write_atomic("update_cmd.json", &config::update_cmd_json(cmd, config::now_ms())) {
                    crate::log!("设置窗口：写 update_cmd.json 失败 {e}");
                }
                refresh();
                ensure_anim();
            }
        }
        Hot::Minus => on_scale(-1),
        Hot::Plus => on_scale(1),
        Hot::CopyKey | Hot::CopyIp => {
            let t = with_app(|a| if hot == Hot::CopyKey { a.cfg.key.clone() } else { a.ips.first().cloned().unwrap_or_default() });
            if let Some(t) = t
                && !t.is_empty()
            {
                if copy_text(main, &t) {
                    with_app(|a| a.copied = Some((hot, Instant::now())));
                } else {
                    crate::log!("设置窗口：复制到剪贴板失败");
                }
            }
        }
        Hot::NewKey => {
            with_app(|a| a.popover = !a.popover);
        }
        Hot::PopCancel => {
            with_app(|a| a.popover = false);
        }
        Hot::PopOk => {
            with_app(|a| {
                a.popover = false;
                a.draft_key = None;
                a.err_key = None;
            });
            write_cfg(|c| c.key = config::new_key_hex());
        }
        _ => {}
    }
    refresh();
}

fn on_down(hwnd: HWND, x: f32, y: f32) {
    let Some((hot, editing, popover)) = with_app(|a| {
        // 每次按下都从头算：上次在窗口外松手留下的 press 不能带到这次
        a.press = Hot::None;
        let sc = scene(a);
        (hit(a, &sc, x, y), a.edit, a.popover)
    }) else {
        return;
    };
    // 点到别处：先提交正在编辑的
    if let Some(f) = editing {
        let same = matches!((f, hot), (Field::Ip, Hot::Ip) | (Field::Key, Hot::Key));
        if !same {
            finish(f, true, false);
        }
    }
    unsafe {
        if windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() != hwnd && !matches!(hot, Hot::Ip | Hot::Key) {
            let _ = SetFocus(Some(hwnd));
        }
    }
    if popover && !matches!(hot, Hot::PopOk | Hot::PopCancel | Hot::Pop | Hot::NewKey) {
        with_app(|a| a.popover = false);
        refresh();
        return;
    }
    match hot {
        Hot::Peer | Hot::NamePeerOff => {
            let ok = with_app(|a| {
                let p = a.layout_pos(&a.layout);
                let view = a.fit_view(p);
                a.drag = Some(Drag { m0: (x, y), p0: p, pos: p, view, active: false });
            });
            if ok.is_some() {
                unsafe {
                    SetCapture(hwnd);
                }
            }
        }
        Hot::Speed | Hot::Volume => {
            if with_app(|a| a.slide = Some(hot)).is_some() {
                unsafe {
                    SetCapture(hwnd);
                }
                on_slide(hot, x, false);
            }
            return;
        }
        Hot::Ip => activate(Field::Ip),
        Hot::Key => activate(Field::Key),
        Hot::NameLocal => activate(Field::LocalName),
        Hot::NamePeer => activate(Field::PeerName),
        Hot::None | Hot::Pop => {}
        h => {
            // 捕获鼠标：在窗口外松手也能收到 WM_LBUTTONUP，把 press 清掉
            if with_app(|a| a.press = h).is_some() {
                unsafe {
                    SetCapture(hwnd);
                }
            }
        }
    }
    refresh();
}

fn on_move(hwnd: HWND, x: f32, y: f32) {
    if let Some(Some(h)) = with_app(|a| a.slide) {
        on_slide(h, x, false);
        return;
    }
    let track = with_app(|a| {
        if let Some(d) = a.drag.as_mut() {
            if d.m0.0.is_nan() {
                return (false, true);
            }
            let (dx, dy) = (x - d.m0.0, y - d.m0.1);
            if !d.active && dx.abs().max(dy.abs()) < 3.0 {
                return (false, true);
            }
            d.active = true;
            let k = d.view.k;
            let mut pos = (d.p0.0 + dx as f64 / k, d.p0.1 + dy as f64 / k);
            // 别拖出舞台：对方整组留在舞台里（不盖住下面的说明行）；比舞台还大就居中
            let (pw, ph) = a.peer_wh();
            let v = a.drag.as_ref().map(|d| d.view).unwrap_or(View { k: 1.0, ox: 0.0, oy: 0.0 });
            let keep = |p: f64, len: f64, lo: f64, hi: f64, o: f64| {
                let x = o + p * v.k;
                let w = len * v.k;
                let nx = if w >= hi - lo { lo + (hi - lo - w) / 2.0 } else { x.clamp(lo, hi - w) };
                p + (nx - x) / v.k
            };
            pos.0 = keep(pos.0, pw, STAGE.x as f64 + 8.0, (STAGE.x + STAGE.w) as f64 - 8.0, v.ox);
            pos.1 = keep(pos.1, ph, STAGE.y as f64 + 8.0, AREA.y as f64 + AREA.h as f64 + 4.0, v.oy);
            if let Some(d) = a.drag.as_mut() {
                d.pos = pos;
            }
            a.hover = Hot::None;
            return (true, true);
        }
        if a.frozen {
            return (false, true);
        }
        let sc = scene(a);
        let h = hit(a, &sc, x, y);
        let changed = h != a.hover;
        a.hover = h;
        let need_track = !a.tracking;
        a.tracking = true;
        (changed, !need_track)
    });
    let Some((changed, tracked)) = track else { return };
    if !tracked {
        let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
        unsafe {
            let _ = TrackMouseEvent(&mut t);
        }
    }
    if changed {
        refresh();
    }
}

fn on_up(x: f32, y: f32) {
    if let Some(Some(h)) = with_app(|a| a.slide.take()) {
        unsafe {
            let _ = ReleaseCapture();
        }
        on_slide(h, x, true);
        return;
    }
    let r = with_app(|a| {
        if let Some(d) = a.drag.take() {
            if d.active {
                let (pw, ph) = a.peer_wh();
                let (side, offset) = snap(a.mine_desk().bounds(), pw, ph, d.pos.0, d.pos.1);
                a.layout.side = side;
                a.layout.offset = offset;
                a.drop = Some(DropAnim { from: d.pos, view0: d.view, t0: Instant::now() });
            }
            return (true, Hot::None, false);
        }
        let sc = scene(a);
        let h = hit(a, &sc, x, y);
        let pressed = std::mem::replace(&mut a.press, Hot::None);
        (false, if pressed == h { h } else { Hot::None }, pressed != Hot::None)
    });
    match r {
        Some((true, _, _)) => unsafe {
            let _ = ReleaseCapture();
            refresh();
        },
        Some((false, h, captured)) => {
            if captured {
                unsafe {
                    let _ = ReleaseCapture();
                }
            }
            if h != Hot::None {
                on_click(h);
            }
        }
        _ => {}
    }
}

fn on_timer(hwnd: HWND, id: usize) {
    if id == TIMER_POLL {
        poll();
        return;
    }
    let r = with_app(|a| {
        let mut done = None;
        if let Some(d) = &a.drop
            && ms_since(d.t0) >= DROP_MS
        {
            a.drop = None;
            done = Some(a.layout);
        }
        if a.flash.is_some_and(|(_, t)| ms_since(t) > FLASH_MS + FADE_MS) {
            a.flash = None;
        }
        (done, a.animating())
    });
    let Some((done, anim)) = r else { return };
    if let Some(l) = done {
        write_layout(l);
        sync_edits();
    }
    if !anim {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_ANIM);
        }
    }
    invalidate(hwnd);
}

fn on_paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        if w > 0 && h > 0 && !hdc.is_invalid() {
            with_app(|a| {
                let mut surf = match a.surf.take() {
                    Some(s) if s.w == w && s.h == h => Some(s),
                    _ => Surface::new(w, h),
                };
                if let Some(sf) = surf.as_mut() {
                    let sc = scene(a);
                    let mut p = Painter::new(sf, a.s);
                    render(&mut p, a, &sc);
                    sf.blit(hdc);
                }
                a.surf = surf;
            });
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 主题变了：重读、换标题栏颜色和输入框底色。
fn on_theme(hwnd: HWND) {
    let th = if system_dark() { DARK } else { LIGHT };
    let old = with_app(|a| {
        if a.th.dark == th.dark {
            return None;
        }
        a.th = th;
        let old = (a.brush_fill, a.brush_card);
        unsafe {
            a.brush_fill = CreateSolidBrush(th.fill.colorref());
            a.brush_card = CreateSolidBrush(th.card.colorref());
        }
        Some((old, a.edits))
    })
    .flatten();
    if let Some(((b1, b2), edits)) = old {
        apply_caption(hwnd, &th);
        unsafe {
            for e in edits {
                let _ = InvalidateRect(Some(e), None, true);
            }
            let _ = DeleteObject(b1.into());
            let _ = DeleteObject(b2.into());
        }
        refresh();
    }
}

fn on_dpi(hwnd: HWND, dpi: u32, suggested: *const RECT) {
    let s = dpi as f32 / 96.0;
    let fonts = Fonts::new(s);
    let edits = with_app(|a| {
        a.s = s;
        a.edit_pos = [None; 3];
        let old = std::mem::replace(&mut a.fonts, fonts);
        let e = a.edits;
        let hf = [a.fonts.input.h, a.fonts.mono.h, a.fonts.name.h];
        (e, hf, old)
    });
    if let Some((edits, hf, old)) = edits {
        unsafe {
            for (e, f) in edits.iter().zip(hf) {
                SendMessageW(*e, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
            }
        }
        drop(old); // 控件已换上新字体，旧的可以删了
    }
    unsafe {
        if !suggested.is_null() {
            let r = *suggested;
            let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
    refresh();
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let s = with_app(|a| a.s).unwrap_or(1.0);
        let logical = |lp: LPARAM| {
            let (x, y) = mouse_xy(lp);
            (x as f32 / s, y as f32 / s)
        };
        match msg {
            WM_PAINT => {
                on_paint(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                if wp.0 != SIZE_MINIMIZED {
                    invalidate(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = logical(lp);
                on_move(hwnd, x, y);
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                let changed = with_app(|a| {
                    a.tracking = false;
                    std::mem::replace(&mut a.hover, Hot::None) != Hot::None
                });
                if changed == Some(true) {
                    refresh();
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = logical(lp);
                on_down(hwnd, x, y);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = logical(lp);
                on_up(x, y);
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                // 捕获被意外拿走：放弃这次拖动 / 按下（on_up 自己放捕获时两者都已清空）
                // 拖滑杆时被拿走：停在当前值（已在内存里，写进配置）
                if let Some(Some(h)) = with_app(|a| a.slide.take()) {
                    let x = with_app(|a| match h {
                        Hot::Speed => SLIDER_X + SLIDER_W * speed_frac(a.cfg.input.speed),
                        _ => SLIDER_X + SLIDER_W * a.cfg.audio.volume,
                    });
                    if let Some(x) = x {
                        on_slide(h, x, true);
                    }
                }
                if with_app(|a| {
                    let pressed = std::mem::replace(&mut a.press, Hot::None) != Hot::None;
                    a.drag.take().is_some() || pressed
                })
                .unwrap_or(false)
                {
                    refresh();
                }
                LRESULT(0)
            }
            // 光标在子窗口（输入框）上：返回 FALSE 让 EDIT 自己设成 I 形光标，别用父窗口的 hover 盖掉
            WM_SETCURSOR if HWND(wp.0 as *mut _) != hwnd => LRESULT(0),
            WM_SETCURSOR if (lp.0 as usize & 0xFFFF) == HTCLIENT => {
                let c = with_app(|a| {
                    if a.drag.as_ref().is_some_and(|d| d.active) {
                        return IDC_SIZEALL;
                    }
                    match a.hover {
                        Hot::NameLocal | Hot::NamePeer | Hot::Upd => IDC_HAND,
                        Hot::Peer => IDC_SIZEALL,
                        Hot::Ip | Hot::Key => IDC_IBEAM,
                        _ => IDC_ARROW,
                    }
                })
                .unwrap_or(IDC_ARROW);
                let _ = SetCursor(LoadCursorW(None, c).ok());
                LRESULT(1)
            }
            WM_TIMER => {
                on_timer(hwnd, wp.0);
                LRESULT(0)
            }
            WM_ACTIVATE => {
                let on = (wp.0 & 0xFFFF) as u32 != WA_INACTIVE;
                // 先记下前后台（随后的 EN_KILLFOCUS 要看）
                let edit = with_app(|a| {
                    a.active = on;
                    a.edit.filter(|_| on).map(|f| a.edits[f.slot()])
                })
                .flatten();
                // 默认处理会把焦点给主窗口；之后若还有开着的输入框（切走时留下的草稿），把焦点还给它
                let r = DefWindowProcW(hwnd, msg, wp, lp);
                if let Some(h) = edit {
                    let _ = SetFocus(Some(h));
                }
                r
            }
            WM_COMMAND => {
                let id = (wp.0 & 0xFFFF) as i32;
                let code = ((wp.0 >> 16) & 0xFFFF) as u32;
                if let Some(slot) = EDIT_IDS.iter().position(|i| *i == id) {
                    let ctl = HWND(lp.0 as *mut _);
                    if code == EN_CHANGE {
                        let text = get_text(ctl);
                        let r = with_app(|a| {
                            let f = a.edit.filter(|f| f.slot() == slot)?;
                            match f {
                                Field::Ip => a.err_ip = None,
                                Field::Key => a.err_key = None,
                                _ => {
                                    a.err_name = false;
                                    a.edit_text = text;
                                }
                            }
                            Some(())
                        });
                        if r.flatten().is_some() {
                            refresh();
                        }
                    } else if code == EN_KILLFOCUS
                        && let Some((Some(f), active)) = with_app(|a| (a.edit.filter(|f| f.slot() == slot), a.active))
                    {
                        // 窗口被切走（Alt+Tab、最小化）引起的失焦：不提交，框和草稿留着，切回来接着改。
                        // 只有焦点在窗口内移走才提交（回车另走 on_key）。
                        if active && GetForegroundWindow() == hwnd {
                            finish(f, true, false);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_CTLCOLOREDIT => {
                let hdc = HDC(wp.0 as *mut _);
                let ctl = HWND(lp.0 as *mut _);
                match with_app(|a| {
                    let name = ctl == a.edits[2];
                    (if name { a.th.card } else { a.th.fill }, a.th.fg, if name { a.brush_card } else { a.brush_fill })
                }) {
                    Some((bg, fg, br)) => {
                        SetTextColor(hdc, fg.colorref());
                        SetBkColor(hdc, bg.colorref());
                        LRESULT(br.0 as isize)
                    }
                    None => DefWindowProcW(hwnd, msg, wp, lp),
                }
            }
            WM_DPICHANGED => {
                on_dpi(hwnd, (wp.0 & 0xFFFF) as u32, lp.0 as *const RECT);
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                let p = lp.0 as *const u16;
                if !p.is_null() && PCWSTR(p).to_string().is_ok_and(|s| s == "ImmersiveColorSet") {
                    on_theme(hwnd);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_DISPLAYCHANGE => {
                let mine = local_monitors();
                with_app(|a| a.mine = mine);
                refresh();
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_CLOSE => {
                if let Some(Some(f)) = with_app(|a| a.edit) {
                    finish(f, true, false);
                }
                if let Some((true, l)) = with_app(|a| (a.drop.take().is_some(), a.layout)) {
                    write_layout(l);
                }
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

#[cfg(test)]
mod tests {
    #[test]
    fn audio_segments_hit_and_layout() {
        for k in 0..3 {
            let r = audio_seg(k);
            assert_eq!(audio_hit(r.cx()), k);
            assert_eq!(audio_hit(r.x + 0.5), k);
            assert_eq!(audio_hit(r.x + r.w - 0.5), k);
            assert!(AUDIO_SEG.contains(r.cx(), r.cy()));
        }
        assert_eq!(audio_hit(AUDIO_SEG.x), 0);
        assert_eq!(audio_hit(AUDIO_SEG.x + AUDIO_SEG.w - 0.1), 2);
        // 右边和行里的开关对齐、在卡片第一行里
        let row = row_rect(CARD_Y, 0);
        assert_eq!(AUDIO_SEG.x + AUDIO_SEG.w, row_switch(row).x + row_switch(row).w);
        assert!(AUDIO_SEG.y > row.y && AUDIO_SEG.y + AUDIO_SEG.h < row.y + ROW_H);
        let mut c = Config::default();
        for num in [1, 2] {
            c.num = num;
            for k in 1..4 {
                c.audio.set_mode(config::audio_slot_mode(k, c.me_one()));
                assert_eq!(audio_seg_index(&c), k - 1);
            }
            // 关着：显示上次开着的那档
            c.audio.set_mode(config::AudioMode::Off);
            assert_eq!(audio_seg_index(&c), 2);
        }
    }

    #[test]
    fn page_layout_fits() {
        // 侧栏各项不重叠、都在玻璃板里
        for w in PAGES.windows(2) {
            assert!(nav_rect(w[0]).y + nav_rect(w[0]).h <= nav_rect(w[1]).y);
        }
        for p in PAGES {
            let r = nav_rect(p);
            assert!(SIDEBAR.contains(r.x, r.y) && SIDEBAR.contains(r.x + r.w - 1.0, r.y + r.h - 1.0));
            assert_eq!(Page::from_env(&format!("{p:?}").to_lowercase()), Some(p));
        }
        // 各页内容在窗口里
        assert!(STAGE.y + STAGE.h < SPEED_CARD_Y && SPEED_CARD_Y + 66.0 <= CLIENT_H - 16.0);
        assert!(STRIP.y + STRIP.h < CARD_Y && CARD_Y + 166.0 <= CLIENT_H);
        assert!(AREA.x > STAGE.x && AREA.x + AREA.w < STAGE.x + STAGE.w && AREA.y + AREA.h < STEPPER.y);
        assert!(POP.y > HEAD_SW.y + HEAD_SW.h && POP_OK.x + POP_OK.w <= POP.x + POP.w);
        assert!(KEY_BOX.x + KEY_BOX.w < COPY_KEY.x && NEW_KEY.x + NEW_KEY.w == CTL_R);
        // 滑杆
        assert_eq!(speed_at(0.0), SPEED_MIN);
        assert_eq!(speed_at(1.0), SPEED_MAX);
        assert_eq!(speed_at(speed_frac(1.0)), 1.0);
        assert_eq!(volume_at(0.8), 0.8);
        assert_eq!(slider_frac(SLIDER_X - 50.0), 0.0);
        assert_eq!(slider_frac(SLIDER_X + SLIDER_W + 50.0), 1.0);
        assert_eq!(hotkey_name(0x91), "Scroll Lock");
    }


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
    fn layout_write_only_when_changed() {
        let l0 = Layout { side: Side::Right, offset: 0, scale: 1.0 };
        assert!(!layout_changed(&l0, &l0));
        assert!(layout_changed(&l0, &Layout { offset: 1, ..l0 }));
        assert!(layout_changed(&l0, &Layout { side: Side::Left, ..l0 }));
        assert!(layout_changed(&l0, &Layout { scale: 1.05, ..l0 }));

        // 磁盘上已是这个排布（比如对方刚同步来的）：不动 rev
        let theirs = Layout { side: Side::Left, offset: 200, scale: 1.0 };
        let mut c = Config::default();
        c.layout.layout = theirs;
        c.layout.rev = 123;
        apply_layout(&mut c, &theirs);
        assert_eq!(c.layout.layout, theirs);
        assert_eq!(c.layout.rev, 123);

        // 改了排布：写入，rev 严格大于磁盘上的（即使磁盘 rev 来自时钟快的对方）
        let mine = Layout { side: Side::Bottom, offset: -10, scale: 1.0 };
        let far = config::now_ms() + 10_000_000;
        c.layout.rev = far;
        apply_layout(&mut c, &mine);
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
        assert_eq!(status_text(&Layout { side: Side::Right, offset: 120, scale: 1.0 }), "在本机右侧 · 下移 120 像素");
        assert_eq!(status_text(&Layout { side: Side::Left, offset: 0, scale: 1.0 }), "在本机左侧 · 对齐");
        assert_eq!(status_text(&Layout { side: Side::Left, offset: -30, scale: 1.0 }), "在本机左侧 · 上移 30 像素");
        assert_eq!(status_text(&Layout { side: Side::Top, offset: 5, scale: 1.0 }), "在本机上方 · 右移 5 像素");
        assert_eq!(status_text(&Layout { side: Side::Bottom, offset: -7, scale: 1.0 }), "在本机下方 · 左移 7 像素");
    }

    #[test]
    fn names() {
        assert_eq!(clean_name("  书房台式 ").as_deref(), Some("书房台式"));
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_name(""), None);
        assert_eq!(clean_name(&"字".repeat(16)).as_deref(), Some("字".repeat(16).as_str()));
        assert_eq!(clean_name(&"字".repeat(17)), None);
        assert_eq!(clean_name("a\tb"), None);
    }

    #[test]
    fn unchanged_edit() {
        // 名字：首尾空白不算改
        assert!(edit_unchanged(Field::LocalName, " 书房 ", "书房"));
        assert!(edit_unchanged(Field::PeerName, "WiFi电脑", "WiFi电脑 "));
        assert!(!edit_unchanged(Field::LocalName, "书房2", "书房"));
        // 不合法的名字只和同样的字相等
        assert!(edit_unchanged(Field::PeerName, "  ", ""));
        assert!(!edit_unchanged(Field::LocalName, "", "书房"));
        // IP：去首尾空白
        assert!(edit_unchanged(Field::Ip, " 192.168.1.5 ", "192.168.1.5"));
        assert!(!edit_unchanged(Field::Ip, "192.168.1.6", "192.168.1.5"));
        assert!(edit_unchanged(Field::Ip, "1.2.3", "1.2.3"));
        // 密钥：去所有空白、大小写不敏感
        let k = "982e0241".repeat(8);
        assert!(edit_unchanged(Field::Key, &format!(" {} {} ", &k[..32].to_uppercase(), &k[32..]), &k));
        assert!(!edit_unchanged(Field::Key, &"0".repeat(64), &k));
    }

    #[test]
    fn key_display() {
        let k = "982e0241aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2895a8b";
        assert_eq!(short_key(k), "982e0241…2895a8b");
        assert_eq!(short_key("abc"), "abc");
    }

    #[test]
    fn lan_filter() {
        assert!(usable_lan_ip("192.168.1.8".parse().unwrap()));
        assert!(usable_lan_ip("10.200.96.48".parse().unwrap()));
        assert!(!usable_lan_ip("127.0.0.1".parse().unwrap()));
        assert!(!usable_lan_ip("169.254.3.4".parse().unwrap()));
        assert!(!usable_lan_ip("198.18.0.1".parse().unwrap()));
        assert!(!usable_lan_ip("198.19.255.1".parse().unwrap()));
        assert!(usable_lan_ip("198.20.0.1".parse().unwrap()));
        assert!(!usable_lan_ip("0.0.0.0".parse().unwrap()));
    }

    #[test]
    fn resolution_text() {
        let r = Rect::new(0, 0, 3840, 2160);
        assert_eq!(res_text(&r, Some(144)), "3840×2160 · 150%");
        assert_eq!(res_text(&r, Some(96)), "3840×2160 · 100%");
        assert_eq!(res_text(&r, None), "3840×2160");
    }

    #[test]
    fn peer_file() {
        let (n, ms) = parse_peer(r#"{"name":"WiFi电脑","monitors":[{"l":0,"t":0,"r":2560,"b":1600}],"dpis":[144]}"#).unwrap();
        assert_eq!(n, "WiFi电脑");
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].dpi, Some(144));
        assert!(ms[0].primary);
        // 老版本：没有 dpis；dpis 比显示器少
        let (_, ms) = parse_peer(r#"{"name":"b","monitors":[{"l":-1920,"t":0,"r":0,"b":1080},{"l":0,"t":0,"r":2560,"b":1440}]}"#).unwrap();
        assert_eq!(ms[0].dpi, None);
        assert!(!ms[0].primary && ms[1].primary);
        let (_, ms) = parse_peer(r#"{"name":"b","monitors":[{"l":0,"t":0,"r":10,"b":10},{"l":10,"t":0,"r":20,"b":10}],"dpis":[120]}"#).unwrap();
        assert_eq!((ms[0].dpi, ms[1].dpi), (Some(120), None));
        assert!(parse_peer(r#"{"name":"b","monitors":[]}"#).is_none());
        assert!(parse_peer("garbage").is_none());
    }

    #[test]
    fn status_file() {
        let s = r#"{"pid":1234,"connected":true,"peer_name":"WiFi电脑","peer_addr":"10.0.0.2:47900","paused":false,"t":1}"#;
        assert_eq!(parse_status(s), Some((1234, true, "WiFi电脑".to_string())));
        assert_eq!(parse_status(r#"{"pid":5,"connected":false}"#), Some((5, false, String::new())));
        assert_eq!(parse_status("{}"), None);
        assert!(!pid_alive(0));
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn contacts() {
        // 本机 3840x2160，对方 2560x1600 在左侧下移 416：相接 416..2016
        let mine = [Rect::new(0, 0, 3840, 2160)];
        let peer = [(-2560.0, 416.0, 0.0, 2016.0)];
        let (line, segs) = contact_segments(&mine, &peer, peer[0], Side::Left);
        assert_eq!(line, 0.0);
        assert_eq!(segs, vec![(416.0, 2016.0)]);
        // 本机两块（右边那块矮），对方在右侧：只有右边那块的右边缘算
        let mine = [Rect::new(0, 0, 3840, 2160), Rect::new(3840, 540, 5760, 1620)];
        let peer = [(5760.0, 0.0, 8320.0, 1600.0)];
        let (line, segs) = contact_segments(&mine, &peer, peer[0], Side::Right);
        assert_eq!(line, 5760.0);
        assert_eq!(segs, vec![(540.0, 1600.0)]);
        // 在下方，错开到不相交
        let mine = [Rect::new(0, 0, 100, 100)];
        let peer = [(200.0, 100.0, 300.0, 200.0)];
        assert!(contact_segments(&mine, &peer, peer[0], Side::Bottom).1.is_empty());
    }

    #[test]
    fn adjacency_and_primary() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(100, 20, 200, 80);
        assert_eq!(adjacent(&a, &[a, b]), [false, false, true, false]);
        assert_eq!(adjacent(&b, &[a, b]), [true, false, false, false]);
        let mut ms = vec![Mon { r: b, dpi: None, primary: false }, Mon { r: a, dpi: None, primary: false }];
        fix_primary(&mut ms);
        assert!(!ms[0].primary && ms[1].primary);
    }
}
