//! desktop 线程：一个隐藏的顶层窗口（WS_EX_TOOLWINDOW，不显示）+ 消息循环。负责：
//!
//! 1. 剪贴板（clip.rs）：AddClipboardFormatListener；WM_CLIPBOARDUPDATE 时若 GetClipboardSequenceNumber()
//!    等于我们自己最后一次写入后的序号 → 忽略；否则 100ms 防抖（SetTimer）后读取（剪贴板被占用则 200ms 后重试，最多 3 次）。
//!    - 回声保护按内容（见 `ClipSync`）：记一个“两边已同步内容的摘要” synced——写入对方内容、或发出本机内容时更新；
//!      本机采集到的内容摘要等于 synced 就不发（对方或本机有程序把同样内容再写一遍也不会来回）；本机出现别的内容
//!      （包括敏感、超限、不支持的内容）时清掉。不设时间窗口、不限次数。写入的对方内容带 CanUploadToCloudClipboard = 0，
//!      切断云剪贴板回路。
//!    - 文字（≤ max_text_kb）→ 先 Out::CancelClip（让 net 丢掉待发的旧图片）再 hub.send(Msg::ClipText)；
//!      超过 32 KiB（UTF-16）的大段文字和图片一样压缩后走 Out::ClipImage（format = CF_UNICODETEXT），不让大帧堵住键鼠消息。
//!      文字超过 max_text_kb 直接跳过（不退而读图片），记一行限频日志。
//!    - 图片（cfg.clipboard.images 且没有文字时；CF_DIBV5 优先，其次 CF_DIB；≤ max_image_mb）→ 交给唯一的、按需启动的
//!      压缩工作线程（`submit`）：摘要、回声判断、CancelClip、分块 deflate（level 1，内容过时中途放弃）、Out::ClipImage。
//!      工作线程只有“最新一条”的槽位，旧任务连同它的大缓冲直接丢弃；空闲 10 秒自己退出，空闲时不占内存。
//!    - 没有连接、暂停共享或 cfg.clipboard.enabled = false 时不发；密码管理器标记的敏感内容
//!      （含 CanUploadToCloudClipboard = 0）不发。
//!    - 收到 ToDesktop::ClipText / ClipImage：每轮 drain 只应用最新一条（旧的直接丢弃，防大图片积压占内存）；
//!      图片先校验 DIB 结构；写入后记下序号、Out::CancelClip（丢掉本机待发的旧图片和对方还在传的旧图片）；
//!      剪贴板被占则只保留最新一条待写内容，定时器重试 3 次。
//! 2. 托盘（tray-icon crate）：图标在代码里生成（32x32 RGBA：两个错开的圆角方块，连接时青色、断开时灰色）。
//!    提示文字：“kmlink · 已连接 <对方名字>” / “kmlink · 等待连接”。菜单：
//!      状态行（禁用项）、“屏幕排布与连接设置…”（启动 `<当前exe> settings`）、“暂停共享”（勾选项，hub.to_input(Pause)）、
//!      “开机自启”（勾选项，写/删 HKCU\Software\Microsoft\Windows\CurrentVersion\Run 的 "kmlink" 值，并存 cfg.autostart）、
//!      “打开配置文件夹”（ShellExecuteW explorer config::dir()）、“退出”（hub.to_input(Quit)，然后结束本线程）。
//!    菜单事件用 tray_icon::menu::MenuEvent::set_event_handler 转成本线程消息处理。
//! 3. 显示器变化：WM_DISPLAYCHANGE（和 WM_SETTINGCHANGE 的 SPI_SETWORKAREA）→ 500ms 防抖后 sys::monitors()，
//!    hub.to_input(LocalDesk) 并 hub.send(Msg::Hello{..}) 让对方更新。
//! 4. 配置文件变化：每 1 秒看 config.toml 的修改时间；变了且连续两次看到同一个值（防读到写了一半的文件）才 Config::load()；
//!    成功才记下这个修改时间（失败下一秒重试），写回 hub.cfg，hub.to_input(Config(..))；
//!    若 layout.rev 变了 → hub.send(Msg::Layout{.., rev})。这里从不写配置（托盘“开机自启”除外）。
//! 5. ToDesktop::HideCursor / ShowCursor → cursor::hide / show。Status → 刷新托盘提示与图标。Quit → 退出循环。
//! 6. 睡眠/注销：WM_POWERBROADCAST(PBT_APMSUSPEND)、WM_QUERYENDSESSION → hub.to_input(GoHome)；
//!    WM_ENDSESSION（真要结束）→ 发 Quit 让主线程走正常退出（release_all、恢复光标、写完日志），在窗口过程里等它。
//!
//! 叫醒：hub 往隐藏窗口 PostMessage(WM_APP_WAKE)，窗口过程里 drain channel（托盘菜单的模态循环里也能收到）；
//! 线程消息 WM_APP_WAKE（窗口登记前）和每秒的 TIMER_CONFIG 也会 drain 一次兜底。
//! first_run = true 时（刚生成了配置）：启动后直接打开设置窗口（`<当前exe> settings`）。
//!
//! 内存：空闲时这里不持有任何剪贴板内容，只留一个 u64 摘要；大缓冲（读出的图片、压缩结果、待写的对方内容）用完即释放。

use crate::config::Config;
use crate::geom::Desk;
use crate::hub::{Hub, Out, Status, ToDesktop, ToInput, WM_APP_WAKE};
use crate::proto::{Msg, PROTO_VERSION};
use crate::{clip, config, cursor, log, sys};
use crossbeam_channel::{Receiver, unbounded};
use std::cell::{Cell, OnceCell, RefCell};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
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
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, SPI_SETWORKAREA, SW_SHOWNORMAL, SetTimer, WM_CLIPBOARDUPDATE, WM_DISPLAYCHANGE,
    WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_POPUP,
};
use windows::core::{PCWSTR, w};

const TIMER_CLIP: usize = 1;
const TIMER_DISPLAY: usize = 2;
const TIMER_CONFIG: usize = 3;
const TIMER_WRITE: usize = 4;

/// 剪贴板打不开时的重试：次数与间隔（毫秒）。
const CLIP_RETRIES: u32 = 3;
const CLIP_RETRY_MS: u32 = 200;
/// 写入对方内容时剪贴板被占：重试次数与间隔（毫秒）。
const WRITE_RETRIES: u32 = 3;
const WRITE_RETRY_MS: u32 = 300;
/// 超过这么多字节（UTF-16）的文字压缩后走低优先级分片通道。
const BIG_TEXT: usize = 32 * 1024;
/// 压缩工作线程空闲这么久就退出。
const WORKER_IDLE: Duration = Duration::from_secs(10);
/// 分块压缩的块大小：每块之间看一次内容是否已过时。
const DEFLATE_CHUNK: usize = 1024 * 1024;
/// “内容太大/格式不对，不同步”类日志的最短间隔。
const SKIP_LOG_EVERY: Duration = Duration::from_secs(60);
/// 注销/关机时等主线程走完退出流程的上限。
const END_WAIT: Duration = Duration::from_secs(3);
/// WM_POWERBROADCAST 的“即将睡眠”（Win32_System_Power 里的常量，免得为它多开一个 feature）。
const PBT_APMSUSPEND: usize = 4;

const ICON_SIZE: u32 = 32;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("kmlink");

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// 我们自己最后一次写剪贴板后的序号（wndproc 不借 STATE 也能读）。
    static LAST_WRITE: Cell<Option<u32>> = const { Cell::new(None) };
    /// 睡眠/注销处理用（重入时借不到 STATE 也要能发 GoHome）。
    static HUB: OnceCell<Arc<Hub>> = const { OnceCell::new() };
}

/// 剪贴板同步状态：桌面线程与压缩工作线程共用。
static SYNC: Mutex<ClipSync> = Mutex::new(ClipSync::new());

pub fn spawn(hub: Arc<Hub>, rx: Receiver<ToDesktop>, first_run: bool) {
    let r = std::thread::Builder::new().name("desktop".into()).spawn(move || run(hub, rx, first_run));
    if let Err(e) = r {
        log!("desktop 线程启动失败：{e}");
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn sync<R>(f: impl FnOnce(&mut ClipSync) -> R) -> R {
    f(&mut lock(&SYNC))
}

// ---------- 纯逻辑（可测试） ----------

/// 按内容的回声保护 + 新旧判断。
struct ClipSync {
    /// 本机内容代次：本机采集到新内容、或收到对方内容时 +1；压缩中的旧任务据此作废。
    generation: u64,
    /// 两边已同步内容的摘要（None = 不能确定两边一样）。
    synced: Option<u64>,
    /// synced 的改动次数：对方图片写入后在工作线程里补算摘要，期间 synced 被别处改过就不再写回。
    ver: u64,
}

impl ClipSync {
    const fn new() -> ClipSync {
        ClipSync { generation: 0, synced: None, ver: 0 }
    }
    /// 本机剪贴板有了新内容（任何种类）：返回新代次，之前的压缩任务作废。
    fn local(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }
    /// 两边不再一致（本机有了不同步的内容、对方内容没写入等）。
    fn forget(&mut self) {
        self.synced = None;
        self.ver += 1;
    }
    /// 本机内容（代次 g、摘要 h）准备发出：代次已过时或两边已同步（回声）就不发；要发就记为已同步。
    fn offer(&mut self, g: u64, h: u64) -> bool {
        if g != self.generation || self.synced == Some(h) {
            return false;
        }
        self.synced = Some(h);
        self.ver += 1;
        true
    }
    /// 收到对方内容、准备写入：本机压缩中的旧内容作废。返回当前 ver（写入成功后交给 `written`）。
    fn incoming(&mut self) -> u64 {
        self.generation += 1;
        self.ver
    }
    /// 对方内容已写入，摘要 h：期间 synced 没被改过才记为已同步。
    fn written(&mut self, ver: u64, h: u64) {
        if self.ver == ver {
            self.synced = Some(h);
            self.ver += 1;
        }
    }
    fn current(&self, g: u64) -> bool {
        self.generation == g
    }
}

/// 剪贴板打不开：返回这是对 seq 的第几次重试（超过 CLIP_RETRIES 返回 None = 放弃）。
fn next_retry(prev: Option<(u32, u32)>, seq: u32) -> Option<u32> {
    let n = match prev {
        Some((s, n)) if s == seq => n + 1,
        _ => 1,
    };
    (n <= CLIP_RETRIES).then_some(n)
}

/// 文字是否大到要压缩后分片发送。
fn is_big_text(s: &str) -> bool {
    // UTF-8 字节数 ≥ UTF-16 单元数，先粗筛
    s.len() > BIG_TEXT / 2 && s.encode_utf16().count() * 2 > BIG_TEXT
}

fn hash_text(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    0u8.hash(&mut h);
    s.hash(&mut h);
    h.finish()
}

/// 图片摘要：结构正常的 DIB 只看尺寸、位深、颜色表和像素（不看头的版本和掩码），
/// 这样 CF_DIB 与系统转换出的 CF_DIBV5 算作同一内容；结构不对的整段算。
fn hash_image(format: u32, data: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match clip::dib(data) {
        Ok(d) => {
            1u8.hash(&mut h);
            (d.width, d.height, d.bit_count).hash(&mut h);
            data[d.palette..d.bits].hash(&mut h);
            data[d.bits..d.bits + d.bits_len].hash(&mut h);
        }
        Err(_) => {
            2u8.hash(&mut h);
            format.hash(&mut h);
            data.hash(&mut h);
        }
    }
    h.finish()
}

fn is_clip(m: &ToDesktop) -> bool {
    matches!(m, ToDesktop::ClipText(_) | ToDesktop::ClipImage { .. })
}

/// 一轮 drain：剪贴板消息（ClipText / ClipImage）只留最新一条，旧的当场丢弃（释放内存）；其余按原顺序。
fn latest_clip(it: impl Iterator<Item = ToDesktop>) -> (Vec<ToDesktop>, Option<ToDesktop>) {
    let mut rest = Vec::new();
    let mut clip = None;
    for m in it {
        if is_clip(&m) {
            clip = Some(m);
        } else {
            rest.push(m);
        }
    }
    (rest, clip)
}

/// 配置文件修改时间：和已加载的一样 → 不动；变了要连续两次看到同一个值才加载（防读到写了一半的文件）。
/// seen = 上次看到、还没加载的修改时间。返回是否该加载。
fn cfg_due(loaded: Option<SystemTime>, seen: &mut Option<SystemTime>, now: Option<SystemTime>) -> bool {
    if now.is_none() || now == loaded {
        *seen = None;
        return false;
    }
    if *seen != now {
        *seen = now;
        return false;
    }
    true
}

/// 限频：距上次至少 gap 才返回 true（并记下这次）。
fn every(last: &mut Option<Instant>, now: Instant, gap: Duration) -> bool {
    if last.is_some_and(|t| now.saturating_duration_since(t) < gap) {
        return false;
    }
    *last = Some(now);
    true
}

/// 分块 deflate（raw deflate，和 miniz_oxide::deflate::compress_to_vec 同格式），每块之前问一次 `live`，
/// 内容过时就中途放弃。输出缓冲按需增长（compress_to_vec 一上来就按输入的一半分配），最后收紧到实际大小。
fn deflate_while(data: &[u8], live: impl Fn() -> bool) -> Option<Vec<u8>> {
    use miniz_oxide::deflate::core::{CompressorOxide, TDEFLFlush, TDEFLStatus, compress, create_comp_flags_from_zip_params};
    let mut c = CompressorOxide::new(create_comp_flags_from_zip_params(1, 0, 0));
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0;
    let mut input = data;
    loop {
        if !live() {
            return None;
        }
        let (chunk, flush) =
            if input.len() > DEFLATE_CHUNK { (&input[..DEFLATE_CHUNK], TDEFLFlush::None) } else { (input, TDEFLFlush::Finish) };
        if out.len() - pos < 128 * 1024 {
            let grow = (out.len() / 2).max(256 * 1024);
            out.resize(out.len() + grow, 0);
        }
        let (st, used, wrote) = compress(&mut c, chunk, &mut out[pos..], flush);
        pos += wrote;
        input = &input[used..];
        match st {
            TDEFLStatus::Done => break,
            TDEFLStatus::Okay => {}
            _ => return None,
        }
    }
    out.truncate(pos);
    out.shrink_to_fit();
    Some(out)
}

// ---------- 压缩工作线程（唯一、按需启动、只保留最新任务） ----------

/// 本机内容。
enum Payload {
    Text(String),
    Image(Vec<u8>),
}

enum Job {
    /// 本机采集到的图片 / 大段文字：摘要 → 回声判断 → CancelClip → 压缩 → Out::ClipImage。
    Local { generation: u64, format: u32, data: Payload },
    /// 刚写入的对方图片：补算摘要记为已同步，然后释放这份字节。
    Written { ver: u64, format: u32, data: Vec<u8> },
}

/// 两个槽位各只留最新一条；Written 先做（它决定紧随其后的本机内容是不是回声）。
struct Work {
    written: Option<Job>,
    local: Option<Job>,
    running: bool,
}

static WORK: Mutex<Work> = Mutex::new(Work { written: None, local: None, running: false });
static WORK_CV: Condvar = Condvar::new();

fn submit(hub: &Arc<Hub>, job: Job) {
    let mut w = lock(&WORK);
    // 槽位里的旧任务直接替换掉，它的大缓冲随之释放
    match job {
        j @ Job::Written { .. } => w.written = Some(j),
        j @ Job::Local { .. } => w.local = Some(j),
    }
    if w.running {
        WORK_CV.notify_one();
        return;
    }
    let h = hub.clone();
    match std::thread::Builder::new().name("clip-work".into()).spawn(move || worker(h)) {
        Ok(_) => w.running = true,
        Err(e) => {
            w.written = None;
            w.local = None;
            drop(w);
            log!("启动剪贴板工作线程失败：{e}");
        }
    }
}

fn worker(hub: Arc<Hub>) {
    loop {
        let job = {
            let mut w = lock(&WORK);
            loop {
                if let Some(j) = w.written.take() {
                    break j;
                }
                if let Some(j) = w.local.take() {
                    break j;
                }
                let (g, r) = WORK_CV.wait_timeout(w, WORKER_IDLE).unwrap_or_else(|e| e.into_inner());
                w = g;
                if r.timed_out() && w.written.is_none() && w.local.is_none() {
                    // 空闲：退出，线程栈和压缩器都还给系统
                    w.running = false;
                    return;
                }
            }
        };
        run_job(&hub, job);
    }
}

fn run_job(hub: &Hub, job: Job) {
    match job {
        Job::Written { ver, format, data } => {
            let h = hash_image(format, &data);
            drop(data);
            sync(|s| s.written(ver, h));
        }
        Job::Local { generation, format, data } => {
            let h = match &data {
                Payload::Text(t) => hash_text(t),
                Payload::Image(d) => hash_image(format, d),
            };
            // 回声判断与 CancelClip 在同一把锁里：先让 net 丢掉旧图片，再发新内容
            let go = sync(|s| {
                let ok = s.offer(generation, h);
                if ok {
                    hub.to_net(Out::CancelClip);
                }
                ok
            });
            if !go {
                return;
            }
            let raw = match data {
                Payload::Text(t) => clip::utf16le(&t),
                Payload::Image(d) => d,
            };
            let Ok(raw_len) = u32::try_from(raw.len()) else { return };
            let Some(deflated) = deflate_while(&raw, || sync(|s| s.current(generation))) else { return };
            drop(raw);
            if !hub.status().connected {
                return;
            }
            // 检查与发送在同一把锁里：期间有了更新的内容就不发
            sync(|s| {
                if s.current(generation) {
                    hub.to_net(Out::ClipImage { format, raw_len, deflated });
                }
            });
        }
    }
}

// ---------- 托盘（纯逻辑） ----------

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

/// 对方发来、要写入本机剪贴板的内容。
enum Incoming {
    Text(String),
    Image { format: u32, data: Vec<u8> },
}

/// 剪贴板被占、还没写进去的对方内容（只留最新一条）。
struct PendingWrite {
    what: Incoming,
    /// 收到时 ClipSync 的 ver。
    ver: u64,
    tries: u32,
}

struct State {
    hub: Arc<Hub>,
    hwnd: HWND,
    rx: Receiver<ToDesktop>,
    mrx: Receiver<MenuId>,
    /// 已收到退出：不再处理 channel，等 WM_QUIT。
    quitting: bool,
    tray: Option<TrayIcon>,
    items: Items,
    /// 上次处理过的剪贴板序号（同一序号不重复处理）。
    last_seq: Option<u32>,
    /// 剪贴板打不开时的重试：(序号, 已重试次数)。
    retry: Option<(u32, u32)>,
    /// 待重试写入的对方内容。
    pending: Option<PendingWrite>,
    /// 上次“内容太大，不同步”日志的时间（限频）。
    skip_log: Option<Instant>,
    /// 已加载的配置文件修改时间 / 看到但还没加载的 / 上次解析失败的（同一个只报一次）。
    cfg_mtime: Option<SystemTime>,
    cfg_seen: Option<SystemTime>,
    cfg_bad: Option<SystemTime>,
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
    /// drain 菜单事件和 ToDesktop；收到退出时 PostQuitMessage，由消息循环收尾。
    fn pump(&mut self) {
        while !self.quitting {
            if let Ok(id) = self.mrx.try_recv() {
                if !self.menu(&id) {
                    self.quit();
                }
                continue;
            }
            let (rest, clip) = latest_clip(self.rx.try_iter());
            if rest.is_empty() && clip.is_none() {
                break;
            }
            for m in rest {
                if !self.handle(m) {
                    self.quit();
                    break;
                }
            }
            if let Some(m) = clip
                && !self.quitting
            {
                self.incoming(m);
            }
        }
    }

    fn quit(&mut self) {
        self.quitting = true;
        unsafe { PostQuitMessage(0) };
    }

    fn refresh_tray(&mut self) {
        let st = self.hub.status();
        if st.connected != self.connected {
            self.connected = st.connected;
            // 连接变化：对方剪贴板可能已变，不再认为两边一致
            sync(|s| s.forget());
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
            m @ (ToDesktop::ClipText(_) | ToDesktop::ClipImage { .. }) => self.incoming(m),
            ToDesktop::HideCursor => cursor::hide(),
            ToDesktop::ShowCursor => cursor::show(),
            ToDesktop::Status => self.refresh_tray(),
            ToDesktop::ClipForget => sync(|s| s.forget()),
            ToDesktop::Quit => return false,
        }
        true
    }

    /// 对方发来的剪贴板内容：校验后写入（剪贴板被占则稍后重试）。
    fn incoming(&mut self, m: ToDesktop) {
        let c = self.hub.cfg();
        let max_text = c.clipboard.max_text_kb as usize * 1024;
        let what = if !c.clipboard.enabled || self.hub.status().paused {
            None
        } else {
            match m {
                ToDesktop::ClipText(t) => Some(Incoming::Text(t)),
                // 大段文字：压缩通道送来的 UTF-16 LE
                ToDesktop::ClipImage { format: clip::CF_UNICODETEXT, data } => Some(Incoming::Text(clip::from_utf16le(&data))),
                ToDesktop::ClipImage { format, data }
                    if c.clipboard.images && (format == clip::CF_DIB || format == clip::CF_DIBV5) =>
                {
                    match clip::dib(&data) {
                        Ok(_) => Some(Incoming::Image { format, data }),
                        Err(e) => {
                            log!("对方发来的图片不对（{e}，{} 字节），丢弃", data.len());
                            None
                        }
                    }
                }
                _ => None,
            }
        };
        let what = what.filter(|w| match w {
            Incoming::Text(t) => !t.is_empty() && t.len() <= max_text,
            Incoming::Image { .. } => true,
        });
        // 有了更新的对方内容：之前没写进去的那条作废
        self.drop_pending();
        let Some(what) = what else {
            // 对方剪贴板变了而本机没跟上：两边不再一致
            sync(|s| s.forget());
            return;
        };
        let ver = sync(|s| s.incoming());
        self.try_write(PendingWrite { what, ver, tries: 0 });
    }

    /// 写入对方内容；剪贴板被占则留着，定时器重试。
    fn try_write(&mut self, mut p: PendingWrite) {
        let r = match &p.what {
            Incoming::Text(t) => clip::write_text(self.hwnd, t),
            Incoming::Image { format, data } => clip::write_image(self.hwnd, *format, data),
        };
        match r {
            Ok(seq) => {
                LAST_WRITE.with(|w| w.set(Some(seq)));
                self.last_seq = Some(seq);
                // 对方内容已到位：本机待发的旧图片、对方还在传的旧图片都不要了
                self.hub.to_net(Out::CancelClip);
                match p.what {
                    Incoming::Text(t) => {
                        let h = hash_text(&t);
                        sync(|s| s.written(p.ver, h));
                    }
                    // 图片摘要在工作线程里算（不卡消息循环），算完这份字节就释放
                    Incoming::Image { format, data } => submit(&self.hub, Job::Written { ver: p.ver, format, data }),
                }
            }
            Err(clip::WriteError::Busy) if p.tries < WRITE_RETRIES => {
                p.tries += 1;
                self.pending = Some(p);
                unsafe {
                    SetTimer(Some(self.hwnd), TIMER_WRITE, WRITE_RETRY_MS, None);
                }
            }
            Err(e) => {
                let what = match &p.what {
                    Incoming::Text(t) => format!("文字 {} 字节", t.len()),
                    Incoming::Image { data, .. } => format!("图片 {} 字节", data.len()),
                };
                log!("写剪贴板失败（{what}，{e:?}），放弃");
                sync(|s| s.forget());
            }
        }
    }

    fn drop_pending(&mut self) {
        if self.pending.take().is_some() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_WRITE);
            }
        }
    }

    /// 本机有了不同步的内容：待写的对方内容作废（本机的更新），两边不再一致。
    fn local_other(&mut self) {
        self.drop_pending();
        let was_synced = sync(|s| {
            s.local();
            let had = s.synced.is_some();
            s.forget();
            had
        });
        // 告诉对方两边已不一致（只在“已同步 → 不一致”时发一次），免得对方之后复制同样内容时被当成回声不发
        if was_synced && self.hub.status().connected {
            self.hub.send(Msg::ClipForget);
        }
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

    /// 把 autostart 存进配置文件（读-改-写，只动这一项）。不更新 cfg_mtime：让 check_config 照常重新加载，
    /// 免得吞掉设置窗口刚存的修改；内存里已先改好，重新加载时不会再切一次注册表。
    fn save_autostart(&mut self, on: bool) {
        if let Ok(mut g) = self.hub.cfg.write() {
            g.autostart = on;
        }
        if let Err(e) = config::update(&self.hub.cfg(), |c| c.autostart = on) {
            log!("保存配置失败：{e}");
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
            TIMER_CONFIG => {
                self.check_config();
                // 兜底：万一叫醒消息丢了
                self.pump();
            }
            TIMER_WRITE => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_WRITE);
                }
                if let Some(p) = self.pending.take() {
                    self.try_write(p);
                }
            }
            _ => {}
        }
    }

    fn clipboard_changed(&mut self) {
        let seq = clip::seq();
        if LAST_WRITE.with(|w| w.get()) == Some(seq) || self.last_seq == Some(seq) {
            return;
        }
        let c = self.hub.cfg();
        let st = self.hub.status();
        if !c.clipboard.enabled || !st.connected || st.paused {
            // 不同步，但要记下本机有了别的内容
            self.retry = None;
            self.last_seq = Some(seq);
            self.local_other();
            return;
        }
        let max_text = c.clipboard.max_text_kb as usize * 1024;
        let max_image = c.clipboard.max_image_mb as usize * 1024 * 1024;
        let content = match clip::read(self.hwnd, max_text, c.clipboard.images, max_image) {
            Ok(x) => x,
            Err(clip::Busy) => {
                // 打不开：不记 last_seq，稍后重试
                match next_retry(self.retry, seq) {
                    Some(n) => {
                        self.retry = Some((seq, n));
                        unsafe {
                            SetTimer(Some(self.hwnd), TIMER_CLIP, CLIP_RETRY_MS, None);
                        }
                    }
                    None => {
                        log!("剪贴板一直被占用，放弃这次同步");
                        self.retry = None;
                        self.last_seq = Some(seq);
                        self.local_other();
                    }
                }
                return;
            }
        };
        self.retry = None;
        self.last_seq = Some(seq);
        match content {
            clip::Content::Text(text) if !is_big_text(&text) => {
                let h = hash_text(&text);
                // 两边已一致（回声）：不发；待写的对方内容也照常写
                if !sync(|s| {
                    let g = s.local();
                    s.offer(g, h)
                }) {
                    return;
                }
                self.drop_pending();
                // 先让 net 丢掉还没发完的旧图片，再发新内容
                self.hub.to_net(Out::CancelClip);
                self.hub.send(Msg::ClipText { text });
            }
            clip::Content::Text(text) => {
                let generation = sync(|s| s.local());
                self.drop_pending();
                submit(&self.hub, Job::Local { generation, format: clip::CF_UNICODETEXT, data: Payload::Text(text) });
            }
            clip::Content::Image { format, data } => {
                let generation = sync(|s| s.local());
                self.drop_pending();
                submit(&self.hub, Job::Local { generation, format, data: Payload::Image(data) });
            }
            clip::Content::TooBig { image, size } => {
                if every(&mut self.skip_log, Instant::now(), SKIP_LOG_EVERY) {
                    log!("剪贴板{}太大（约 {} KB），不同步", if image { "图片" } else { "文字" }, size / 1024);
                }
                self.local_other();
            }
            clip::Content::Concealed | clip::Content::Other => self.local_other(),
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
        self.hub.send(Msg::Hello { ver: PROTO_VERSION, name: c.name, monitors: d.monitors, udp_port: c.port, id: crate::proto::instance_id() });
    }

    fn check_config(&mut self) {
        let m = cfg_mtime(&self.hub.cfg_path);
        if !cfg_due(self.cfg_mtime, &mut self.cfg_seen, m) {
            return;
        }
        let Some(c) = Config::load() else {
            // 不记修改时间：下一秒再试（同一个版本只报一次）
            if self.cfg_bad != m {
                self.cfg_bad = m;
                log!("config.toml 读取/解析失败，稍后重试");
            }
            return;
        };
        self.cfg_mtime = m;
        self.cfg_seen = None;
        self.cfg_bad = None;
        log!("配置文件已变化，重新加载");
        let old = self.hub.cfg();
        if let Ok(mut g) = self.hub.cfg.write() {
            *g = c.clone();
        }
        self.hub.to_input(ToInput::Config(Box::new(c.clone())));
        // 只在 rev 变了时发：采用对方排布后的重新加载（rev 相同）不回发
        if c.layout.rev != old.layout.rev {
            let l = c.layout.layout;
            self.hub.send(Msg::Layout { side: l.side, offset: l.offset, scale: l.scale, rev: c.layout.rev });
        }
        if c.name != old.name || c.port != old.port {
            let d = self.desk.clone();
            self.hub.send(Msg::Hello { ver: PROTO_VERSION, name: c.name.clone(), monitors: d.monitors, udp_port: c.port, id: crate::proto::instance_id() });
        }
        if c.autostart != old.autostart {
            autostart_set(c.autostart);
            self.items.autostart.set_checked(autostart_get().is_some());
        }
    }
}

// ---------- 睡眠 / 注销 ----------

fn go_home(why: &str) {
    HUB.with(|h| {
        if let Some(hub) = h.get() {
            log!("{why}：回到本机");
            hub.to_input(ToInput::GoHome);
        }
    });
}

/// 会话真的要结束（注销/关机/安装程序要求退出）：窗口过程一返回系统就可能结束进程，
/// 所以在这里发 Quit 让主线程走正常退出（release_all、恢复光标、写完日志、结束进程），并等它。
fn end_session() {
    let Some(hub) = HUB.with(|h| h.get().cloned()) else { return };
    log!("系统注销/关机：退出");
    // 先移除托盘图标（借不到状态就算了）
    let _ = with_state(|s| s.tray = None);
    hub.to_input(ToInput::GoHome);
    hub.to_input(ToInput::Quit);
    let end = Instant::now() + END_WAIT;
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    // 主线程没走完：至少恢复光标、写完日志
    cursor::show();
    log::flush(300);
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
        WM_POWERBROADCAST => {
            if wp.0 == PBT_APMSUSPEND {
                go_home("系统即将睡眠");
            }
            LRESULT(1)
        }
        WM_QUERYENDSESSION => {
            go_home("系统即将注销/关机");
            LRESULT(1)
        }
        WM_ENDSESSION => {
            if wp.0 != 0 {
                end_session();
            }
            LRESULT(0)
        }
        WM_TIMER => {
            // 借不到状态（重入）时不处理：一次性定时器没被 Kill，下个周期再来
            let _ = with_state(|s| s.on_timer(wp.0));
            LRESULT(0)
        }
        WM_APP_WAKE => {
            // 借不到状态（重入）时消息留在 channel 里，下次叫醒或 TIMER_CONFIG 再处理
            let _ = with_state(|s| s.pump());
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
    HUB.with(|h| {
        let _ = h.set(hub.clone());
    });

    unsafe {
        if let Err(e) = AddClipboardFormatListener(hwnd) {
            log!("AddClipboardFormatListener 失败：{e}");
        }
    }

    // 菜单事件 → 本线程（投给隐藏窗口，模态循环里也不丢）
    let (mtx, mrx) = unbounded::<MenuId>();
    let hv = hwnd.0 as isize;
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = mtx.send(e.id);
        unsafe {
            let _ = PostMessageW(Some(HWND(hv as *mut core::ffi::c_void)), WM_APP_WAKE, WPARAM(0), LPARAM(0));
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
        rx,
        mrx,
        quitting: false,
        tray,
        items,
        last_seq: Some(clip::seq()), // 启动前已有的剪贴板内容不发
        retry: None,
        pending: None,
        skip_log: None,
        cfg_mtime: cfg_mtime(&hub.cfg_path),
        cfg_seen: None,
        cfg_bad: None,
        desk: sys::monitors(),
        shown: Some((st.connected, tip_text(&st))),
        connected: st.connected,
    };
    STATE.with(|s| *s.borrow_mut() = Some(state));

    unsafe {
        SetTimer(Some(hwnd), TIMER_CONFIG, 1000, None);
    }
    hub.set_desktop_tid(tid);
    hub.set_desktop_hwnd(hwnd.0 as isize);
    // 登记前可能已有 Status 等消息：先刷新一次
    let _ = with_state(|s| s.refresh_tray());

    if first_run {
        open_settings();
    }

    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        if msg.hwnd.is_invalid() && msg.message == WM_APP_WAKE {
            // 线程消息（窗口登记前的叫醒）；退出由 pump 里的 PostQuitMessage 结束循环
            let _ = with_state(|s| s.pump());
            continue;
        }
        unsafe {
            DispatchMessageW(&msg);
        }
    }

    // 清理：之后的叫醒改回线程消息（没人收也无妨）
    hub.set_desktop_hwnd(0);
    MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
    unsafe {
        let _ = RemoveClipboardFormatListener(hwnd);
        let _ = KillTimer(Some(hwnd), TIMER_CONFIG);
        let _ = KillTimer(Some(hwnd), TIMER_CLIP);
        let _ = KillTimer(Some(hwnd), TIMER_DISPLAY);
        let _ = KillTimer(Some(hwnd), TIMER_WRITE);
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
    use crate::clip::tests::make_dib;

    #[test]
    fn echo_by_content() {
        let mut s = ClipSync::new();
        let (a, b, c) = (hash_text("a"), hash_text("b"), hash_text("c"));
        // 本机复制 a：发出
        let g = s.local();
        assert!(s.offer(g, a));
        // 本机有程序把 a 再写一遍（任意次、任意时间）：不发
        for _ in 0..5 {
            let g = s.local();
            assert!(!s.offer(g, a));
        }
        // 收到对方 b 并写入；对方或本机程序把 b 再写回来：不回发
        let v = s.incoming();
        s.written(v, b);
        for _ in 0..5 {
            let g = s.local();
            assert!(!s.offer(g, b));
        }
        // 用户再复制 a：和已同步的不同，发出
        let g = s.local();
        assert!(s.offer(g, a));
        // 本机出现敏感内容等不同步的东西：清掉；之后再复制 a 也要发
        s.local();
        s.forget();
        let g = s.local();
        assert!(s.offer(g, a));
        // 对方内容没写入（暂停、太大）：两边不一致，再复制 a 照发
        s.incoming();
        s.forget();
        let g = s.local();
        assert!(s.offer(g, a));
        // 过时的代次（压缩期间又有新内容）不发
        let g1 = s.local();
        let g2 = s.local();
        assert!(!s.offer(g1, c));
        assert!(s.offer(g2, c));
        assert!(!s.current(g1));
    }

    #[test]
    fn echo_pending_image_digest() {
        let mut s = ClipSync::new();
        let (img, t) = (hash_image(8, &[1, 2, 3]), hash_text("t"));
        // 写入对方图片后，摘要在工作线程里补算；期间本机程序把同一图片再写一遍（代次变了）
        let v = s.incoming();
        let g = s.local();
        s.written(v, img); // 工作线程先做 Written
        assert!(!s.offer(g, img)); // 再判断本机内容：回声，不发
        // 补算期间本机已复制了新文字：迟到的摘要不能覆盖
        let v = s.incoming();
        let g = s.local();
        assert!(s.offer(g, t));
        s.written(v, img);
        let g = s.local();
        assert!(!s.offer(g, t)); // synced 仍是 t
        // 收到对方内容：本机压缩中的旧内容作废
        let g = s.local();
        s.incoming();
        assert!(!s.current(g));
    }

    #[test]
    fn image_digest_ignores_header_version() {
        let dib = make_dib(40, 2, 2, 32, 0, 0, 0, 16);
        let v5 = make_dib(124, 2, 2, 32, 3, 0, 0, 16);
        assert_eq!(hash_image(clip::CF_DIB, &dib), hash_image(clip::CF_DIBV5, &v5));
        let other = make_dib(40, 2, -2, 32, 0, 0, 0, 16);
        assert_ne!(hash_image(clip::CF_DIB, &dib), hash_image(clip::CF_DIB, &other));
        let mut px = dib.clone();
        px[45] ^= 1;
        assert_ne!(hash_image(clip::CF_DIB, &dib), hash_image(clip::CF_DIB, &px));
        // 文字与图片摘要不同；结构不对的按整段算
        assert_ne!(hash_text("x"), hash_image(8, b"x"));
        assert_ne!(hash_image(8, b"x"), hash_image(17, b"x"));
    }

    #[test]
    fn keeps_only_latest_clip() {
        let msgs = vec![
            ToDesktop::ClipText("old".into()),
            ToDesktop::Status,
            ToDesktop::ClipImage { format: 8, data: vec![0; 1000] },
            ToDesktop::HideCursor,
            ToDesktop::ClipText("new".into()),
            ToDesktop::Quit,
        ];
        let (rest, clip) = latest_clip(msgs.into_iter());
        assert!(matches!(clip, Some(ToDesktop::ClipText(ref t)) if t == "new"));
        assert_eq!(rest.len(), 3);
        assert!(matches!(rest[0], ToDesktop::Status));
        assert!(matches!(rest[1], ToDesktop::HideCursor));
        assert!(matches!(rest[2], ToDesktop::Quit));
        // 最新的是图片
        let (rest, clip) = latest_clip(
            vec![ToDesktop::ClipText("a".into()), ToDesktop::ClipImage { format: 17, data: vec![1] }].into_iter(),
        );
        assert!(rest.is_empty());
        assert!(matches!(clip, Some(ToDesktop::ClipImage { format: 17, .. })));
        let (rest, clip) = latest_clip(vec![ToDesktop::Status].into_iter());
        assert_eq!(rest.len(), 1);
        assert!(clip.is_none());
    }

    #[test]
    fn config_mtime_must_settle() {
        let t = |s: u64| Some(SystemTime::UNIX_EPOCH + Duration::from_secs(s));
        let mut seen = None;
        // 没变
        assert!(!cfg_due(t(1), &mut seen, t(1)));
        // 变了：第一次只记下
        assert!(!cfg_due(t(1), &mut seen, t(2)));
        // 又变了（还在写）：重新等
        assert!(!cfg_due(t(1), &mut seen, t(3)));
        // 连续两次一样：加载
        assert!(cfg_due(t(1), &mut seen, t(3)));
        // 加载失败（调用方不更新 loaded）：下一秒再试
        assert!(cfg_due(t(1), &mut seen, t(3)));
        // 加载成功后与已加载的一样：不动
        assert!(!cfg_due(t(3), &mut seen, t(3)));
        assert_eq!(seen, None);
        // 文件不见了
        assert!(!cfg_due(t(3), &mut seen, None));
    }

    #[test]
    fn rate_limit() {
        let t0 = Instant::now();
        let mut last = None;
        assert!(every(&mut last, t0, Duration::from_secs(60)));
        assert!(!every(&mut last, t0 + Duration::from_secs(10), Duration::from_secs(60)));
        assert!(every(&mut last, t0 + Duration::from_secs(61), Duration::from_secs(60)));
    }

    #[test]
    fn chunked_deflate() {
        // 3.5 块：有重复也有变化
        let data: Vec<u8> = (0..DEFLATE_CHUNK * 7 / 2).map(|i| ((i / 7) ^ (i >> 12)) as u8).collect();
        let z = deflate_while(&data, || true).unwrap();
        assert!(z.len() < data.len());
        assert_eq!(z.capacity(), z.len());
        assert_eq!(miniz_oxide::inflate::decompress_to_vec(&z).unwrap(), data);
        // 空输入
        let z = deflate_while(&[], || true).unwrap();
        assert!(miniz_oxide::inflate::decompress_to_vec(&z).unwrap().is_empty());
        // 内容过时：中途放弃
        let n = std::cell::Cell::new(0);
        assert!(
            deflate_while(&data, || {
                n.set(n.get() + 1);
                n.get() < 2
            })
            .is_none()
        );
    }

    #[test]
    fn clip_retry() {
        assert_eq!(next_retry(None, 5), Some(1));
        assert_eq!(next_retry(Some((5, 1)), 5), Some(2));
        assert_eq!(next_retry(Some((5, 2)), 5), Some(3));
        assert_eq!(next_retry(Some((5, 3)), 5), None);
        assert_eq!(next_retry(Some((5, 3)), 6), Some(1)); // 新内容重新计数
    }

    #[test]
    fn big_text() {
        assert!(!is_big_text(""));
        assert!(!is_big_text(&"a".repeat(BIG_TEXT / 2)));
        assert!(is_big_text(&"a".repeat(BIG_TEXT / 2 + 1)));
        // 中文：UTF-8 3 字节、UTF-16 2 字节
        assert!(!is_big_text(&"中".repeat(BIG_TEXT / 2)));
        assert!(is_big_text(&"中".repeat(BIG_TEXT / 2 + 1)));
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
