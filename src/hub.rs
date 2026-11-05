//! 线程之间的连接点。三个常驻线程：
//!   - input（主线程）：低级键鼠钩子 + 控制端状态机（跨屏、虚拟光标）。回调里只做判断、发消息，不阻塞。
//!   - desktop：隐藏窗口 + 消息循环：剪贴板监听/读写、托盘、显示器变化、配置文件变化、隐藏/恢复系统光标。
//!   - net：会话（TCP 可靠通道 + UDP 低延迟通道），以及被控端的注入（`inject::Injector`，可在任意线程调用）。
//! 发给 input/desktop 的消息走 channel，再 PostThreadMessage(WM_APP_WAKE) 叫醒对方的消息循环。
//!
//! 剪贴板图片 / 大段文字的发送是一条“压缩流”（`clip_stream`）：desktop 的压缩工作线程边压缩边 `StreamTx::send`，
//! net-tcp 从 `StreamRx` 取分片发出。有界：还没发出的压缩数据超过上限时压缩线程等着（net 发走一片就叫醒它）；
//! net 不要了（CancelClip、新内容顶掉、会话断开 —— StreamRx 被丢弃）、内容过时或 30 秒没进展时压缩线程立刻放弃，不会死等。

use crate::config::Config;
use crate::geom::Desk;
use crate::proto::Msg;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};

/// 叫醒线程消息循环：收到后去 drain 自己的 channel。
pub const WM_APP_WAKE: u32 = WM_APP + 1;

/// 发给 input 线程（控制端状态机）。
#[derive(Debug)]
pub enum ToInput {
    /// 会话建立/对方显示器变化。
    PeerUp {
        #[allow(dead_code)]
        name: String,
        desk: Desk,
        /// 对方 Hello 里的能力位（proto::CAP_*）
        caps: u32,
    },
    /// 会话断开：若正在驱动对方，立刻回到本机。
    PeerDown,
    /// 本机显示器变化（desktop 线程检测到 WM_DISPLAYCHANGE 后发）。
    LocalDesk(Desk),
    /// 排布或输入设置变化（配置重新加载、对方同步来的排布）。
    Config(Box<Config>),
    /// 对方请求放手（对方本地有人动了键鼠）。参数 = 对方报告的接管轮次（0 = 不确定）。
    Yield(u32),
    /// 对方的鼠标真的在用（键盘跟随）。参数 = 对方被本机接管的轮次（0 = 对方没被接管）。
    PeerActive(u32),
    /// 对方的键盘开始（true）/ 结束（false）注入本机（injector 在持锁时通知）。
    KbdIn(bool),
    /// 对方 1.5 秒没有任何帧：只转发键盘时立刻收回（驱动中照旧等 GoHome）。
    PeerStale,
    /// 立刻回到本机（系统即将睡眠/注销等）。不在驱动时什么都不做。
    GoHome,
    /// 本机正在被对方控制（true）/ 控制结束（false）。被控期间本机的物理输入会发 Yield。
    Driven(bool),
    /// 托盘“暂停共享”。
    Pause(bool),
    Quit,
}

/// 发给 desktop 线程。
#[derive(Debug)]
#[allow(dead_code)] // HideCursor/ShowCursor/Quit：现由 cursor 工作线程 / 托盘直接处理，保留以兼容
pub enum ToDesktop {
    /// 对方发来的剪贴板文字。
    ClipText(String),
    /// 对方剪贴板已和本机不一致：清掉“已同步”记录。
    ClipForget,
    /// 对方发来的图片（CF_DIB / CF_DIBV5，已校验）或大段文字（CF_UNICODETEXT）：已解压进 GlobalAlloc 的内存，
    /// 可以直接交给剪贴板；摘要在接收时边解压边算好。
    ClipData(crate::clip::Blob),
    /// 隐藏本机系统光标（开始驱动对方）/ 恢复（回到本机）。
    HideCursor,
    ShowCursor,
    /// 连接状态变化（刷新托盘提示）。
    Status,
    /// 对方发来 ClipForget（对方没接收本机的内容，或对方剪贴板变了）：清“已同步”记录；
    /// 本机正显示着自己的“已同步”就改成“未同步”。
    PeerForget,
    /// 弹剪贴板同步提示（灵动岛）。
    Note(crate::island::Note),
    /// 键盘跟随提示：true = 键盘刚换到本机，不等打字（对方键盘新打进来：injector 在 kbd_enter 时发；
    /// 本机鼠标在用/热键把本机键盘收回：input 的 KbdHere 发）；false = 本机键盘被意外收回（断线、管理员窗口等）。
    KbdNote(bool),
    /// 对方在它的设置窗口里给本机改了名（未清理）：写进本机 config.toml。
    Rename(String),
    /// 收起正显示的“键盘 <本机名>”提示（用户到对方那台干活了：对方弹了提示 / 本机鼠标跨过去了）。
    KbdTipGone,
    Quit,
}

/// 发给 net 线程（发往对方）。
#[derive(Debug)]
pub enum Out {
    /// 可靠通道（优先发送）。
    Msg(Msg),
    /// UDP：被控端光标绝对位置。net 线程只发最新的一个。
    Move { epoch: u32, seq: u32, x: i32, y: i32 },
    /// 剪贴板图片 / 大段文字（format 13）开始发送：raw_len = 原始长度，压缩分片随后从 data 流出
    /// （在可靠通道上分片、低优先级发送）。
    ClipStream { format: u32, raw_len: u32, data: StreamRx },
    /// 剪贴板已被更新的内容取代（本机复制了新东西，或刚写入了对方发来的内容）：
    /// 丢掉还没发完的图片，也丢掉正在接收的对方图片。
    CancelClip,
    /// 只丢掉本机待发的图片（刚写入了对方的内容）：正在接收的对方内容一定比刚写入的新，不能丢。
    CancelClipSend,
}

// ---------- 剪贴板压缩流（压缩线程 → net-tcp，有界） ----------

/// 压缩线程等 net 发走数据时，每隔这么久看一次内容是否已过时。
const STREAM_POLL: Duration = Duration::from_millis(100);
/// 这么久一片都没发出去：放弃（正常情况下会话早已因写超时断开）。
const STREAM_STALL: Duration = Duration::from_secs(30);

/// 叫醒 net-tcp（有新分片 / 流结束了）。
pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct PipeState {
    q: VecDeque<Vec<u8>>,
    /// q 里的字节数。
    bytes: usize,
    /// 压缩线程已送完全部分片。
    done: bool,
    /// 压缩线程没送完就放弃了。
    tx_gone: bool,
    /// net 不要了（StreamRx 已丢弃）。
    rx_gone: bool,
    waker: Option<Waker>,
}

struct Pipe {
    limit: usize,
    st: Mutex<PipeState>,
    /// 压缩线程在这里等 net 取走分片。
    cv: Condvar,
}

impl Pipe {
    fn lock(&self) -> MutexGuard<'_, PipeState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 一条剪贴板压缩流：limit = 还没发出的压缩数据上限（字节）。
pub fn clip_stream(limit: usize) -> (StreamTx, StreamRx) {
    let p = Arc::new(Pipe { limit, st: Mutex::new(PipeState::default()), cv: Condvar::new() });
    (StreamTx(p.clone()), StreamRx(p))
}

/// 压缩线程这一端。没 `finish` 就丢弃 = 放弃（net 随后发 Abort）。
pub struct StreamTx(Arc<Pipe>);

/// net 这一端。丢弃 = 不要了：压缩线程的 `send` 立刻返回 false。
pub struct StreamRx(Arc<Pipe>);

/// `StreamRx::take` 的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    Data(Vec<u8>),
    /// 全部送完。
    End,
    /// 压缩线程中途放弃了。
    Gone,
    /// 暂时没有。
    Wait,
}

impl StreamTx {
    /// 交出一片压缩数据。还没发出的超过上限时等 net 取走；等待期间 net 不要了、live() 为假（内容已过时）、
    /// 或 30 秒没有进展：返回 false（调用方放弃）。叫醒 net 时不持有本流的锁。
    pub fn send(&self, piece: Vec<u8>, live: &dyn Fn() -> bool) -> bool {
        let len = piece.len();
        let mut moved = Instant::now();
        let mut g = self.0.lock();
        loop {
            if g.rx_gone {
                return false;
            }
            if g.bytes == 0 || g.bytes + len <= self.0.limit {
                break;
            }
            let before = g.bytes;
            g = self.0.cv.wait_timeout(g, STREAM_POLL).unwrap_or_else(|e| e.into_inner()).0;
            if g.bytes < before {
                moved = Instant::now();
            }
            if g.rx_gone || moved.elapsed() >= STREAM_STALL {
                return false;
            }
            // 问 live 时不持有本流的锁（它会去拿别的锁）
            drop(g);
            if !live() {
                return false;
            }
            g = self.0.lock();
        }
        g.q.push_back(piece);
        g.bytes += len;
        let w = g.waker.clone();
        drop(g);
        if let Some(w) = w {
            w();
        }
        true
    }

    /// 全部送完。
    pub fn finish(self) {
        self.0.lock().done = true;
        // Drop 里叫醒 net
    }
}

impl Drop for StreamTx {
    fn drop(&mut self) {
        let w = {
            let mut g = self.0.lock();
            if !g.done {
                g.tx_gone = true;
            }
            g.waker.clone()
        };
        if let Some(w) = w {
            w();
        }
    }
}

impl StreamRx {
    /// 登记叫醒 net-tcp 的办法（net-out 收下这条流时）。
    pub fn set_waker(&self, w: Waker) {
        self.0.lock().waker = Some(w);
    }

    /// 有东西可取（分片、结束或放弃）。
    pub fn ready(&self) -> bool {
        let g = self.0.lock();
        !g.q.is_empty() || g.done || g.tx_gone
    }

    /// 排队中（还没取走）的压缩字节数。
    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.0.lock().bytes
    }

    /// 取一片；取走后叫醒可能在等的压缩线程。
    pub fn take(&self) -> Piece {
        let mut g = self.0.lock();
        if let Some(v) = g.q.pop_front() {
            g.bytes -= v.len();
            drop(g);
            self.0.cv.notify_all();
            return Piece::Data(v);
        }
        if g.done {
            Piece::End
        } else if g.tx_gone {
            Piece::Gone
        } else {
            Piece::Wait
        }
    }
}

impl Drop for StreamRx {
    fn drop(&mut self) {
        {
            let mut g = self.0.lock();
            g.rx_gone = true;
            g.q = VecDeque::new();
            g.bytes = 0;
            g.waker = None;
        }
        self.0.cv.notify_all();
    }
}

impl std::fmt::Debug for StreamRx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.0.lock();
        write!(f, "StreamRx {{ queued: {}, done: {}, gone: {} }}", g.bytes, g.done, g.tx_gone)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub connected: bool,
    pub peer_name: String,
    pub peer_addr: String,
    /// 正在控制对方 / 被对方控制
    pub driving: bool,
    pub driven: bool,
    pub paused: bool,
    /// 本机键盘正打到对方（键盘跟随，只转发键盘）
    pub kbd_out: bool,
    pub last_error: String,
}

pub struct Hub {
    pub cfg: RwLock<Config>,
    pub cfg_path: PathBuf,
    pub status: Mutex<Status>,
    input_tx: Sender<ToInput>,
    input_tid: AtomicU32,
    desktop_tx: Sender<ToDesktop>,
    desktop_tid: AtomicU32,
    /// desktop 线程的隐藏窗口：有了它就用 PostMessage 叫醒（托盘菜单等模态循环里线程消息会丢）。
    desktop_hwnd: std::sync::atomic::AtomicIsize,
    net_tx: Sender<Out>,
}

pub struct HubRx {
    pub input: Receiver<ToInput>,
    pub desktop: Receiver<ToDesktop>,
    pub net: Receiver<Out>,
}

impl Hub {
    pub fn new(cfg: Config) -> (Hub, HubRx) {
        let (input_tx, input) = unbounded();
        let (desktop_tx, desktop) = unbounded();
        let (net_tx, net) = unbounded();
        let hub = Hub {
            cfg: RwLock::new(cfg),
            cfg_path: crate::config::path(),
            status: Mutex::new(Status::default()),
            input_tx,
            input_tid: AtomicU32::new(0),
            desktop_tx,
            desktop_tid: AtomicU32::new(0),
            desktop_hwnd: std::sync::atomic::AtomicIsize::new(0),
            net_tx,
        };
        (hub, HubRx { input, desktop, net })
    }

    pub fn cfg(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    /// 线程启动时登记自己的线程 id（GetCurrentThreadId），之后才能被叫醒。
    pub fn set_input_tid(&self, tid: u32) {
        self.input_tid.store(tid, Ordering::SeqCst);
        wake(tid);
    }
    pub fn set_desktop_tid(&self, tid: u32) {
        self.desktop_tid.store(tid, Ordering::SeqCst);
        wake(tid);
    }

    /// desktop 线程创建好隐藏窗口后登记（窗口过程要处理 WM_APP_WAKE）。
    pub fn set_desktop_hwnd(&self, hwnd: isize) {
        self.desktop_hwnd.store(hwnd, Ordering::SeqCst);
        wake_desktop(hwnd, self.desktop_tid.load(Ordering::SeqCst));
    }

    pub fn to_input(&self, m: ToInput) {
        let _ = self.input_tx.send(m);
        wake(self.input_tid.load(Ordering::SeqCst));
    }
    pub fn to_desktop(&self, m: ToDesktop) {
        let _ = self.desktop_tx.send(m);
        wake_desktop(self.desktop_hwnd.load(Ordering::SeqCst), self.desktop_tid.load(Ordering::SeqCst));
    }
    pub fn to_net(&self, m: Out) {
        let _ = self.net_tx.send(m);
    }
    pub fn send(&self, m: Msg) {
        self.to_net(Out::Msg(m));
    }

    pub fn set_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
        self.to_desktop(ToDesktop::Status);
    }
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}

fn wake(tid: u32) {
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_APP_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

fn wake_desktop(hwnd: isize, tid: u32) {
    if hwnd != 0 {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void)),
                WM_APP_WAKE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    } else {
        wake(tid);
    }
}


