//! desktop 线程：一个隐藏的顶层窗口（WS_EX_TOOLWINDOW，不显示）+ 消息循环。负责：
//!
//! 1. 剪贴板（clip.rs）：AddClipboardFormatListener；WM_CLIPBOARDUPDATE 时若 GetClipboardSequenceNumber()
//!    等于我们自己最后一次写入后的序号 → 忽略；否则 100ms 防抖（SetTimer）后读取（剪贴板被占用则 200ms 后重试，最多 3 次）。
//!    - 回声保护按内容（见 `ClipSync`）：记一个“两边已同步内容的摘要” synced——写入对方内容、或发出本机内容时更新；
//!      本机采集到的内容摘要等于 synced 就不发（对方或本机有程序把同样内容再写一遍也不会来回）；本机出现别的内容
//!      （包括敏感、超限、不支持的内容）时清掉。不设时间窗口、不限次数。写入的对方内容带 CanUploadToCloudClipboard = 0，
//!      切断云剪贴板回路。
//!    - 文字（≤ max_text_kb）→ 先 Out::CancelClip（让 net 丢掉待发的旧图片）再 hub.send(Msg::ClipText)；
//!      超过 32 KiB（UTF-16）的大段文字和图片一样走压缩流（format = CF_UNICODETEXT），不让大帧堵住键鼠消息。
//!      文字超过 max_text_kb 直接跳过（不退而读图片），记一行限频日志。
//!    - 图片（cfg.clipboard.images 且没有文字时；CF_DIBV5 优先，其次 CF_DIB；≤ max_image_mb）→ 交给唯一的、按需启动的
//!      压缩工作线程（`submit`）：摘要、回声判断、CancelClip、Out::ClipStream（开始），然后边压缩（level 1）边把
//!      ≤ 16 KiB 的分片交给 net（`hub::clip_stream`，还没发出的压缩数据至多 1 MiB，满了就等 net 发走）；
//!      内容过时、net 不要了（取消/断开/被顶掉）就中途放弃。压缩完原始数据立刻释放。大段文字边编码成 UTF-16 边压缩，
//!      不另建整段 UTF-16 副本。工作线程只有“最新一条”的槽位，旧任务连同它的大缓冲直接丢弃；空闲 10 秒自己退出。
//!    - 没有连接、暂停共享或 cfg.clipboard.enabled = false 时不发；密码管理器标记的敏感内容
//!      （含 CanUploadToCloudClipboard = 0）不发。
//!    - 收到 ToDesktop::ClipText / ClipData：每轮 drain 只应用最新一条（旧的直接丢弃并释放）；
//!      ClipData 是 net 边收边解压进去的 GlobalAlloc 内存（图片已校验 DIB，摘要已算好），直接交给 SetClipboardData，
//!      不再拷贝；写入后记下序号与摘要、Out::CancelClip（丢掉本机待发的旧图片和对方还在传的旧图片）；
//!      剪贴板被占则只保留最新一条待写内容（连同那块内存），定时器重试 3 次，放弃时释放。
//! 2. 托盘（tray-icon crate）：图标在代码里生成（32x32 RGBA：两个错开的圆角方块，连接时青色、断开时灰色）。
//!    提示文字：“kmlink · 已连接 <对方名字>” / “kmlink · 等待连接”。菜单：
//!      状态行（禁用项）、“屏幕排布与连接设置…”（启动 `<当前exe> settings`）、“暂停共享”（勾选项，hub.to_input(Pause)）、
//!      “开机自启”（勾选项，写/删 HKCU\Software\Microsoft\Windows\CurrentVersion\Run 的 "kmlink" 值，并存 cfg.autostart）、
//!      “打开配置文件夹”（ShellExecuteW explorer config::dir()）、“退出”（hub.to_input(Quit)，然后结束本线程）。
//!    菜单事件用 tray_icon::menu::MenuEvent::set_event_handler 转成本线程消息处理。
//! 3. 显示器变化：WM_DISPLAYCHANGE（和 WM_SETTINGCHANGE 的 SPI_SETWORKAREA）→ 500ms 防抖后 sys::monitors_dpi()，
//!    显示器变了 hub.to_input(LocalDesk)；显示器或 DPI 变了 hub.send(Msg::Hello{..}) 让对方更新。
//! 4. 配置文件变化：每 1 秒看 config.toml 的修改时间；变了且连续两次看到同一个值（防读到写了一半的文件）才 Config::load()；
//!    成功才记下这个修改时间（失败下一秒重试），写回 hub.cfg，hub.to_input(Config(..))；
//!    若 layout.rev 变了 → hub.send(Msg::Layout{.., rev})；名字/端口变了 → 重发 Hello。
//!    这里从不写配置（托盘“开机自启”、对方给本机改名除外）。
//!    同一个定时器里看 rename_peer.json（设置窗口要给对方改名）：读了就删，连着且没过期 → hub.send(Msg::Rename)。
//! 5. ToDesktop::HideCursor / ShowCursor → cursor::hide / show。Quit → 退出循环。
//!    Status → 刷新托盘提示与图标；连接状态（连着、对方名字/地址、暂停）变了就重写 status.json（启动时也写一次，
//!    退出时主线程写 connected = false，见 `save_exit_status`）。
//!    Rename（对方给本机改名）→ config::clean_name 后 config::update 只写 name，不改内存里的 hub.cfg：
//!    让上面的重新加载照常发现名字变了、重发 Hello，对方随之更新。
//! 6. 睡眠/注销：WM_POWERBROADCAST(PBT_APMSUSPEND)、WM_QUERYENDSESSION → hub.to_input(GoHome)；
//!    WM_ENDSESSION（真要结束）→ 发 Quit 让主线程走正常退出（release_all、恢复光标、写完日志），在窗口过程里等它。
//!
//! 叫醒：hub 往隐藏窗口 PostMessage(WM_APP_WAKE)，窗口过程里 drain channel（托盘菜单的模态循环里也能收到）；
//! 线程消息 WM_APP_WAKE（窗口登记前）和每秒的 TIMER_CONFIG 也会 drain 一次兜底。
//! first_run = true 时（刚生成了配置）：启动后直接打开设置窗口（`<当前exe> settings`）。
//!
//! 内存：空闲时这里不持有任何剪贴板内容，只留一个 u64 摘要；大缓冲（读出的图片、待写的对方内容）用完即释放。
//! 发一张图片时的峰值 ≈ 读出的一份原始数据 + 压缩器（约 0.3 MiB）+ 未发出的压缩数据（≤ 1 MiB）。

use crate::clip::{hash_image, hash_text};
use crate::config::{Config, StatusFile};
use crate::geom::Desk;
use crate::hub::{self, Hub, Out, Status, StreamTx, ToDesktop, ToInput, WM_APP_WAKE};
use crate::proto::{self, Msg, PROTO_VERSION};
use crate::island::{self, Kind, Note};
use crate::{clip, config, cursor, log, sys};
use crossbeam_channel::{Receiver, unbounded};
use std::cell::{Cell, OnceCell, RefCell};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
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
/// 每次喂给压缩器的原始数据（块之间看一次内容是否已过时；大段文字的 UTF-16 编码缓冲也是这么大）。
const INPUT_BLOCK: usize = 64 * 1024;
/// 压缩流里还没发出的压缩数据上限：满了压缩线程就等 net 发走。
const STREAM_LIMIT: usize = 1024 * 1024;
/// “内容太大/格式不对，不同步”类日志的最短间隔。
const SKIP_LOG_EVERY: Duration = Duration::from_secs(60);
/// 注销/关机时等主线程走完退出流程的上限。
const END_WAIT: Duration = Duration::from_secs(3);
/// WM_POWERBROADCAST 的“即将睡眠”（Win32_System_Power 里的常量，免得为它多开一个 feature）。
const PBT_APMSUSPEND: usize = 4;

const ICON_SIZE: u32 = 32;

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
    /// synced 的改动次数：收到对方内容到真正写入（可能要重试）之间 synced 被别处改过，写入后就不再记为已同步。
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
    /// 标成已同步的本机内容其实没送到（中途放弃、没连上）：撤销，之后同样内容还能再发。
    fn unoffer(&mut self, h: u64) {
        if self.synced == Some(h) {
            self.synced = None;
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

fn is_clip(m: &ToDesktop) -> bool {
    matches!(m, ToDesktop::ClipText(_) | ToDesktop::ClipData(_))
}

/// 一轮 drain：剪贴板消息（ClipText / ClipData）只留最新一条，旧的当场丢弃（释放内存）；其余按原顺序。
/// ClipForget 若出现在留下的那条剪贴板消息之后，返回 forget_after = true（写入之后还要再清“已同步”）；
/// 出现在它之前的照常放进 rest（先处理）。
fn latest_clip(it: impl Iterator<Item = ToDesktop>) -> (Vec<ToDesktop>, Option<ToDesktop>, bool) {
    let mut rest = Vec::new();
    let mut clip = None;
    let mut forget_after = false;
    for m in it {
        if is_clip(&m) {
            clip = Some(m);
            forget_after = false;
        } else if matches!(m, ToDesktop::ClipForget) && clip.is_some() {
            forget_after = true;
        } else {
            rest.push(m);
        }
    }
    (rest, clip, forget_after)
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

// ---------- 压缩工作线程（唯一、按需启动、只保留最新任务） ----------

/// 本机内容。
enum Payload {
    Text(String),
    Image(Vec<u8>),
}

impl Payload {
    /// 发送的原始字节数（文字按 UTF-16 LE 算）。
    fn raw_len(&self) -> usize {
        match self {
            Payload::Text(t) => t.encode_utf16().count() * 2,
            Payload::Image(d) => d.len(),
        }
    }
}

/// 本机采集到的图片 / 大段文字：摘要 → 回声判断 → CancelClip → Out::ClipStream → 流式压缩。
struct Job {
    generation: u64,
    format: u32,
    data: Payload,
}

/// 槽位只留最新一条。
struct Work {
    local: Option<Job>,
    running: bool,
}

static WORK: Mutex<Work> = Mutex::new(Work { local: None, running: false });
static WORK_CV: Condvar = Condvar::new();

fn submit(hub: &Arc<Hub>, job: Job) {
    let mut w = lock(&WORK);
    // 槽位里的旧任务直接替换掉，它的大缓冲随之释放
    w.local = Some(job);
    if w.running {
        WORK_CV.notify_one();
        return;
    }
    let h = hub.clone();
    match std::thread::Builder::new().name("clip-work".into()).spawn(move || worker(h)) {
        Ok(_) => w.running = true,
        Err(e) => {
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
                if let Some(j) = w.local.take() {
                    break j;
                }
                let (g, r) = WORK_CV.wait_timeout(w, WORKER_IDLE).unwrap_or_else(|e| e.into_inner());
                w = g;
                if r.timed_out() && w.local.is_none() {
                    // 空闲：退出，线程栈都还给系统
                    w.running = false;
                    return;
                }
            }
        };
        run_job(&hub, job);
    }
}

fn run_job(hub: &Hub, job: Job) {
    let Job { generation, format, data } = job;
    let h = match &data {
        Payload::Text(t) => hash_text(t),
        Payload::Image(d) => hash_image(format, d),
    };
    let Ok(raw_len) = u32::try_from(data.raw_len()) else { return };
    let connected = hub.status().connected;
    let (tx, rx) = hub::clip_stream(STREAM_LIMIT);
    // 回声判断、CancelClip、开始发送在同一把锁里：先让 net 丢掉旧图片，再开始新内容；
    // 之后才有的更新内容（代次变了）会在它自己的 CancelClip / 新流里顶掉这一条
    let go = sync(|s| {
        if !s.offer(generation, h) {
            log!("剪贴板{}与对方一致或已过时，不发", describe_format(format));
            return false;
        }
        if !connected {
            s.unoffer(h);
            return false;
        }
        hub.to_net(Out::CancelClip);
        hub.to_net(Out::ClipStream { format, raw_len, data: rx });
        true
    });
    if !go {
        return;
    }
    let (kind, detail) = match &data {
        Payload::Text(t) => (Kind::Text, island::preview(t)),
        Payload::Image(d) => (Kind::Image, island::size_text(d.len())),
    };
    if stream_out(&data, &tx, &|| sync(|s| s.current(generation))) {
        tx.finish();
        // 交完了才弹（至多还有 1 MiB 没发出）：和对方写入后弹的那颗差不多同时；没发完就不弹
        hub.to_desktop(ToDesktop::Note(Note { who: hub.cfg().name, kind, detail, ok: true }));
        log!("剪贴板{}已发出（{} KB）", describe_format(format), raw_len / 1024);
    } else {
        // 没送到：撤销“已同步”。常见情形：很多程序放图片会连着触发两次剪贴板通知（放入、OleFlushClipboard），
        // 第二次让第一次的发送过时；不撤销的话第二次会被当成回声，结果一次都没发出去。
        sync(|s| s.unoffer(h));
        log!("剪贴板{}没发完就放弃了（更新的内容、断开或对方不要了）", describe_format(format));
    }
    // 没送完就丢弃 tx = 放弃（net 随后告诉对方作废）；原始数据随 data 一起释放
}

/// 把原始数据流式压缩成 ≤ CHUNK 的分片交给 tx（还没发出的满了就等 net 发走）。
/// 内容过时（live 为假）、net 不要了：中途返回 false。文字边编码成 UTF-16 LE 边压缩。
fn stream_out(data: &Payload, tx: &StreamTx, live: &dyn Fn() -> bool) -> bool {
    let mut z = proto::Deflater::new();
    let mut emit = |piece: Vec<u8>| tx.send(piece, live);
    match data {
        Payload::Image(d) => {
            let mut pos = 0;
            loop {
                if !live() {
                    return false;
                }
                let end = (pos + INPUT_BLOCK).min(d.len());
                let last = end == d.len();
                if !z.write(&d[pos..end], last, &mut emit) {
                    return false;
                }
                if last {
                    return true;
                }
                pos = end;
            }
        }
        Payload::Text(t) => {
            let mut units = t.encode_utf16();
            let mut buf = Vec::with_capacity(INPUT_BLOCK);
            loop {
                if !live() {
                    return false;
                }
                buf.clear();
                for u in units.by_ref().take(INPUT_BLOCK / 2) {
                    buf.extend_from_slice(&u.to_le_bytes());
                }
                let last = buf.len() < INPUT_BLOCK;
                if !z.write(&buf, last, &mut emit) {
                    return false;
                }
                if last {
                    return true;
                }
            }
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

// ---------- 开机自启（注册表读写在 sys） ----------

/// 启动时：cfg 要求自启则确保注册表指向当前 exe。返回实际是否自启。
fn autostart_sync(want: bool) -> bool {
    let cur = sys::autostart_get();
    if want {
        let expect = std::env::current_exe().ok().map(|e| sys::run_value(&e));
        if cur.is_none() || (expect.is_some() && cur != expect) {
            sys::autostart_set(true);
        }
    }
    sys::autostart_get().is_some()
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
    /// 图片 / 大段文字：已在 GlobalAlloc 的内存里，直接交给剪贴板。
    Data(clip::Blob),
}

impl Incoming {
    fn describe(&self) -> String {
        match self {
            Incoming::Text(t) => format!("文字 {} 字节", t.len()),
            Incoming::Data(b) if b.format == clip::CF_UNICODETEXT => format!("文字 {} 字节", b.text_len),
            Incoming::Data(b) => format!("图片 {} 字节", b.mem.len()),
        }
    }
}

/// 写入对方内容时弹的提示：谁复制的、预览或大小。
fn incoming_note(what: &mut Incoming, who: String) -> Note {
    let (kind, detail) = match what {
        Incoming::Text(t) => (Kind::Text, island::preview(t)),
        Incoming::Data(b) if b.format == clip::CF_UNICODETEXT => {
            // 大段文字：只解开头一小段
            let head = b.mem.with(|m| {
                let n = (m.len() / 2).min(256);
                let units: Vec<u16> = (0..n).map(|i| u16::from_le_bytes([m[2 * i], m[2 * i + 1]])).take_while(|&u| u != 0).collect();
                String::from_utf16_lossy(&units)
            });
            (Kind::Text, island::preview(&head.unwrap_or_default()))
        }
        Incoming::Data(b) => (Kind::Image, island::size_text(b.mem.len())),
    };
    Note { who, kind, detail, ok: true }
}

/// 对方内容是否写入本机：剪贴板同步开着（未暂停）、在上限内、不为空；图片还要求开着图片同步、格式是 DIB。
fn accept_incoming(m: ToDesktop, c: &config::ClipCfg, paused: bool) -> Option<Incoming> {
    if !c.enabled || paused {
        return None;
    }
    let max_text = c.max_text_kb as usize * 1024;
    match m {
        ToDesktop::ClipText(t) if !t.is_empty() && t.len() <= max_text => Some(Incoming::Text(t)),
        // 大段文字：net 已算好 UTF-8 长度
        ToDesktop::ClipData(b) if b.format == clip::CF_UNICODETEXT => {
            (b.text_len > 0 && b.text_len <= max_text).then_some(Incoming::Data(b))
        }
        // 图片：net 收齐时已校验过 DIB 结构
        ToDesktop::ClipData(b) if c.images && (b.format == clip::CF_DIB || b.format == clip::CF_DIBV5) => {
            Some(Incoming::Data(b))
        }
        _ => None,
    }
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
    /// 本机各显示器的有效 DPI（和 desk.monitors 一一对应），随 Hello 发给对方。
    dpis: Vec<u32>,
    /// 上次写进 status.json 的内容（t = 0；变了才写）。
    status_file: Option<StatusFile>,
    /// 上次写 status.json 失败了（连续失败只记一次日志）。
    status_failed: bool,
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
            let (rest, clip, forget_after) = latest_clip(self.rx.try_iter());
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
                if forget_after {
                    sync(|s| s.forget());
                }
            }
        }
    }

    fn quit(&mut self) {
        self.quitting = true;
        unsafe { PostQuitMessage(0) };
    }

    fn refresh_tray(&mut self) {
        let st = self.hub.status();
        self.write_status(&st);
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

    /// 连接状态变了才重写 status.json（设置窗口读）。写成功才记下，失败的由每秒的 TIMER_CONFIG 重试；
    /// 连续失败只记第一次日志。
    fn write_status(&mut self, st: &Status) {
        let f = status_file(st, 0);
        if self.status_file.as_ref() == Some(&f) {
            return;
        }
        match config::save_status(&status_file(st, config::now_ms())) {
            Ok(()) => {
                self.status_file = Some(f);
                self.status_failed = false;
            }
            Err(e) => {
                if !self.status_failed {
                    log!("写 status.json 失败：{e}");
                }
                self.status_failed = true;
            }
        }
    }

    /// 对方给本机改名：只写配置文件里的 name，由重新加载去更新 hub.cfg 并重发 Hello。
    fn renamed_by_peer(&self, name: &str) {
        let Some(name) = config::clean_name(name) else {
            log!("对方发来的新名字无效，忽略");
            return;
        };
        log!("对方把本机改名为 {name}");
        if let Err(e) = config::update(&self.hub.cfg(), |c| c.name = name) {
            log!("保存配置失败：{e}");
        }
    }

    /// 设置窗口要给对方改名（rename_peer.json）：连着且没过期才发。
    fn check_rename_peer(&self) {
        let Some(r) = config::take_rename_peer(config::now_ms()) else { return };
        match r {
            Some(name) if self.hub.status().connected => {
                log!("把对方改名为 {name}");
                self.hub.send(Msg::Rename { name });
            }
            Some(name) => log!("没连上对方，不改名（{name}）"),
            None => log!("rename_peer.json 无效或已过期，忽略"),
        }
    }

    /// 处理一条 ToDesktop；返回 false 表示退出。
    fn handle(&mut self, m: ToDesktop) -> bool {
        match m {
            m @ (ToDesktop::ClipText(_) | ToDesktop::ClipData(_)) => self.incoming(m),
            ToDesktop::HideCursor => cursor::hide(),
            ToDesktop::ShowCursor => cursor::show(),
            ToDesktop::Status => self.refresh_tray(),
            ToDesktop::ClipForget => sync(|s| s.forget()),
            ToDesktop::PeerForget => {
                sync(|s| s.forget());
                if self.hub.cfg().clipboard.notify {
                    island::fail_own(&self.hub.cfg().name, "对方未接收");
                }
            }
            ToDesktop::Note(n) => self.note(&n),
            ToDesktop::Rename(name) => self.renamed_by_peer(&name),
            ToDesktop::Quit => return false,
        }
        true
    }

    /// 弹剪贴板同步提示（设置里关了就不弹）。
    fn note(&self, n: &Note) {
        if self.hub.cfg().clipboard.notify {
            island::show(n);
        }
    }

    /// 本机复制的内容没同步（太大 / 敏感）：本机弹“未同步”，也让对方弹。
    /// 对方弹不弹由对方自己的设置决定。
    fn note_unsynced(&self, kind: Kind, detail: String) {
        self.note(&Note { who: self.hub.cfg().name, kind, detail: detail.clone(), ok: false });
        self.hub.send(Msg::ClipNote { kind: kind.code(), detail });
    }

    /// 对方发来的剪贴板内容：校验后写入（剪贴板被占则稍后重试）。
    fn incoming(&mut self, m: ToDesktop) {
        let what = accept_incoming(m, &self.hub.cfg().clipboard, self.hub.status().paused);
        // 有了更新的对方内容：之前没写进去的那条作废
        self.drop_pending();
        let Some(what) = what else {
            // 对方剪贴板变了而本机没跟上：两边不再一致
            log!("对方的剪贴板内容没写入（暂停、关闭了同步、类型或大小不符）");
            sync(|s| s.forget());
            self.hub.send(Msg::ClipForget);
            return;
        };
        let ver = sync(|s| s.incoming());
        self.try_write(PendingWrite { what, ver, tries: 0 });
    }

    /// 写入对方内容；剪贴板被占则留着，定时器重试。
    fn try_write(&mut self, mut p: PendingWrite) {
        // 提示要在写入前取（写入成功后那块内存就归系统了）
        let note = incoming_note(&mut p.what, self.hub.status().peer_name);
        let r = match &mut p.what {
            Incoming::Text(t) => clip::write_text(self.hwnd, t),
            // 成功则这块内存归系统；失败时还在 b.mem 里（Busy 留着重试，放弃时随 p 释放）
            Incoming::Data(b) => clip::write_owned(self.hwnd, b.format, &mut b.mem),
        };
        match r {
            Ok(seq) => {
                LAST_WRITE.with(|w| w.set(Some(seq)));
                self.last_seq = Some(seq);
                // 对方内容已到位：本机待发的旧图片不要了（对方此刻还在传的一定更新，不能丢）
                self.hub.to_net(Out::CancelClipSend);
                // 图片 / 大段文字的摘要 net 收的时候已算好
                let h = match &p.what {
                    Incoming::Text(t) => hash_text(t),
                    Incoming::Data(b) => b.hash,
                };
                sync(|s| s.written(p.ver, h));
                if matches!(p.what, Incoming::Data(_)) {
                    log!("已写入对方的{}", p.what.describe());
                }
                self.note(&note);
            }
            Err(clip::WriteError::Busy) if p.tries < WRITE_RETRIES => {
                p.tries += 1;
                self.pending = Some(p);
                unsafe {
                    SetTimer(Some(self.hwnd), TIMER_WRITE, WRITE_RETRY_MS, None);
                }
            }
            Err(e) => {
                log!("写剪贴板失败（{}，{e:?}），放弃", p.what.describe());
                sync(|s| s.forget());
                self.hub.send(Msg::ClipForget);
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
            sys::autostart_set(want);
            let actual = sys::autostart_get().is_some();
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
                self.check_rename_peer();
                // status.json 上次没写成就重试（内容没变时是空操作）
                let st = self.hub.status();
                self.write_status(&st);
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
                let detail = island::preview(&text);
                // 先让 net 丢掉还没发完的旧图片，再发新内容
                self.hub.to_net(Out::CancelClip);
                self.hub.send(Msg::ClipText { text });
                self.note(&Note { who: c.name.clone(), kind: Kind::Text, detail, ok: true });
            }
            clip::Content::Text(text) => {
                let generation = sync(|s| s.local());
                self.drop_pending();
                submit(&self.hub, Job { generation, format: clip::CF_UNICODETEXT, data: Payload::Text(text) });
            }
            clip::Content::Image { format, data } => {
                log!("本机复制了图片（格式 {format}，{} KB）", data.len() / 1024);
                let generation = sync(|s| s.local());
                self.drop_pending();
                submit(&self.hub, Job { generation, format, data: Payload::Image(data) });
            }
            clip::Content::TooBig { image, size } => {
                if every(&mut self.skip_log, Instant::now(), SKIP_LOG_EVERY) {
                    log!("剪贴板{}太大（约 {} KB），不同步", if image { "图片" } else { "文字" }, size / 1024);
                }
                self.local_other();
                let (kind, limit) = if image {
                    (Kind::Image, format!("{} MB", c.clipboard.max_image_mb))
                } else {
                    (Kind::Text, limit_text(c.clipboard.max_text_kb))
                };
                self.note_unsynced(kind, format!("超过 {limit}"));
            }
            clip::Content::Concealed => {
                log!("本机剪贴板是敏感内容（密码等），不同步");
                self.local_other();
                self.note_unsynced(Kind::Secret, "敏感内容".into());
            }
            clip::Content::Other => self.local_other(),
        }
    }

    fn display_changed(&mut self) {
        let (d, dpis) = sys::monitors_dpi();
        if d.is_empty() || (d == self.desk && dpis == self.dpis) {
            return;
        }
        if d != self.desk {
            log!("显示器变化：{:?}", d.monitors);
            self.desk = d.clone();
            self.hub.to_input(ToInput::LocalDesk(d.clone()));
        } else {
            log!("显示器缩放变化：{dpis:?}");
        }
        self.dpis = dpis.clone();
        let c = self.hub.cfg();
        self.hub.send(hello(&c, d, dpis));
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
            self.hub.send(hello(&c, self.desk.clone(), self.dpis.clone()));
        }
        if c.autostart != old.autostart {
            sys::autostart_set(c.autostart);
            self.items.autostart.set_checked(sys::autostart_get().is_some());
        }
    }
}

fn hello(c: &Config, d: Desk, dpis: Vec<u32>) -> Msg {
    Msg::Hello { ver: PROTO_VERSION, name: c.name.clone(), monitors: d.monitors, udp_port: c.port, id: crate::proto::instance_id(), dpis }
}

/// status.json 的内容（t = 写入时间）。
fn status_file(st: &Status, t: u64) -> StatusFile {
    StatusFile {
        pid: std::process::id(),
        connected: st.connected,
        peer_name: st.peer_name.clone(),
        peer_addr: st.peer_addr.clone(),
        paused: st.paused,
        t,
    }
}

/// 正常退出时（主线程）：status.json 写成没连着。
pub fn save_exit_status(hub: &Hub) {
    let st = Status { connected: false, ..hub.status() };
    if let Err(e) = config::save_status(&status_file(&st, config::now_ms())) {
        log!("写 status.json 失败：{e}");
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

    let (desk, dpis) = sys::monitors_dpi();
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
        desk,
        dpis,
        status_file: None,
        status_failed: false,
        shown: Some((st.connected, tip_text(&st))),
        connected: st.connected,
    };
    STATE.with(|s| *s.borrow_mut() = Some(state));

    unsafe {
        SetTimer(Some(hwnd), TIMER_CONFIG, 1000, None);
    }
    hub.set_desktop_tid(tid);
    hub.set_desktop_hwnd(hwnd.0 as isize);
    // 登记前可能已有 Status 等消息：先刷新一次（也写启动时的 status.json）
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
    fn status_file_fields() {
        let st = Status { connected: true, peer_name: "客厅".into(), peer_addr: "10.0.0.2:47900".into(), paused: true, driving: true, ..Default::default() };
        let f = status_file(&st, 9);
        assert_eq!((f.pid, f.connected, f.paused, f.t), (std::process::id(), true, true, 9));
        assert_eq!((f.peer_name.as_str(), f.peer_addr.as_str()), ("客厅", "10.0.0.2:47900"));
        // 只是驱动状态变了：内容不变，不重写
        assert_eq!(status_file(&Status { driving: false, ..st.clone() }, 0), status_file(&st, 0));
    }

    #[test]
    fn aborted_send_is_not_synced() {
        // 同一张图连着两次通知：第一次开始发、被第二次顶掉（没送到）→ 撤销 → 第二次照常发
        let mut s = ClipSync::new();
        let g1 = s.local();
        assert!(s.offer(g1, 7));
        let g2 = s.local();
        assert!(!s.current(g1));
        s.unoffer(7);
        assert!(s.offer(g2, 7));
        // 已经换成别的内容时撤销旧摘要不影响
        let g3 = s.local();
        assert!(s.offer(g3, 8));
        s.unoffer(7);
        assert_eq!(s.synced, Some(8));
    }

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
        // 对方图片写入后记下（net 收的时候算好的）摘要；本机程序把同一图片再写一遍（代次变了）：回声，不发
        let v = s.incoming();
        let g = s.local();
        s.written(v, img);
        assert!(!s.offer(g, img));
        // 收到对方内容到写入（剪贴板被占要重试）之间本机已复制了新文字：迟到的写入不能覆盖
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

    fn blob(format: u32, len: usize, text_len: usize) -> clip::Blob {
        clip::Blob { format, mem: clip::Global::alloc(len).unwrap(), hash: 1, text_len }
    }

    #[test]
    fn forget_after_latest_clip() {
        // 先文字后 ClipForget：写入之后还要清
        let (rest, clip, after) = latest_clip(vec![ToDesktop::ClipText("x".into()), ToDesktop::ClipForget].into_iter());
        assert!(rest.is_empty() && clip.is_some() && after);
        // ClipForget 在前：照常先处理，写入后不清
        let (rest, clip, after) = latest_clip(vec![ToDesktop::ClipForget, ToDesktop::ClipText("x".into())].into_iter());
        assert!(matches!(rest[..], [ToDesktop::ClipForget]) && clip.is_some() && !after);
        // 文字、ClipForget、更新的文字：以最新那条为准，不清
        let (_, clip, after) =
            latest_clip(vec![ToDesktop::ClipText("a".into()), ToDesktop::ClipForget, ToDesktop::ClipText("b".into())].into_iter());
        assert!(matches!(clip, Some(ToDesktop::ClipText(ref t)) if t == "b") && !after);
    }

    #[test]
    fn keeps_only_latest_clip() {
        let msgs = vec![
            ToDesktop::ClipText("old".into()),
            ToDesktop::Status,
            ToDesktop::ClipData(blob(8, 1000, 0)),
            ToDesktop::HideCursor,
            ToDesktop::ClipText("new".into()),
            ToDesktop::Quit,
        ];
        let (rest, clip, _) = latest_clip(msgs.into_iter());
        assert!(matches!(clip, Some(ToDesktop::ClipText(ref t)) if t == "new"));
        assert_eq!(rest.len(), 3);
        assert!(matches!(rest[0], ToDesktop::Status));
        assert!(matches!(rest[1], ToDesktop::HideCursor));
        assert!(matches!(rest[2], ToDesktop::Quit));
        // 最新的是图片
        let (rest, clip, _) =
            latest_clip(vec![ToDesktop::ClipText("a".into()), ToDesktop::ClipData(blob(17, 10, 0))].into_iter());
        assert!(rest.is_empty());
        assert!(matches!(clip, Some(ToDesktop::ClipData(ref b)) if b.format == 17));
        let (rest, clip, _) = latest_clip(vec![ToDesktop::Status].into_iter());
        assert_eq!(rest.len(), 1);
        assert!(clip.is_none());
    }

    #[test]
    fn incoming_rules() {
        let mut c = config::ClipCfg { enabled: true, images: true, max_text_kb: 1, max_image_mb: 1, notify: true };
        let ok = |m: ToDesktop, c: &config::ClipCfg, paused: bool| accept_incoming(m, c, paused).is_some();
        assert!(ok(ToDesktop::ClipText("a".into()), &c, false));
        assert!(!ok(ToDesktop::ClipText(String::new()), &c, false));
        assert!(!ok(ToDesktop::ClipText("a".repeat(1025)), &c, false));
        assert!(!ok(ToDesktop::ClipText("a".into()), &c, true)); // 暂停
        assert!(ok(ToDesktop::ClipData(blob(13, 10, 1024)), &c, false));
        assert!(!ok(ToDesktop::ClipData(blob(13, 10, 1025)), &c, false)); // UTF-8 超限
        assert!(!ok(ToDesktop::ClipData(blob(13, 10, 0)), &c, false)); // 空文字
        assert!(ok(ToDesktop::ClipData(blob(8, 10, 0)), &c, false));
        assert!(ok(ToDesktop::ClipData(blob(17, 10, 0)), &c, false));
        assert!(!ok(ToDesktop::ClipData(blob(2, 10, 0)), &c, false)); // 不认识的格式
        c.images = false;
        assert!(!ok(ToDesktop::ClipData(blob(17, 10, 0)), &c, false));
        assert!(ok(ToDesktop::ClipData(blob(13, 10, 5)), &c, false));
        c.enabled = false;
        assert!(!ok(ToDesktop::ClipData(blob(13, 10, 5)), &c, false));
        assert!(!ok(ToDesktop::ClipText("a".into()), &c, false));
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

    /// 起一个压缩线程把 data 流进管道；返回 (接收端, 线程)。线程结果 = stream_out 是否送完。
    fn produce(
        data: Payload,
        limit: usize,
        live: impl Fn() -> bool + Send + 'static,
    ) -> (hub::StreamRx, std::thread::JoinHandle<bool>) {
        let (tx, rx) = hub::clip_stream(limit);
        let t = std::thread::spawn(move || {
            let ok = stream_out(&data, &tx, &live);
            if ok {
                tx.finish();
            }
            ok
        });
        (rx, t)
    }

    /// 从管道取完并解压。
    fn consume(rx: &hub::StreamRx, raw_len: usize) -> Vec<u8> {
        let mut out = vec![0u8; raw_len];
        let mut z = proto::Inflater::new();
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < end, "超时");
            assert!(rx.queued() <= STREAM_LIMIT);
            match rx.take() {
                hub::Piece::Data(p) => {
                    assert!(!p.is_empty() && p.len() <= proto::CHUNK);
                    z.feed(&p, &mut out).unwrap();
                }
                hub::Piece::End => break,
                hub::Piece::Gone => panic!("压缩线程放弃了"),
                hub::Piece::Wait => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        assert!(z.done() && z.written() == raw_len);
        out
    }

    #[test]
    fn stream_out_roundtrip() {
        let mut noise = vec![0u8; 3 * 1024 * 1024 + 5];
        getrandom::fill(&mut noise).unwrap();
        let mixed: Vec<u8> = (0..INPUT_BLOCK * 7 / 2).map(|i| ((i / 7) ^ (i >> 12)) as u8).collect();
        for data in [noise, mixed, vec![1u8], vec![0u8; INPUT_BLOCK], Vec::new()] {
            let len = data.len();
            let want = data.clone();
            let (rx, t) = produce(Payload::Image(data), STREAM_LIMIT, || true);
            let got = consume(&rx, len);
            assert!(t.join().unwrap());
            assert!(got == want);
        }
        // 大段文字：边编码 UTF-16 边压缩，结果与整段编码一致（含恰好整块、代理对）
        for t in ["大段文字 😀 abc\n".repeat(30_000), "a".repeat(INPUT_BLOCK / 2), String::new()] {
            let want = clip::utf16le(&t);
            let (rx, th) = produce(Payload::Text(t.clone()), STREAM_LIMIT, || true);
            assert_eq!(Payload::Text(t).raw_len(), want.len());
            let got = consume(&rx, want.len());
            assert!(th.join().unwrap());
            assert!(got == want);
        }
    }

    #[test]
    fn stream_backpressure_and_cancel() {
        let mut noise = vec![0u8; 4 * 1024 * 1024];
        getrandom::fill(&mut noise).unwrap();
        // 没人取：压缩线程填满上限后等着，排队的不超过上限
        let limit = 256 * 1024;
        let (tx, rx) = hub::clip_stream(limit);
        let data = Payload::Image(noise.clone());
        let t = std::thread::spawn(move || stream_out(&data, &tx, &|| true));
        std::thread::sleep(Duration::from_millis(500));
        assert!(!t.is_finished());
        let queued = rx.queued();
        assert!(queued > limit - proto::CHUNK && queued <= limit, "排队 {queued}");
        // net 不要了（取消 / 断开）：压缩线程立刻放弃，不死等
        let t0 = Instant::now();
        drop(rx);
        assert!(!t.join().unwrap());
        assert!(t0.elapsed() < Duration::from_secs(2));

        // 等待期间内容过时：放弃
        let stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s2 = stale.clone();
        let (rx, t) =
            produce(Payload::Image(noise.clone()), limit, move || !s2.load(std::sync::atomic::Ordering::Relaxed));
        std::thread::sleep(Duration::from_millis(300));
        let t0 = Instant::now();
        stale.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(!t.join().unwrap());
        assert!(t0.elapsed() < Duration::from_secs(2));
        // 压缩线程放弃：接收端看到 Gone（net 据此告诉对方作废）
        let mut last = rx.take();
        while matches!(last, hub::Piece::Data(_)) {
            last = rx.take();
        }
        assert_eq!(last, hub::Piece::Gone);

        // 一开始就过时：不发任何东西
        let (rx, t) = produce(Payload::Image(noise), limit, || false);
        assert!(!t.join().unwrap());
        assert_eq!(rx.take(), hub::Piece::Gone);
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
}

/// 文字上限的说法：整 MB 用 MB，否则 KB。
fn limit_text(kb: u32) -> String {
    if kb >= 1024 && kb % 1024 == 0 { format!("{} MB", kb / 1024) } else { format!("{kb} KB") }
}

fn describe_format(format: u32) -> &'static str {
    if format == clip::CF_UNICODETEXT { "大段文字" } else { "图片" }
}
