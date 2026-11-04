//! desktop 线程：一个隐藏的顶层窗口（WS_EX_TOOLWINDOW，不显示）+ 消息循环。负责：
//!
//! 1. 剪贴板（clip.rs）：AddClipboardFormatListener；WM_CLIPBOARDUPDATE 时若 GetClipboardSequenceNumber()
//!    等于我们自己最后一次写入后的序号 → 忽略；否则 100ms 防抖（SetTimer）后读取：
//!    文字（≤ max_text_kb）→ 与上次发出/收到的文字相同则不发 → hub.send(Msg::ClipText)；
//!    图片（cfg.clipboard.images 且没有文字时；CF_DIBV5 优先，其次 CF_DIB；≤ max_image_mb）→
//!    在另一个工作线程里 miniz_oxide 压缩（level 1）后 hub.to_net(Out::ClipImage{..})（压缩不能在本线程做，会卡消息循环）。
//!    没有连接或 cfg.clipboard.enabled = false 时什么都不做。
//!    收到 ToDesktop::ClipText / ClipImage → 写入剪贴板（clip.rs），记下写入后的序号。
//! 2. 托盘（tray-icon crate）：图标在代码里生成（32x32 RGBA：两个错开的圆角方块，连接时青色、断开时灰色）。
//!    提示文字：“kmlink · 已连接 <对方名字>” / “kmlink · 等待连接”。菜单：
//!      状态行（禁用项）、“屏幕排布与连接设置…”（启动 `<当前exe> settings`）、“暂停共享”（勾选项，hub.to_input(Pause)）、
//!      “开机自启”（勾选项，写/删 HKCU\Software\Microsoft\Windows\CurrentVersion\Run 的 "kmlink" 值，并存 cfg.autostart）、
//!      “打开配置文件夹”（ShellExecuteW explorer config::dir()）、“退出”（hub.to_input(Quit)，然后结束本线程）。
//!    菜单事件用 tray_icon::menu::MenuEvent::set_event_handler 转成本线程消息处理。
//! 3. 显示器变化：WM_DISPLAYCHANGE（和 WM_SETTINGCHANGE 的 SPI_SETWORKAREA）→ 500ms 防抖后 sys::monitors()，
//!    hub.to_input(LocalDesk) 并 hub.send(Msg::Hello{..}) 让对方更新。
//! 4. 配置文件变化：每 1 秒看 config.toml 的修改时间；变了就 Config::load()，若成功：
//!    写回 hub.cfg，hub.to_input(Config(..))；若排布或 rev 变了 → hub.send(Msg::Layout{.., rev})。
//! 5. ToDesktop::HideCursor / ShowCursor → cursor::hide / show。Status → 刷新托盘提示与图标。Quit → 退出循环。
//! 6. first_run = true 时（刚生成了配置）：启动后直接打开设置窗口（`<当前exe> settings`）。

use crate::config::Config;
use crate::geom::Desk;
use crate::hub::{Hub, Out, Status, ToDesktop, ToInput, WM_APP_WAKE};
use crate::proto::{Msg, PROTO_VERSION};
use crate::{clip, config, cursor, log, sys};
use crossbeam_channel::{Receiver, unbounded};
use std::cell::{Cell, RefCell};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, MSG,
    PostThreadMessageW, RegisterClassW, SPI_SETWORKAREA, SW_SHOWNORMAL, SetTimer, WM_CLIPBOARDUPDATE,
    WM_DISPLAYCHANGE, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

const TIMER_CLIP: usize = 1;
const TIMER_DISPLAY: usize = 2;
const TIMER_CONFIG: usize = 3;

const ICON_SIZE: u32 = 32;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("kmlink");

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// 我们自己最后一次写剪贴板后的序号（wndproc 不借 STATE 也能读）。
    static LAST_WRITE: Cell<Option<u32>> = const { Cell::new(None) };
}

/// 剪贴板“代次”：每次本机采集到新内容 +1；压缩中的旧图片发现代次变了就作废。
static CLIP_GEN: AtomicU64 = AtomicU64::new(0);

pub fn spawn(hub: Arc<Hub>, rx: Receiver<ToDesktop>, first_run: bool) {
    let r = std::thread::Builder::new().name("desktop".into()).spawn(move || run(hub, rx, first_run));
    if let Err(e) = r {
        log!("desktop 线程启动失败：{e}");
    }
}

// ---------- 纯逻辑（可测试） ----------

/// 回声抑制：记住最后一次发出或收到的内容摘要，相同内容不再发出。
#[derive(Default)]
struct Echo {
    last: Option<u64>,
}

impl Echo {
    /// 本机采集到内容：返回是否应发送（并记下）。
    fn should_send(&mut self, h: u64) -> bool {
        if self.last == Some(h) {
            return false;
        }
        self.last = Some(h);
        true
    }
    fn received(&mut self, h: u64) {
        self.last = Some(h);
    }
    fn reset(&mut self) {
        self.last = None;
    }
}

fn hash_text(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    0u8.hash(&mut h);
    s.hash(&mut h);
    h.finish()
}

fn hash_image(format: u32, data: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    1u8.hash(&mut h);
    format.hash(&mut h);
    data.hash(&mut h);
    h.finish()
}

/// 托盘提示文字。
fn tip_text(st: &Status) -> String {
    let mut s = if st.connected {
        let name = if st.peer_name.is_empty() { "对方".to_string() } else { st.peer_name.chars().take(40).collect() };
        format!("kmlink · 已连接 {name}")
    } else {
        "kmlink · 等待连接".to_string()
    };
    if st.paused {
        s.push_str("（已暂停）");
    }
    s
}

/// 菜单状态行（去掉 “kmlink · ” 前缀）。
fn status_line(st: &Status) -> String {
    let t = tip_text(st);
    t.strip_prefix("kmlink · ").map(str::to_string).unwrap_or(t)
}

/// 圆角矩形覆盖率（像素中心采样，带 1 像素抗锯齿）。
fn rrect_cov(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (hx, hy) = ((x1 - x0) / 2.0, (y1 - y0) / 2.0);
    let qx = (px - cx).abs() - (hx - r);
    let qy = (py - cy).abs() - (hy - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let d = outside + qx.max(qy).min(0.0) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

/// 32x32 RGBA 图标：左上浅色方块 + 右下深色方块（中间留一圈透明缝）。
fn icon_rgba(connected: bool) -> Vec<u8> {
    let (back, front): ([u8; 3], [u8; 3]) =
        if connected { ([0x67, 0xE8, 0xF9], [0x06, 0xB6, 0xD4]) } else { ([0xB4, 0xB9, 0xC2], [0x6B, 0x72, 0x80]) };
    let n = ICON_SIZE as usize;
    let mut v = Vec::with_capacity(n * n * 4);
    for y in 0..n {
        for x in 0..n {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let b = rrect_cov(px, py, 1.0, 1.0, 20.0, 20.0, 4.5);
            let gap = rrect_cov(px, py, 10.0, 10.0, 33.0, 33.0, 6.5);
            let f = rrect_cov(px, py, 12.0, 12.0, 31.0, 31.0, 4.5);
            let ba = b * (1.0 - gap);
            // front over back
            let a = f + ba * (1.0 - f);
            let mut rgb = [0u8; 3];
            if a > 0.0 {
                for i in 0..3 {
                    let c = front[i] as f32 * f + back[i] as f32 * ba * (1.0 - f);
                    rgb[i] = (c / a).round().clamp(0.0, 255.0) as u8;
                }
            }
            v.extend_from_slice(&[rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8]);
        }
    }
    v
}

/// 开机自启的注册表值：带引号的 exe 路径。
fn run_value(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

// ---------- 注册表（开机自启） ----------

fn open_run_key(sam: REG_SAM_FLAGS) -> Option<HKEY> {
    let mut k = HKEY::default();
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, sam, &mut k) };
    (r == ERROR_SUCCESS).then_some(k)
}

/// 当前 Run 里 "kmlink" 的值（没有则 None）。
fn autostart_get() -> Option<String> {
    let k = open_run_key(KEY_QUERY_VALUE)?;
    let mut out = None;
    unsafe {
        let mut len = 0u32;
        if RegQueryValueExW(k, RUN_VALUE, None, None, None, Some(&mut len)) == ERROR_SUCCESS {
            let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
            let mut bytes = (buf.len() * 2) as u32;
            if RegQueryValueExW(k, RUN_VALUE, None, None, Some(buf.as_mut_ptr() as *mut u8), Some(&mut bytes))
                == ERROR_SUCCESS
            {
                let n = (bytes as usize / 2).min(buf.len());
                out = Some(clip::utf16_until_nul(&buf[..n]));
            }
        }
        let _ = RegCloseKey(k);
    }
    out
}

/// 写/删 Run 值。成功返回 true。
fn autostart_set(on: bool) -> bool {
    let Some(k) = open_run_key(KEY_SET_VALUE) else {
        log!("开机自启：打开注册表失败");
        return false;
    };
    let ok = unsafe {
        if on {
            match std::env::current_exe() {
                Ok(exe) => {
                    let bytes: Vec<u8> = sys::wide(&run_value(&exe)).iter().flat_map(|u| u.to_le_bytes()).collect();
                    RegSetValueExW(k, RUN_VALUE, None, REG_SZ, Some(&bytes)) == ERROR_SUCCESS
                }
                Err(e) => {
                    log!("开机自启：取 exe 路径失败：{e}");
                    false
                }
            }
        } else {
            let r = RegDeleteValueW(k, RUN_VALUE);
            r == ERROR_SUCCESS || r == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND
        }
    };
    unsafe {
        let _ = RegCloseKey(k);
    }
    if !ok {
        log!("开机自启：{}失败", if on { "写入" } else { "删除" });
    }
    ok
}

/// 启动时：cfg 要求自启则确保注册表指向当前 exe。返回实际是否自启。
fn autostart_sync(want: bool) -> bool {
    let cur = autostart_get();
    if want {
        let expect = std::env::current_exe().ok().map(|e| run_value(&e));
        if cur.is_none() || (expect.is_some() && cur != expect) {
            autostart_set(true);
        }
    }
    autostart_get().is_some()
}

// ---------- 状态 ----------

struct Items {
    status: MenuItem,
    settings: MenuItem,
    pause: CheckMenuItem,
    autostart: CheckMenuItem,
    folder: MenuItem,
    quit: MenuItem,
}

struct State {
    hub: Arc<Hub>,
    hwnd: HWND,
    tray: Option<TrayIcon>,
    items: Items,
    echo: Echo,
    /// 上次处理过的剪贴板序号（同一序号不重复处理）。
    last_seq: Option<u32>,
    cfg_mtime: Option<SystemTime>,
    desk: Desk,
    /// 托盘上次显示的内容（变了才刷新）。
    shown: Option<(bool, String)>,
    connected: bool,
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| match s.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => None,
    })
}

fn cfg_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn open_settings() {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = std::process::Command::new(exe).arg("settings").spawn() {
                log!("打开设置窗口失败：{e}");
            }
        }
        Err(e) => log!("取 exe 路径失败：{e}"),
    }
}

fn open_config_dir() {
    let dir = config::dir();
    let _ = std::fs::create_dir_all(&dir);
    let param = sys::wide(&format!("\"{}\"", dir.display()));
    unsafe {
        let r = ShellExecuteW(None, w!("open"), w!("explorer.exe"), PCWSTR(param.as_ptr()), None, SW_SHOWNORMAL);
        if r.0 as isize <= 32 {
            log!("打开配置文件夹失败：{}", r.0 as isize);
        }
    }
}

impl State {
    fn refresh_tray(&mut self) {
        let st = self.hub.status();
        if st.connected != self.connected {
            self.connected = st.connected;
            // 连接变化后重新计算回声（对方剪贴板可能已变）
            self.echo.reset();
        }
        self.items.pause.set_checked(st.paused);
        let tip = tip_text(&st);
        let key = (st.connected, tip.clone());
        if self.shown.as_ref() == Some(&key) {
            return;
        }
        let icon_changed = self.shown.as_ref().map(|s| s.0) != Some(st.connected);
        self.shown = Some(key);
        self.items.status.set_text(status_line(&st));
        if let Some(t) = &self.tray {
            if let Err(e) = t.set_tooltip(Some(&tip)) {
                log!("托盘提示更新失败：{e}");
            }
            if icon_changed {
                match Icon::from_rgba(icon_rgba(st.connected), ICON_SIZE, ICON_SIZE) {
                    Ok(i) => {
                        if let Err(e) = t.set_icon(Some(i)) {
                            log!("托盘图标更新失败：{e}");
                        }
                    }
                    Err(e) => log!("生成托盘图标失败：{e}"),
                }
            }
        }
    }

    /// 处理一条 ToDesktop；返回 false 表示退出。
    fn handle(&mut self, m: ToDesktop) -> bool {
        match m {
            ToDesktop::ClipText(text) => {
                let c = self.hub.cfg();
                if !c.clipboard.enabled || text.is_empty() {
                    return true;
                }
                self.echo.received(hash_text(&text));
                match clip::write_text(self.hwnd, &text) {
                    Some(seq) => {
                        LAST_WRITE.with(|w| w.set(Some(seq)));
                        self.last_seq = Some(seq);
                    }
                    None => log!("写剪贴板文字失败"),
                }
            }
            ToDesktop::ClipImage { format, data } => {
                let c = self.hub.cfg();
                if !c.clipboard.enabled || !c.clipboard.images {
                    return true;
                }
                self.echo.received(hash_image(format, &data));
                match clip::write_image(self.hwnd, format, &data) {
                    Some(seq) => {
                        LAST_WRITE.with(|w| w.set(Some(seq)));
                        self.last_seq = Some(seq);
                    }
                    None => log!("写剪贴板图片失败（{} 字节）", data.len()),
                }
            }
            ToDesktop::HideCursor => cursor::hide(),
            ToDesktop::ShowCursor => cursor::show(),
            ToDesktop::Status => self.refresh_tray(),
            ToDesktop::Quit => return false,
        }
        true
    }

    /// 菜单事件；返回 false 表示退出。
    fn menu(&mut self, id: &MenuId) -> bool {
        let it = &self.items;
        if id == it.settings.id() {
            open_settings();
        } else if id == it.pause.id() {
            let on = it.pause.is_checked();
            self.hub.to_input(ToInput::Pause(on));
            self.hub.set_status(|s| s.paused = on);
        } else if id == it.autostart.id() {
            let want = it.autostart.is_checked();
            autostart_set(want);
            let actual = autostart_get().is_some();
            it.autostart.set_checked(actual);
            self.save_autostart(actual);
        } else if id == it.folder.id() {
            open_config_dir();
        } else if id == it.quit.id() {
            log!("托盘：退出");
            // 先移除托盘图标，免得进程退出后留下残影
            self.tray = None;
            self.hub.to_input(ToInput::Quit);
            return false;
        }
        true
    }

    /// 把 autostart 存进配置文件（以磁盘上最新的为底，避免覆盖设置窗口刚写的内容）。
    fn save_autostart(&mut self, on: bool) {
        let mut c = Config::load().unwrap_or_else(|| self.hub.cfg());
        if let Ok(mut g) = self.hub.cfg.write() {
            g.autostart = on;
        }
        if c.autostart == on {
            return;
        }
        c.autostart = on;
        match c.save() {
            Ok(()) => self.cfg_mtime = cfg_mtime(&self.hub.cfg_path),
            Err(e) => log!("保存配置失败：{e}"),
        }
    }

    fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_CLIP => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_CLIP);
                }
                self.clipboard_changed();
            }
            TIMER_DISPLAY => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_DISPLAY);
                }
                self.display_changed();
            }
            TIMER_CONFIG => self.check_config(),
            _ => {}
        }
    }

    fn clipboard_changed(&mut self) {
        let seq = clip::seq();
        if LAST_WRITE.with(|w| w.get()) == Some(seq) || self.last_seq == Some(seq) {
            return;
        }
        let c = self.hub.cfg();
        if !c.clipboard.enabled || !self.hub.status().connected {
            return;
        }
        self.last_seq = Some(seq);
        let max_text = c.clipboard.max_text_kb as usize * 1024;
        let max_image = c.clipboard.max_image_mb as usize * 1024 * 1024;
        match clip::read(self.hwnd, max_text, c.clipboard.images, max_image) {
            Some(clip::Content::Text(text)) => {
                if self.echo.should_send(hash_text(&text)) {
                    CLIP_GEN.fetch_add(1, Ordering::SeqCst);
                    self.hub.send(Msg::ClipText { text });
                }
            }
            Some(clip::Content::Image { format, data }) => {
                if !self.echo.should_send(hash_image(format, &data)) {
                    return;
                }
                let generation = CLIP_GEN.fetch_add(1, Ordering::SeqCst) + 1;
                let hub = self.hub.clone();
                let r = std::thread::Builder::new().name("clip-deflate".into()).spawn(move || {
                    let deflated = miniz_oxide::deflate::compress_to_vec(&data, 1);
                    // 压缩期间又有新内容或断开了：作废
                    if CLIP_GEN.load(Ordering::SeqCst) != generation || !hub.status().connected {
                        return;
                    }
                    hub.to_net(Out::ClipImage { format, raw_len: data.len() as u32, deflated });
                });
                if let Err(e) = r {
                    log!("启动压缩线程失败：{e}");
                }
            }
            None => {}
        }
    }

    fn display_changed(&mut self) {
        let d = sys::monitors();
        if d.is_empty() || d == self.desk {
            return;
        }
        log!("显示器变化：{:?}", d.monitors);
        self.desk = d.clone();
        self.hub.to_input(ToInput::LocalDesk(d.clone()));
        let c = self.hub.cfg();
        self.hub.send(Msg::Hello { ver: PROTO_VERSION, name: c.name, monitors: d.monitors, udp_port: c.port });
    }

    fn check_config(&mut self) {
        let m = cfg_mtime(&self.hub.cfg_path);
        if m.is_none() || m == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = m;
        let Some(c) = Config::load() else {
            log!("config.toml 读取/解析失败，忽略这次修改");
            return;
        };
        log!("配置文件已变化，重新加载");
        let old = self.hub.cfg();
        if let Ok(mut g) = self.hub.cfg.write() {
            *g = c.clone();
        }
        self.hub.to_input(ToInput::Config(Box::new(c.clone())));
        if c.layout.rev != old.layout.rev || c.layout.layout != old.layout.layout {
            let l = c.layout.layout;
            self.hub.send(Msg::Layout { side: l.side, offset: l.offset, scale: l.scale, rev: c.layout.rev });
        }
        if c.name != old.name || c.port != old.port {
            let d = self.desk.clone();
            self.hub.send(Msg::Hello { ver: PROTO_VERSION, name: c.name.clone(), monitors: d.monitors, udp_port: c.port });
        }
        if c.autostart != old.autostart {
            autostart_set(c.autostart);
            self.items.autostart.set_checked(autostart_get().is_some());
        }
    }
}

// ---------- 窗口与消息循环 ----------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            // 自己写入引起的通知：忽略
            if LAST_WRITE.with(|w| w.get()) != Some(clip::seq()) {
                unsafe {
                    SetTimer(Some(hwnd), TIMER_CLIP, 100, None);
                }
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            unsafe {
                SetTimer(Some(hwnd), TIMER_DISPLAY, 500, None);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_SETTINGCHANGE => {
            if wp.0 == SPI_SETWORKAREA.0 as usize {
                unsafe {
                    SetTimer(Some(hwnd), TIMER_DISPLAY, 500, None);
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_TIMER => {
            // 借不到状态（重入）时不处理：一次性定时器没被 Kill，下个周期再来
            let _ = with_state(|s| s.on_timer(wp.0));
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn create_window() -> Option<HWND> {
    unsafe {
        let hinst = match GetModuleHandleW(None) {
            Ok(h) => HINSTANCE(h.0),
            Err(e) => {
                log!("GetModuleHandleW 失败：{e}");
                return None;
            }
        };
        let class = w!("kmlink-desktop");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst, lpszClassName: class, ..Default::default() };
        if RegisterClassW(&wc) == 0 {
            log!("RegisterClassW 失败");
            return None;
        }
        match CreateWindowExW(WS_EX_TOOLWINDOW, class, w!("kmlink"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst), None) {
            Ok(h) => Some(h),
            Err(e) => {
                log!("创建隐藏窗口失败：{e}");
                None
            }
        }
    }
}

fn build_tray(items: &Items, st: &Status) -> Option<TrayIcon> {
    let menu = Menu::new();
    let sep1 = PredefinedMenuItem::separator();
    let sep2 = PredefinedMenuItem::separator();
    if let Err(e) = menu.append_items(&[
        &items.status,
        &sep1,
        &items.settings,
        &items.pause,
        &items.autostart,
        &items.folder,
        &sep2,
        &items.quit,
    ]) {
        log!("托盘菜单创建失败：{e}");
    }
    let mut b = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip(tip_text(st));
    match Icon::from_rgba(icon_rgba(st.connected), ICON_SIZE, ICON_SIZE) {
        Ok(i) => b = b.with_icon(i),
        Err(e) => log!("生成托盘图标失败：{e}"),
    }
    match b.build() {
        Ok(t) => Some(t),
        Err(e) => {
            log!("托盘图标创建失败：{e}");
            None
        }
    }
}

fn run(hub: Arc<Hub>, rx: Receiver<ToDesktop>, first_run: bool) {
    let Some(hwnd) = create_window() else {
        // 没有窗口也要能响应 Quit 等，最低限度：只 drain channel
        log!("desktop：没有窗口，剪贴板/托盘不可用");
        for m in rx.iter() {
            match m {
                ToDesktop::HideCursor => cursor::hide(),
                ToDesktop::ShowCursor => cursor::show(),
                ToDesktop::Quit => break,
                _ => {}
            }
        }
        return;
    };
    let tid = unsafe { GetCurrentThreadId() };

    unsafe {
        if let Err(e) = AddClipboardFormatListener(hwnd) {
            log!("AddClipboardFormatListener 失败：{e}");
        }
    }

    // 菜单事件 → 本线程
    let (mtx, mrx) = unbounded::<MenuId>();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = mtx.send(e.id);
        unsafe {
            let _ = PostThreadMessageW(tid, WM_APP_WAKE, WPARAM(0), LPARAM(0));
        }
    }));
    // 托盘鼠标事件用不到；设个空处理器，免得事件堆在内部无界队列里
    TrayIconEvent::set_event_handler(Some(|_e: TrayIconEvent| {}));

    let cfg = hub.cfg();
    let autostart_on = autostart_sync(cfg.autostart);
    let st = hub.status();
    let items = Items {
        status: MenuItem::new(status_line(&st), false, None),
        settings: MenuItem::new("屏幕排布与连接设置…", true, None),
        pause: CheckMenuItem::new("暂停共享", true, st.paused, None),
        autostart: CheckMenuItem::new("开机自启", true, autostart_on, None),
        folder: MenuItem::new("打开配置文件夹", true, None),
        quit: MenuItem::new("退出", true, None),
    };
    let tray = build_tray(&items, &st);

    let state = State {
        hub: hub.clone(),
        hwnd,
        tray,
        items,
        echo: Echo::default(),
        last_seq: Some(clip::seq()), // 启动前已有的剪贴板内容不发
        cfg_mtime: cfg_mtime(&hub.cfg_path),
        desk: sys::monitors(),
        shown: Some((st.connected, tip_text(&st))),
        connected: st.connected,
    };
    STATE.with(|s| *s.borrow_mut() = Some(state));

    unsafe {
        SetTimer(Some(hwnd), TIMER_CONFIG, 1000, None);
    }
    hub.set_desktop_tid(tid);
    // 登记前可能已有 Status 等消息：先刷新一次
    let _ = with_state(|s| s.refresh_tray());

    if first_run {
        open_settings();
    }

    let mut msg = MSG::default();
    'outer: loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        if msg.hwnd.is_invalid() && msg.message == WM_APP_WAKE {
            while let Ok(id) = mrx.try_recv() {
                if with_state(|s| s.menu(&id)) == Some(false) {
                    break 'outer;
                }
            }
            while let Ok(m) = rx.try_recv() {
                if with_state(|s| s.handle(m)) == Some(false) {
                    break 'outer;
                }
            }
            continue;
        }
        unsafe {
            DispatchMessageW(&msg);
        }
    }

    // 清理
    MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
    unsafe {
        let _ = RemoveClipboardFormatListener(hwnd);
        let _ = KillTimer(Some(hwnd), TIMER_CONFIG);
        let _ = KillTimer(Some(hwnd), TIMER_CLIP);
        let _ = KillTimer(Some(hwnd), TIMER_DISPLAY);
    }
    let state = STATE.with(|s| s.borrow_mut().take());
    drop(state); // 托盘图标随之移除
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    log!("desktop 线程结束");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_suppression() {
        let mut e = Echo::default();
        let a = hash_text("a");
        let b = hash_text("b");
        assert!(e.should_send(a));
        assert!(!e.should_send(a)); // 同样的内容不重发
        e.received(b);
        assert!(!e.should_send(b)); // 收到的不回发
        assert!(e.should_send(a));
        e.reset();
        assert!(e.should_send(a));
        // 文字与图片摘要不同
        assert_ne!(hash_text("x"), hash_image(8, b"x"));
        assert_ne!(hash_image(8, b"x"), hash_image(17, b"x"));
    }

    #[test]
    fn icon_shape() {
        for c in [true, false] {
            let v = icon_rgba(c);
            assert_eq!(v.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            let px = |x: usize, y: usize| &v[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4];
            assert_eq!(px(0, 0)[3], 0); // 角落透明
            assert_eq!(px(31, 0)[3], 0);
            assert_eq!(px(0, 31)[3], 0);
            assert_eq!(px(6, 6)[3], 255); // 后方块
            assert_eq!(px(22, 22)[3], 255); // 前方块
            assert_eq!(px(16, 11)[3], 0); // 两块之间的缝
            assert!(Icon::from_rgba(v, ICON_SIZE, ICON_SIZE).is_ok());
        }
        assert_ne!(icon_rgba(true), icon_rgba(false));
    }

    #[test]
    fn tips() {
        let mut s = Status::default();
        assert_eq!(tip_text(&s), "kmlink · 等待连接");
        assert_eq!(status_line(&s), "等待连接");
        s.connected = true;
        s.peer_name = "OFFICE".into();
        assert_eq!(tip_text(&s), "kmlink · 已连接 OFFICE");
        s.paused = true;
        assert_eq!(status_line(&s), "已连接 OFFICE（已暂停）");
        s.peer_name = "x".repeat(200);
        assert!(tip_text(&s).chars().count() < 128);
    }

    #[test]
    fn run_value_quoted() {
        let p = Path::new(r"C:\Program Files\kmlink\kmlink.exe");
        assert_eq!(run_value(p), "\"C:\\Program Files\\kmlink\\kmlink.exe\"");
    }
}
