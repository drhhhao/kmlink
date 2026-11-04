//! 线程之间的连接点。三个常驻线程：
//!   - input（主线程）：低级键鼠钩子 + 控制端状态机（跨屏、虚拟光标）。回调里只做判断、发消息，不阻塞。
//!   - desktop：隐藏窗口 + 消息循环：剪贴板监听/读写、托盘、显示器变化、配置文件变化、隐藏/恢复系统光标。
//!   - net：会话（TCP 可靠通道 + UDP 低延迟通道），以及被控端的注入（`inject::Injector`，可在任意线程调用）。
//! 发给 input/desktop 的消息走 channel，再 PostThreadMessage(WM_APP_WAKE) 叫醒对方的消息循环。

use crate::config::Config;
use crate::geom::{Desk, Layout, Point};
use crate::proto::Msg;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, RwLock};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};

/// 叫醒线程消息循环：收到后去 drain 自己的 channel。
pub const WM_APP_WAKE: u32 = WM_APP + 1;

/// 发给 input 线程（控制端状态机）。
#[derive(Debug)]
pub enum ToInput {
    /// 会话建立/对方显示器变化。
    PeerUp { name: String, desk: Desk },
    /// 会话断开：若正在驱动对方，立刻回到本机。
    PeerDown,
    /// 本机显示器变化（desktop 线程检测到 WM_DISPLAYCHANGE 后发）。
    LocalDesk(Desk),
    /// 排布或输入设置变化（配置重新加载、对方同步来的排布）。
    Config(Box<Config>),
    /// 对方请求放手（对方本地有人动了键鼠）。
    Yield,
    /// 本机正在被对方控制（true）/ 控制结束（false）。被控期间本机的物理输入会发 Yield。
    Driven(bool),
    /// 托盘“暂停共享”。
    Pause(bool),
    Quit,
}

/// 发给 desktop 线程。
#[derive(Debug)]
pub enum ToDesktop {
    /// 对方发来的剪贴板文字。
    ClipText(String),
    /// 对方发来的剪贴板图片（已解压）：format 为 CF_DIB(8) 或 CF_DIBV5(17)。
    ClipImage { format: u32, data: Vec<u8> },
    /// 隐藏本机系统光标（开始驱动对方）/ 恢复（回到本机）。
    HideCursor,
    ShowCursor,
    /// 连接状态变化（刷新托盘提示）。
    Status,
    Quit,
}

/// 发给 net 线程（发往对方）。
#[derive(Debug)]
pub enum Out {
    /// 可靠通道（优先发送）。
    Msg(Msg),
    /// UDP：被控端光标绝对位置。net 线程只发最新的一个。
    Move { epoch: u32, seq: u32, x: i32, y: i32 },
    /// 剪贴板图片：deflate 后的数据（在可靠通道上分片、低优先级发送）。
    ClipImage { format: u32, raw_len: u32, deflated: Vec<u8> },
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
            net_tx,
        };
        (hub, HubRx { input, desktop, net })
    }

    pub fn cfg(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    pub fn layout(&self) -> Layout {
        self.cfg.read().unwrap().layout.layout
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

    pub fn to_input(&self, m: ToInput) {
        let _ = self.input_tx.send(m);
        wake(self.input_tid.load(Ordering::SeqCst));
    }
    pub fn to_desktop(&self, m: ToDesktop) {
        let _ = self.desktop_tx.send(m);
        wake(self.desktop_tid.load(Ordering::SeqCst));
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

/// 便于日志里打印点。
pub fn pt(p: Point) -> String {
    format!("({}, {})", p.x, p.y)
}
